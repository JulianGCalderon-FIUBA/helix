use std::collections::HashSet;

use helix_core::Transaction;
use iroh::SecretKey;

use super::*;

struct PeerTransport {
    id: EndpointId,
    in_session: bool,
    topics: HashSet<TopicId>,
}

impl PeerTransport {
    fn apply(&mut self, transmit: Transmit) {
        match transmit {
            Transmit::Subscribe(topic, _) => {
                self.topics.insert(topic);
            }
            Transmit::Unsubscribe(topic) => {
                self.topics.remove(&topic);
            }
            Transmit::Shutdown => {
                self.in_session = false;
                self.topics.clear();
            }
            Transmit::Send(..) => unreachable!("the network delivers sends"),
        }
    }

    fn listens(&self, channel: Channel) -> bool {
        match channel {
            Channel::Session => self.in_session,
            Channel::Document(topic) => self.topics.contains(&topic),
            Channel::Peer(id) => self.id == id,
        }
    }
}

struct Peer {
    session: Session,
    transport: PeerTransport,
    buffers: HashMap<SharedId, Rope>,
    unshared: Vec<(SharedId, String)>,
}

impl Peer {
    fn share(&mut self, text: &str) -> SharedId {
        let text = Rope::from(text);
        let id = self.session.share(None, &text);
        self.buffers.insert(id, text);
        id
    }

    fn open(&mut self, id: SharedId) {
        self.session.open(id).unwrap();
        self.buffers.insert(id, Rope::new());
    }

    fn insert(&mut self, id: SharedId, pos: usize, text: &str) {
        let buffer = self.buffers.get_mut(&id).unwrap();
        let transaction = Transaction::change(buffer, [(pos, pos, Some(text.into()))].into_iter());
        transaction.apply(buffer);
        self.session.local_change(id, transaction.changes());
    }

    fn text(&self, id: SharedId) -> String {
        self.buffers[&id].to_string()
    }

    fn handle_events(&mut self) {
        while let Some(event) = self.session.poll_event() {
            match event {
                Event::Changes { id, changes } => {
                    let buffer = self.buffers.get_mut(&id).unwrap();
                    Transaction::change(buffer, changes.into_iter()).apply(buffer);
                }
                Event::Unshared { id, reason } => self.unshared.push((id, reason)),
            }
        }
    }
}

#[derive(Default)]
struct Network {
    // A list for consistent delivery order
    peers: Vec<Peer>,
}

impl Network {
    fn join(&mut self) -> EndpointId {
        let id = SecretKey::generate().public();
        let mut session = Session::new(id, rand::random());
        session.join();
        self.peers.push(Peer {
            session,
            transport: PeerTransport {
                id,
                in_session: true,
                topics: HashSet::new(),
            },
            buffers: HashMap::new(),
            unshared: Vec::new(),
        });
        id
    }

    fn peer(&mut self, id: EndpointId) -> &mut Peer {
        self.peers
            .iter_mut()
            .find(|peer| peer.transport.id == id)
            .expect("peer should have joined")
    }

    fn flush(&mut self) {
        loop {
            let mut transmits = Vec::new();
            for peer in &mut self.peers {
                peer.handle_events();
                while let Some(transmit) = peer.session.poll_transmit() {
                    transmits.push((peer.transport.id, transmit));
                }
            }
            if transmits.is_empty() {
                return;
            }
            for (from, transmit) in transmits {
                self.deliver(from, transmit);
            }
        }
    }

    fn deliver(&mut self, from: EndpointId, transmit: Transmit) {
        let Transmit::Send(channel, message) = transmit else {
            self.peer(from).transport.apply(transmit);
            return;
        };
        let arrives_on = match channel {
            Channel::Peer(_) => Channel::Peer(from),
            channel => channel,
        };
        for peer in &mut self.peers {
            if peer.transport.id != from && peer.transport.listens(channel) {
                peer.session.receive(arrives_on, &message);
            }
        }
    }
}

#[test]
fn shared_documents_are_announced() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();

    let id = net.peer(owner).share("hello");
    net.flush();

    assert_eq!(net.peer(guest).session.documents()[&id].owner, owner);
}

#[test]
fn late_joiner_learns_shared_documents() {
    let mut net = Network::default();
    let owner = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();

    let guest = net.join();
    net.flush();

    assert!(net.peer(guest).session.documents().contains_key(&id));
}

#[test]
fn opening_fetches_the_owner_text() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();

    net.peer(guest).open(id);
    net.flush();

    assert_eq!(net.peer(guest).text(id), "hello");
}

#[test]
fn opening_an_unknown_document_fails() {
    let mut net = Network::default();
    let guest = net.join();
    let id = SharedId::from(TopicId::from_bytes([0; 32]));
    assert!(net.peer(guest).session.open(id).is_err());
}

