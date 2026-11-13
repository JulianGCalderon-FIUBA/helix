//! The collaboration protocol, as a sans-IO state machine.
//! It relies on the transport layer to communicate with peers.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use helix_core::{Change, ChangeSet, Rope};
use iroh_tickets::ParseError;
pub use iroh_tickets::Ticket;
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    crdt::Replica,
    net::{Channel, EndpointAddr, EndpointId, TopicId},
    wire::{self, GossipMessage, PeerMessage, SharedId},
};

/// An invitation into a session. Any member can hand one out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTicket {
    pub topic: TopicId,
    pub addr: EndpointAddr,
}

impl Ticket for SessionTicket {
    const KIND: &'static str = "helix";

    fn encode_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("ticket should serialize")
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// Metadata about a shared document, opened or not.
#[derive(Debug, Clone)]
pub struct SharedMeta {
    pub id: SharedId,
    pub owner: EndpointId,
    /// The path relative to the owner's workspace.
    pub path: Option<PathBuf>,
}

impl SharedMeta {
    pub fn label(&self) -> String {
        match &self.path {
            Some(path) => format!("{} ({})", path.display(), self.id.fmt_short()),
            None => format!("[scratch] ({})", self.id.fmt_short()),
        }
    }
}

/// What the session tells the driver.
#[derive(Debug)]
pub enum Event {
    Changes { id: SharedId, changes: Vec<Change> },
    Reset { id: SharedId, text: String },
    Unshared { id: SharedId, reason: String },
    Joined { name: String },
    Left { name: String },
}

/// What the transport must do.
#[derive(Debug)]
pub enum Transmit {
    Subscribe(TopicId, Vec<EndpointId>),
    Unsubscribe(TopicId),
    Send(Channel, Bytes),
}

pub struct Session {
    me: EndpointId,
    name: String,
    topic: TopicId,
    rng: StdRng,
    /// The other peers, by name.
    peers: HashMap<EndpointId, String>,
    documents: HashMap<SharedId, SharedMeta>,
    replicas: HashMap<SharedId, Replica>,
    /// The owner and document of each snapshot we requested.
    pending: HashMap<u64, (EndpointId, SharedId)>,
    events: VecDeque<Event>,
    transmits: VecDeque<Transmit>,
}

impl Session {
    /// A new session with no peers
    /// Without a name, peers see our short id.
    pub fn create(me: EndpointId, name: Option<String>, seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let topic = TopicId::from_bytes(rng.random());
        log::info!("created session {}", topic.fmt_short());
        Self::new(me, name, topic, rng)
    }

    pub fn join(me: EndpointId, name: Option<String>, topic: TopicId, seed: u64) -> Self {
        log::info!("joining session {}", topic.fmt_short());
        let mut session = Self::new(me, name, topic, StdRng::seed_from_u64(seed));
        session.hello();
        session
    }

    fn new(me: EndpointId, name: Option<String>, topic: TopicId, rng: StdRng) -> Self {
        Self {
            me,
            name: name.unwrap_or_else(|| me.fmt_short().to_string()),
            topic,
            rng,
            peers: HashMap::new(),
            documents: HashMap::new(),
            replicas: HashMap::new(),
            pending: HashMap::new(),
            events: VecDeque::new(),
            transmits: VecDeque::new(),
        }
    }

    pub fn share(&mut self, path: Option<PathBuf>, text: &Rope) -> SharedId {
        let id = SharedId::from(TopicId::from_bytes(self.rng.random()));
        let meta = SharedMeta {
            id,
            owner: self.me,
            path,
        };
        log::info!("sharing {}", meta.label());

        self.replicas.insert(id, Replica::new(text));
        self.transmits
            .push_back(Transmit::Subscribe(id.topic(), Vec::new()));
        self.announce(&meta);
        self.documents.insert(id, meta);
        id
    }

    pub fn open(&mut self, id: SharedId) -> Result<()> {
        let meta = self
            .documents
            .get(&id)
            .with_context(|| format!("{} is no longer shared", id.fmt_short()))?;
        log::info!("opening {}", meta.label());
        let owner = meta.owner;

        // Subscribe first, so that no edit after the snapshot is missed.
        self.replicas.insert(id, Replica::empty());
        self.transmits
            .push_back(Transmit::Subscribe(id.topic(), vec![owner]));
        self.resync(id, owner);
        Ok(())
    }

