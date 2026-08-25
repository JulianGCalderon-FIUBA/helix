//! Membership of a collaboration session.
//!
//! A session is a full mesh: every peer holds one connection to every other
//! peer. A peer joins by dialing any member with a ticket, and is answered
//! with the addresses of the rest of the session, which it then dials itself.
//! The member it dialed tells the others about it, so that a peer invited by
//! B also ends up connected to A and C.

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use anyhow::{bail, ensure, Result};
use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
    Endpoint, EndpointAddr, EndpointId,
};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::{
    proto::{self, Message},
    Event, ALPN,
};

/// Close code for leaving a session on purpose.
const BYE: u32 = 0;
/// Close code for the losing half of a simultaneous dial.
const DUPLICATE: u32 = 1;

/// A peer of the session, and the stream we talk to it over.
#[derive(Debug)]
struct Peer {
    /// Identifies the connection this peer was entered with, so that a
    /// connection dropped after being replaced does not evict its successor.
    token: u64,
    addr: EndpointAddr,
    connection: Connection,
    /// Messages queued for the writer of this connection.
    outbox: UnboundedSender<Message>,
}

/// The peers we are currently in a session with.
#[derive(Debug, Clone)]
pub struct Session {
    endpoint: Endpoint,
    events: UnboundedSender<Event>,
    peers: Arc<Mutex<HashMap<EndpointId, Peer>>>,
    /// Peers with a dial in flight, so that two introductions arriving at
    /// once do not leave us with two connections to the same peer.
    dialing: Arc<Mutex<HashSet<EndpointId>>>,
    tokens: Arc<AtomicU64>,
}

impl Session {
    pub fn new(endpoint: Endpoint, events: UnboundedSender<Event>) -> Self {
        Self {
            endpoint,
            events,
            peers: Arc::default(),
            dialing: Arc::default(),
            tokens: Arc::default(),
        }
    }

    pub fn events(&self) -> &UnboundedSender<Event> {
        &self.events
    }

    fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    fn emit(&self, event: Event) {
        // The editor is gone once it stops draining events, which is not
        // something the session can do anything about.
        let _ = self.events.send(event);
    }

    fn report(&self, error: String) {
        self.emit(Event::Error(error));
    }

    /// Mints a ticket for this session.
    ///
    /// Every member can invite, so the ticket is simply our own address: the
    /// peer that redeems it reaches the whole session through us.
    pub fn ticket_new(&self) -> String {
        EndpointTicket::new(self.endpoint.addr()).to_string()
    }

    /// Joins the session advertised by a ticket.
    pub fn ticket_join(&self, ticket: &str) -> Result<()> {
        let ticket = EndpointTicket::from_str(ticket)?;
        ensure!(
            ticket.endpoint_addr().id != self.id(),
            "cannot join our own session"
        );

        self.connect(ticket.endpoint_addr().clone());

        Ok(())
    }

    /// Sends an opaque payload to every peer of the session.
    ///
    /// What the bytes mean is up to the layer above; this one only carries
    /// them, in order, to everyone currently in the session.
    pub fn broadcast(&self, data: Vec<u8>) -> Result<()> {
        let peers = self.peers.lock().unwrap();
        ensure!(!peers.is_empty(), "not in a session");

        for peer in peers.values() {
            let _ = peer.outbox.send(Message::Data(data.clone()));
        }

        Ok(())
    }

    /// Reports the peers currently in the session.
    pub fn ticket_peers(&self) -> Result<()> {
        let mut peers: Vec<_> = self.peers.lock().unwrap().keys().copied().collect();
        ensure!(!peers.is_empty(), "not in a session");
        peers.sort();

        self.emit(Event::Peers(peers));

        Ok(())
    }

    /// Leaves the session, dropping every peer.
    pub fn ticket_close(&self) -> Result<()> {
        let peers = std::mem::take(&mut *self.peers.lock().unwrap());
        ensure!(!peers.is_empty(), "not in a session");

        for (id, peer) in peers {
            peer.connection.close(BYE.into(), b"bye");
            self.emit(Event::Disconnected(id));
        }

        Ok(())
    }

    /// Dials a peer, unless it is us or one we are already talking to.
    fn connect(&self, addr: EndpointAddr) {
        let id = addr.id;
        if id == self.id() || !self.dialing.lock().unwrap().insert(id) {
            return;
        }

        let session = self.clone();
        tokio::spawn(async move {
            let result = session.dial(addr).await;
            session.dialing.lock().unwrap().remove(&id);

            if let Err(err) = result {
                session.report(format!(
                    "failed to connect to {}: {:#}",
                    id.fmt_short(),
                    err
                ));
            }
        });
    }