#[test]
fn edits_reach_every_peer() {
    let mut net = Network::default();
    let owner = net.join();
    let guests = [net.join(), net.join()];
    let id = net.peer(owner).share("hello");
    net.flush();
    for guest in guests {
        net.peer(guest).open(id);
    }
    net.flush();

    net.peer(owner).insert(id, 5, " world");
    net.flush();
    net.peer(guests[0]).insert(id, 0, ">");
    net.flush();

    for peer in &net.peers {
        assert_eq!(peer.text(id), ">hello world");
    }
}

#[test]
fn concurrent_edits_converge() {
    let mut net = Network::default();
    let owner = net.join();
    let guests = [net.join(), net.join()];
    let id = net.peer(owner).share("abc");
    net.flush();
    for guest in guests {
        net.peer(guest).open(id);
    }
    net.flush();

    net.peer(owner).insert(id, 1, "X");
    net.peer(guests[0]).insert(id, 1, "Y");
    net.peer(guests[1]).insert(id, 1, "Z");
    net.flush();

    let text = net.peer(owner).text(id);
    assert_eq!(text.len(), 6);
    for peer in &net.peers {
        assert_eq!(peer.text(id), text);
    }
}

#[test]
fn edits_while_opening_are_not_lost() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();

    // The owner edits before the guest subscribes,
    // so only the snapshot can carry the edit.
    net.peer(guest).open(id);
    net.peer(owner).insert(id, 5, "!");
    net.flush();

    assert_eq!(net.peer(guest).text(id), "hello!");
}

#[test]
fn closing_an_owned_document_unshares_it() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();
    net.peer(guest).open(id);
    net.flush();

    net.peer(owner).session.close(id);
    net.flush();

    assert!(net.peer(owner).session.documents().is_empty());
    assert!(net.peer(guest).session.documents().is_empty());
    assert!(net.peer(owner).unshared.is_empty());
    assert_eq!(net.peer(guest).unshared, [(id, "owner unshared".into())]);
}

#[test]
fn closing_an_opened_document_keeps_it_shared() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();
    net.peer(guest).open(id);
    net.flush();

    net.peer(guest).session.close(id);
    net.peer(owner).insert(id, 0, ">");
    net.flush();

    assert!(net.peer(guest).session.documents().contains_key(&id));
    assert_eq!(net.peer(guest).text(id), "hello");
    assert!(net.peer(owner).unshared.is_empty());
}

#[test]
fn unsharing_while_opening_reports_once() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();

    // The guest is notified twice that the document no longer exists,
    // but it reports it only once.
    net.peer(owner).session.close(id);
    net.peer(guest).open(id);
    net.flush();

    assert_eq!(net.peer(guest).unshared, [(id, "owner unshared".into())]);
}

#[test]
fn losing_the_owner_while_opening_unshares() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();

    net.peer(guest).open(id);
    net.peer(guest).session.transport_exit(Channel::Peer(owner));
    net.flush();

    assert_eq!(
        net.peer(guest).unshared,
        [(id, "lost connection to owner".into())]
    );
}

#[test]
fn losing_the_owner_after_opening_is_harmless() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();

    net.peer(guest).open(id);
    net.flush();
    net.peer(guest).session.transport_exit(Channel::Peer(owner));
    net.flush();

    assert!(net.peer(guest).unshared.is_empty());
}

#[test]
fn losing_a_document_swarm_unshares_it() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();
    net.peer(guest).open(id);
    net.flush();

    net.peer(guest)
        .session
        .transport_exit(Channel::Document(id.topic()));
    net.flush();

    assert_eq!(net.peer(guest).unshared, [(id, "lost connection".into())]);
}

#[test]
fn losing_the_session_unshares_everything() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let owned = net.peer(guest).share("mine");
    let opened = net.peer(owner).share("theirs");
    net.flush();
    net.peer(guest).open(opened);
    net.flush();

    net.peer(guest).session.transport_exit(Channel::Session);
    net.flush();

    let unshared: HashSet<_> = net.peer(guest).unshared.iter().map(|(id, _)| *id).collect();
    assert_eq!(unshared, HashSet::from([owned, opened]));
    assert!(net.peer(guest).session.documents().is_empty());
}

#[test]
fn shutdown_unshares_owned_documents() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();
    net.peer(guest).open(id);
    net.flush();

    net.peer(owner).session.shutdown();
    net.flush();

    assert_eq!(net.peer(owner).unshared, [(id, "closed session".into())]);
    assert_eq!(net.peer(guest).unshared, [(id, "owner unshared".into())]);
    assert!(net.peer(guest).session.documents().is_empty());
}

#[test]
fn malformed_messages_are_dropped() {
    let mut net = Network::default();
    let owner = net.join();
    let guest = net.join();
    let id = net.peer(owner).share("hello");
    net.flush();
    net.peer(guest).open(id);
    net.flush();

    for channel in [
        Channel::Session,
        Channel::Document(id.topic()),
        Channel::Peer(owner),
    ] {
        net.peer(guest).session.receive(channel, b"garbage");
    }
    net.flush();

    assert_eq!(net.peer(guest).text(id), "hello");
    assert!(net.peer(guest).unshared.is_empty());
}