    /// Asks the owner for a snapshot.
    fn resync(&mut self, id: SharedId, owner: EndpointId) {
        let request = self.rng.random();
        self.pending.insert(request, (owner, id));
        self.send(
            Channel::Peer(owner),
            &PeerMessage::SnapshotRequest { request, id },
        );
    }

    pub fn local_change(&mut self, id: SharedId, changes: &ChangeSet) {
        // The snapshot would overwrite these edits.
        if self.syncing(id) {
            return;
        }
        let Some(replica) = self.replicas.get_mut(&id) else {
            return;
        };
        let update = replica.from_local(changes);
        self.send(Channel::Topic(id.topic()), &GossipMessage::Edit { update });
    }

    pub fn close(&mut self, id: SharedId) {
        self.unsync(id);
        self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
    }

    pub fn transport_exit(&mut self, channel: Channel) {
        match channel {
            // The driver drops the session when it loses it.
            Channel::Topic(topic) if topic == self.topic => {}
            Channel::Topic(topic) => {
                let id = SharedId::from(topic);
                if self.unsync(id) {
                    self.events.push_back(Event::Unshared {
                        id,
                        reason: "lost connection".to_string(),
                    });
                }
            }
            Channel::Peer(peer) => {
                let lost: Vec<_> = self
                    .pending
                    .extract_if(|_, (owner, _)| *owner == peer)
                    .map(|(_, (_, id))| id)
                    .collect();
                for id in lost {
                    if self.unsync(id) {
                        self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
                        self.events.push_back(Event::Unshared {
                            id,
                            reason: "lost connection to owner".to_string(),
                        });
                    }
                }
            }
        }
    }

    /// The driver must send these before leaving the session's swarm.
    pub fn leave(mut self) -> impl Iterator<Item = Transmit> {
        let owned: Vec<_> = self
            .documents
            .values()
            .filter(|meta| meta.owner == self.me)
            .map(|meta| meta.id)
            .collect();
        for id in owned {
            log::info!("unsharing {}", id.fmt_short());
            self.send(Channel::Topic(self.topic), &GossipMessage::Unshare { id });
        }
        let bye = GossipMessage::Bye {
            peer: self.me,
            nonce: self.rng.random(),
        };
        self.send(Channel::Topic(self.topic), &bye);
        self.transmits.into_iter()
    }

    /// Receive data from the network layer
    pub fn receive(&mut self, channel: Channel, message: &[u8]) {
        match channel {
            Channel::Peer(peer) => self.receive_from_peer(peer, message),
            Channel::Topic(topic) if topic == self.topic => self.receive_from_session(message),
            // Any other topic is a document's, or an old session's.
            Channel::Topic(topic) => self.receive_from_document(SharedId::from(topic), message),
        }
    }

    fn receive_from_session(&mut self, message: &[u8]) {
        let Some(message) = decode(message) else {
            return;
        };

        match message {
            GossipMessage::Hello { peer, name, .. } => {
                if self.peers.insert(peer, name.clone()).is_none() {
                    log::info!("{} joined", peer.fmt_short());
                    self.events.push_back(Event::Joined { name });
                    self.announce_owned();
                    self.hello();
                }
            }
            GossipMessage::Bye { peer, .. } => {
                if let Some(name) = self.peers.remove(&peer) {
                    log::info!("{} left", peer.fmt_short());
                    self.events.push_back(Event::Left { name });
                }
            }
            GossipMessage::Share {
                id, owner, path, ..
            } => {
                let meta = SharedMeta { id, owner, path };
                if self.documents.insert(id, meta).is_none() {
                    log::info!("{} shared {}", owner.fmt_short(), id.fmt_short());
                }
            }
            GossipMessage::Unshare { id } => {
                if let Some(meta) = self.documents.remove(&id) {
                    log::info!("{} unshared {}", meta.owner.fmt_short(), id.fmt_short());
                }
                if self.unsync(id) {
                    self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
                    self.events.push_back(Event::Unshared {
                        id,
                        reason: "owner unshared".to_string(),
                    });
                }
            }
            GossipMessage::Edit { .. } => log::warn!("dropping edit sent to the session"),
        }
    }

    fn receive_from_document(&mut self, id: SharedId, message: &[u8]) {
        let Some(message) = decode(message) else {
            return;
        };

        match message {
            GossipMessage::Edit { update } => self.remote_change(id, &update),
            _ => log::warn!("dropping session message sent to {}", id.fmt_short()),
        }
    }