    async fn dial(&self, addr: EndpointAddr) -> Result<()> {
        if self.peers.lock().unwrap().contains_key(&addr.id) {
            return Ok(());
        }

        let connection = self.endpoint.connect(addr.clone(), ALPN).await?;
        let (mut send, mut recv) = connection.open_bi().await?;

        proto::write(
            &mut send,
            &Message::Hello {
                addr: self.endpoint.addr(),
            },
        )
        .await?;

        let Some(Message::Welcome { peers }) = proto::read(&mut recv).await? else {
            bail!("expected a welcome");
        };

        // The rest of the session is ours to dial: the peer that let us in
        // only introduces us, it does not relay for us.
        for peer in peers {
            self.connect(peer);
        }

        self.serve(addr, connection, send, recv, Dialed::Us).await;

        Ok(())
    }

    async fn answer(&self, connection: Connection) -> Result<()> {
        let (mut send, mut recv) = connection.accept_bi().await?;

        let Some(Message::Hello { addr }) = proto::read(&mut recv).await? else {
            bail!("expected a hello");
        };
        ensure!(
            addr.id == connection.remote_id(),
            "hello does not match the connection it arrived on"
        );

        proto::write(
            &mut send,
            &Message::Welcome {
                peers: self.addrs(),
            },
        )
        .await?;

        // The newcomer is not a peer yet, so this reaches everyone but it.
        self.deliver_all(Message::PeerJoined { addr: addr.clone() });

        self.serve(addr, connection, send, recv, Dialed::Them).await;

        Ok(())
    }

    /// Runs a connection until it ends, reading messages from the peer.
    async fn serve(
        &self,
        addr: EndpointAddr,
        connection: Connection,
        send: SendStream,
        mut recv: RecvStream,
        dialed: Dialed,
    ) {
        let id = addr.id;
        let (outbox, queue) = unbounded_channel();

        let Some(token) = self.enter(addr, connection.clone(), outbox, dialed) else {
            connection.close(DUPLICATE.into(), b"duplicate");
            return;
        };

        self.write(connection.clone(), send, queue);
        self.emit(Event::Connected(id));

        loop {
            match proto::read(&mut recv).await {
                Ok(Some(message)) => self.handle(id, message),
                // The peer finished the stream, which is how it says goodbye.
                Ok(None) => break,
                Err(err) => {
                    if connection.close_reason().is_none() {
                        self.report(format!("{}: {:#}", id.fmt_short(), err));
                    }
                    break;
                }
            }
        }

        self.leave(id, token, &connection);
    }

    /// Drains the outbox of a peer into its half of the session stream.
    fn write(
        &self,
        connection: Connection,
        mut send: SendStream,
        mut queue: UnboundedReceiver<Message>,
    ) {
        let session = self.clone();
        tokio::spawn(async move {
            let id = connection.remote_id();

            while let Some(message) = queue.recv().await {
                if let Err(err) = proto::write(&mut send, &message).await {
                    if connection.close_reason().is_none() {
                        session.report(format!("failed to write to {}: {:#}", id.fmt_short(), err));
                    }
                    break;
                }
            }

            let _ = send.finish();
        });
    }

    fn handle(&self, from: EndpointId, message: Message) {
        match message {
            Message::Data(data) => self.emit(Event::Message { from, data }),
            Message::PeerJoined { addr } => self.connect(addr),
            // The handshake is over; seeing it again means the peer is
            // speaking a protocol we do not.
            Message::Hello { .. } | Message::Welcome { .. } => self.report(format!(
                "unexpected handshake message from {}",
                from.fmt_short()
            )),
        }
    }

    /// Adds a peer to the session, if this connection is the one to keep.
    ///
    /// Two peers may dial each other at the same time, leaving them with two
    /// connections. Both sides keep the one dialed by the lower endpoint id,
    /// so they agree on which one to drop without having to negotiate.
    fn enter(
        &self,
        addr: EndpointAddr,
        connection: Connection,
        outbox: UnboundedSender<Message>,
        dialed: Dialed,
    ) -> Option<u64> {
        let id = addr.id;
        let preferred = match dialed {
            Dialed::Us => self.id() < id,
            Dialed::Them => id < self.id(),
        };

        let mut peers = self.peers.lock().unwrap();
        if let Some(previous) = peers.get(&id) {
            if !preferred {
                return None;
            }
            previous.connection.close(DUPLICATE.into(), b"duplicate");
        }

        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        peers.insert(
            id,
            Peer {
                token,
                addr,
                connection,
                outbox,
            },
        );

        Some(token)
    }

    /// Drops a peer, unless it has already been replaced or removed.
    fn leave(&self, id: EndpointId, token: u64, connection: &Connection) {
        let mut peers = self.peers.lock().unwrap();
        if peers.get(&id).is_none_or(|peer| peer.token != token) {
            return;
        }
        peers.remove(&id);
        drop(peers);

        connection.close(BYE.into(), b"bye");
        self.emit(Event::Disconnected(id));
    }

    fn addrs(&self) -> Vec<EndpointAddr> {
        self.peers
            .lock()
            .unwrap()
            .values()
            .map(|peer| peer.addr.clone())
            .collect()
    }

