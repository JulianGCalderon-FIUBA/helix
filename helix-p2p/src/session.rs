//! The collaboration protocol, as a sans-IO state machine.
//!
//! It depends on a topic-based network layer.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use helix_core::{Change, ChangeSet, Rope};
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::{
    crdt::Replica,
    net::{Channel, EndpointId, TopicId},
    wire::{self, Message, SharedId},
};

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
    Unshared { id: SharedId, reason: String },
}

/// What the transport must do.
#[derive(Debug)]
pub enum Transmit {
    Subscribe(TopicId, Vec<EndpointId>),
    Unsubscribe(TopicId),
    Broadcast(Channel, Bytes),
    Shutdown,
}

pub struct Session {
    me: EndpointId,
    rng: StdRng,
    documents: HashMap<SharedId, SharedMeta>,
    replicas: HashMap<SharedId, Replica>,
    events: VecDeque<Event>,
    transmits: VecDeque<Transmit>,
}

impl Session {
    pub fn new(me: EndpointId, seed: u64) -> Self {
        Self {
            me,
            rng: StdRng::seed_from_u64(seed),
            documents: HashMap::new(),
            replicas: HashMap::new(),
            events: VecDeque::new(),
            transmits: VecDeque::new(),
        }
    }

    /// A session we have just joined, which asks for its documents.
    pub fn joined(me: EndpointId, seed: u64) -> Self {
        let mut session = Self::new(me, seed);
        let nonce = session.rng.random();
        session.broadcast(Channel::Session, &Message::DocumentsRequest { nonce });
        session
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

        self.replicas.insert(id, Replica::empty());
        self.transmits
            .push_back(Transmit::Subscribe(id.topic(), vec![meta.owner]));

        let nonce = self.rng.random();
        self.broadcast(
            Channel::Document(id.topic()),
            &Message::SnapshotRequest { nonce },
        );
        Ok(())
    }

    pub fn local_change(&mut self, id: SharedId, changes: &ChangeSet) {
        let Some(replica) = self.replicas.get_mut(&id) else {
            return;
        };
        let update = replica.from_local(changes);
        self.broadcast(Channel::Document(id.topic()), &Message::Edit { update });
    }

    pub fn close(&mut self, id: SharedId) {
        self.unsync(id);
        self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
    }

    pub fn transport_exit(&mut self, channel: Channel, reason: String) {
        match channel {
            Channel::Session => {
                self.documents.clear();
                self.replicas.clear();
            }
            Channel::Document(topic) => {
                let id = SharedId::from(topic);
                if self.unsync(id) {
                    self.events.push_back(Event::Unshared {
                        id,
                        reason: format!("topic lost: {reason}"),
                    });
                }
            }
        }
    }

    pub fn shutdown(&mut self) {
        let ids: Vec<_> = self.replicas.keys().copied().collect();
        for id in ids {
            self.close(id);
        }
        self.documents.clear();
        self.replicas.clear();
        self.transmits.push_back(Transmit::Shutdown);
    }

    /// Receive data from the network layer
    pub fn receive(&mut self, channel: Channel, message: &[u8]) {
        let message = match wire::decode(message) {
            Ok(message) => message,
            Err(err) => {
                log::warn!("dropping malformed message: {err:#}");
                return;
            }
        };

        match (channel, message) {
            (Channel::Session, Message::DocumentsRequest { .. }) => {
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
            (Channel::Session, Message::Share { id, owner, path }) => {
                let meta = SharedMeta { id, owner, path };
                if self.documents.insert(id, meta).is_none() {
                    log::info!("{} shared {}", owner.fmt_short(), id.fmt_short());
                }
            }
            (Channel::Session, Message::Unshare { id }) => {
                self.documents.remove(&id);
                if self.unsync(id) {
                    self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
                    self.events.push_back(Event::Unshared {
                        id,
                        reason: "owner unshared".to_string(),
                    });
                }
            }
            (Channel::Document(topic), Message::SnapshotRequest { .. }) => {
                let id = SharedId::from(topic);
                // Only the owner answers.
                if !self.owns(id) {
                    return;
                }
                let Some(replica) = self.replicas.get(&id) else {
                    return;
                };
                log::debug!("sending snapshot of {}", id.fmt_short());
                let message = Message::Snapshot {
                    nonce: self.rng.random(),
                    replica: replica.encode(),
                };
                self.broadcast(channel, &message);
            }
            (
                Channel::Document(topic),
                Message::Snapshot { replica, .. } | Message::Edit { update: replica },
            ) => self.remote_change(SharedId::from(topic), &replica),
            (channel, _) => log::warn!("dropping message sent to the wrong channel {channel:?}"),
        }
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
        if !changes.is_empty() {
            self.events.push_back(Event::Changes { id, changes });
        }
    }

    fn unsync(&mut self, id: SharedId) -> bool {
        if self.owns(id) {
            log::info!("unsharing {}", id.fmt_short());
            self.documents.remove(&id);
            self.broadcast(Channel::Session, &Message::Unshare { id });
        }
        self.replicas.remove(&id).is_some()
    }

    fn announce(&mut self, meta: &SharedMeta) {
        let message = Message::Share {
            id: meta.id,
            owner: meta.owner,
            path: meta.path.clone(),
        };
        self.broadcast(Channel::Session, &message);
    }

    fn broadcast(&mut self, channel: Channel, message: &Message) {
        self.transmits
            .push_back(Transmit::Broadcast(channel, wire::encode(message)));
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    pub fn documents(&self) -> &HashMap<SharedId, SharedMeta> {
        &self.documents
    }

    fn owns(&self, id: SharedId) -> bool {
        self.documents
            .get(&id)
            .is_some_and(|meta| meta.owner == self.me)
    }
}