    fn receive_from_peer(&mut self, peer: EndpointId, message: &[u8]) {
        let Some(message) = decode(message) else {
            return;
        };

        match message {
            PeerMessage::SnapshotRequest { request, id } => {
                let replica = self.snapshot(id).map_err(|err| format!("{err:#}"));
                self.send(
                    Channel::Peer(peer),
                    &PeerMessage::Snapshot { request, replica },
                );
            }
            PeerMessage::Snapshot { request, replica } => {
                let Some((_, id)) = self.pending.remove(&request) else {
                    return;
                };
                match replica {
                    Ok(replica) => self.load_snapshot(id, &replica),
                    Err(reason) => {
                        log::warn!(
                            "{} refused snapshot of {}: {reason}",
                            peer.fmt_short(),
                            id.fmt_short()
                        );
                        if self.unsync(id) {
                            self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
                            self.events.push_back(Event::Unshared {
                                id,
                                reason: "owner refused snapshot".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    fn snapshot(&self, id: SharedId) -> Result<Vec<u8>> {
        ensure!(self.owns(id), "{} is not ours", id.fmt_short());
        let replica = self
            .replicas
            .get(&id)
            .with_context(|| format!("{} is not open", id.fmt_short()))?;
        log::debug!("sending snapshot of {}", id.fmt_short());
        Ok(replica.encode())
    }

    fn remote_change(&mut self, id: SharedId, update: &[u8]) {
        let Some(replica) = self.replicas.get_mut(&id) else {
            return;
        };
        let changes = match replica.from_remote(update) {
            Ok(changes) => changes,
            Err(err) => {
                log::warn!("dropping update for {}: {err:#}", id.fmt_short());
                return;
            }
        };
        // The snapshot would carry these changes.
        if !changes.is_empty() && !self.syncing(id) {
            self.events.push_back(Event::Changes { id, changes });
        }
    }

    fn load_snapshot(&mut self, id: SharedId, snapshot: &[u8]) {
        let Some(replica) = self.replicas.get_mut(&id) else {
            return;
        };
        if let Err(err) = replica.from_remote(snapshot) {
            log::warn!("dropping snapshot of {}: {err:#}", id.fmt_short());
            return;
        }
        let text = replica.text();
        self.events.push_back(Event::Reset { id, text });
    }

    fn unsync(&mut self, id: SharedId) -> bool {
        if self.owns(id) {
            log::info!("unsharing {}", id.fmt_short());
            self.documents.remove(&id);
            self.send(Channel::Topic(self.topic), &GossipMessage::Unshare { id });
        }
        self.replicas.remove(&id).is_some()
    }

    fn hello(&mut self) {
        let message = GossipMessage::Hello {
            peer: self.me,
            name: self.name.clone(),
            nonce: self.rng.random(),
        };
        self.send(Channel::Topic(self.topic), &message);
    }

    fn announce_owned(&mut self) {
        let owned: Vec<_> = self
            .documents
            .values()
            .filter(|meta| meta.owner == self.me)
            .cloned()
            .collect();
        for meta in owned {
            self.announce(&meta);
        }
    }

    fn announce(&mut self, meta: &SharedMeta) {
        let message = GossipMessage::Share {
            id: meta.id,
            owner: meta.owner,
            path: meta.path.clone(),
            nonce: self.rng.random(),
        };
        self.send(Channel::Topic(self.topic), &message);
    }

    fn send(&mut self, channel: Channel, message: &impl Serialize) {
        self.transmits
            .push_back(Transmit::Send(channel, wire::encode(message)));
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    pub fn topic(&self) -> TopicId {
        self.topic
    }

    pub fn peers(&self) -> &HashMap<EndpointId, String> {
        &self.peers
    }

    pub fn name(&self, peer: EndpointId) -> String {
        if peer == self.me {
            return self.name.clone();
        }
        let name = self.peers.get(&peer).cloned();
        name.unwrap_or_else(|| peer.fmt_short().to_string())
    }

    pub fn documents(&self) -> &HashMap<SharedId, SharedMeta> {
        &self.documents
    }

    fn syncing(&self, id: SharedId) -> bool {
        self.pending.values().any(|(_, pending)| *pending == id)
    }

    pub fn owns(&self, id: SharedId) -> bool {
        self.documents
            .get(&id)
            .is_some_and(|meta| meta.owner == self.me)
    }
}

fn decode<T: DeserializeOwned>(message: &[u8]) -> Option<T> {
    wire::decode(message)
        .inspect_err(|err| log::warn!("dropping malformed message: {err:#}"))
        .ok()
}

#[cfg(test)]
mod tests;