    fn deliver_all(&self, message: Message) {
        for peer in self.peers.lock().unwrap().values() {
            let _ = peer.outbox.send(message.clone());
        }
    }
}

/// Which side of a connection dialed it.
#[derive(Debug, Clone, Copy)]
enum Dialed {
    Us,
    Them,
}

impl ProtocolHandler for Session {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();

        if let Err(err) = self.answer(connection).await {
            self.report(format!(
                "failed to accept {}: {:#}",
                remote.fmt_short(),
                err
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use iroh::endpoint::presets;
    use iroh::protocol::Router;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;

    struct Member {
        session: Session,
        events: UnboundedReceiver<Event>,
        _router: Router,
    }

    impl Member {
        /// Binds an endpoint that only talks over the local network, so that
        /// the test does not depend on a relay being reachable.
        async fn spawn() -> Self {
            let endpoint = Endpoint::builder(presets::N0DisableRelay)
                .bind()
                .await
                .unwrap();

            let (events_tx, events_rx) = unbounded_channel();
            let session = Session::new(endpoint.clone(), events_tx);

            let router = Router::builder(endpoint)
                .accept(ALPN, session.clone())
                .spawn();

            Self {
                session,
                events: events_rx,
                _router: router,
            }
        }

        /// Waits for the next payload this member receives.
        async fn recv(&mut self) -> (EndpointId, Vec<u8>) {
            loop {
                match self.events.recv().await.expect("session should be running") {
                    Event::Message { from, data } => return (from, data),
                    _ => continue,
                }
            }
        }

        fn peers(&self) -> Vec<EndpointId> {
            let mut peers: Vec<_> = self.session.peers.lock().unwrap().keys().copied().collect();
            peers.sort();
            peers
        }
    }

    /// Waits for every member to see every other member.
    async fn await_mesh(members: &[Member]) {
        let expected: Vec<Vec<EndpointId>> = members
            .iter()
            .map(|member| {
                let mut ids: Vec<_> = members
                    .iter()
                    .map(|other| other.session.id())
                    .filter(|id| *id != member.session.id())
                    .collect();
                ids.sort();
                ids
            })
            .collect();

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let actual: Vec<_> = members.iter().map(Member::peers).collect();
            if actual == expected {
                return;
            }
            assert!(Instant::now() < deadline, "mesh did not form: {:?}", actual);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_ticket_connects_two_peers() {
        let members = [Member::spawn().await, Member::spawn().await];
        members[1]
            .session
            .ticket_join(&members[0].session.ticket_new())
            .unwrap();

        await_mesh(&members).await;
    }

    /// A peer invited by B has to end up connected to A as well.
    #[tokio::test]
    async fn a_peer_joining_through_another_reaches_the_whole_session() {
        let members = [
            Member::spawn().await,
            Member::spawn().await,
            Member::spawn().await,
        ];

        members[1]
            .session
            .ticket_join(&members[0].session.ticket_new())
            .unwrap();
        await_mesh(&members[..2]).await;

        members[2]
            .session
            .ticket_join(&members[1].session.ticket_new())
            .unwrap();
        await_mesh(&members).await;
    }

    /// Two peers inviting each other at the same time keep one connection.
    #[tokio::test]
    async fn a_simultaneous_join_settles_on_one_connection() {
        let members = [Member::spawn().await, Member::spawn().await];

        let first = members[0].session.ticket_new();
        let second = members[1].session.ticket_new();
        members[1].session.ticket_join(&first).unwrap();
        members[0].session.ticket_join(&second).unwrap();

        await_mesh(&members).await;
    }

    /// A payload reaches every peer, including the ones we never dialed.
    #[tokio::test]
    async fn a_broadcast_reaches_the_whole_session() {
        let mut members = [
            Member::spawn().await,
            Member::spawn().await,
            Member::spawn().await,
        ];

        members[1]
            .session
            .ticket_join(&members[0].session.ticket_new())
            .unwrap();
        await_mesh(&members[..2]).await;
        members[2]
            .session
            .ticket_join(&members[1].session.ticket_new())
            .unwrap();
        await_mesh(&members).await;

        let sender = members[0].session.id();
        members[0].session.broadcast(b"hello".to_vec()).unwrap();

        for member in &mut members[1..] {
            assert_eq!(member.recv().await, (sender, b"hello".to_vec()));
        }
    }

    #[tokio::test]
    async fn leaving_drops_every_peer() {
        let members = [Member::spawn().await, Member::spawn().await];
        members[1]
            .session
            .ticket_join(&members[0].session.ticket_new())
            .unwrap();
        await_mesh(&members).await;

        members[0].session.ticket_close().unwrap();
        assert!(members[0].peers().is_empty());

        let deadline = Instant::now() + Duration::from_secs(30);
        while !members[1].peers().is_empty() {
            assert!(Instant::now() < deadline, "peer was not dropped");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
