//! The collaboration protocol, as a sans-IO state machine.
//!
//! The [`Session`] performs no IO, never awaits, never reads the clock and
//! spawns nothing. The caller feeds it inputs through plain method calls,
//! then drains what comes out: events for the editor, transmits for the
//! transport. Keeping every protocol decision here, apart from the editor
//! and the transport, makes it testable without either.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use helix_core::{Change, ChangeSet, Operation, Rope};
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::{
    crdt::Replica,
    net::{Channel, EndpointId, TopicId},
    wire::{self, Message, SharedId},
};

/// What the session knows about a shared document, opened or not.
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
            None => format!("buffer ({})", self.id.fmt_short()),
        }
    }
}

/// Why we stopped syncing a document.
#[derive(Debug)]
pub enum Reason {
    OwnerUnshared,
    TopicLost(String),
}

/// What the editor must do.
#[derive(Debug)]
pub enum Event {
    /// Apply to our copy before the next input.
    Changes { id: SharedId, changes: Vec<Change> },
    /// We stopped syncing a document. Its buffer stays, as a plain one.
    Unshared { id: SharedId, reason: Reason },
    /// We are no longer in the session. No reason means we left on purpose.
    Left { reason: Option<String> },
}

/// What the transport must do.
#[derive(Debug)]
pub enum Transmit {
    Subscribe(TopicId, Vec<EndpointId>),
    Unsubscribe(TopicId),
    Broadcast(Channel, Bytes),
    Leave,
}

pub struct Session {
    me: EndpointId,
    /// Seeded by the caller, so that the session stays deterministic.
    rng: StdRng,
    documents: HashMap<SharedId, SharedMeta>,
    replicas: HashMap<SharedId, Replica>,
    /// Documents we opened but have no snapshot of yet, with the length of
    /// their buffer. Edits typed meanwhile are discarded, and the first
    /// snapshot replaces them.
    waiting: HashMap<SharedId, usize>,
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
            waiting: HashMap::new(),
            events: VecDeque::new(),
            transmits: VecDeque::new(),
        }
    }

    /// A session we have just joined, which asks for its documents.
    ///
    /// Anything shared after this reaches us as a `Share`,
    /// since we are already in the swarm.
    pub fn joined(me: EndpointId, seed: u64) -> Self {
        let mut session = Self::new(me, seed);
        session.broadcast(Channel::Session, &Message::DocumentsRequest);
        session
    }

    // What the user does.

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

    /// Starts syncing a document. Its buffer starts empty, and fills in
    /// once the owner answers with a snapshot.
    pub fn open(&mut self, id: SharedId) -> Result<()> {
        let meta = self
            .documents
            .get(&id)
            .with_context(|| format!("{} is no longer shared", id.fmt_short()))?;
        log::info!("opening {}", meta.label());

        let owner = meta.owner;
        self.replicas.insert(id, Replica::empty());
        self.waiting.insert(id, 0);
        self.transmits
            .push_back(Transmit::Subscribe(id.topic(), vec![owner]));
        let nonce = self.rng.random();
        self.broadcast(
            Channel::Document(id.topic()),
            &Message::SnapshotRequest { nonce },
        );
        Ok(())
    }

    /// Stops syncing a document whose buffer is gone.
    pub fn close(&mut self, id: SharedId) {
        self.stop_syncing(id);
        self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
    }

    pub fn leave(&mut self) {
        let ids: Vec<_> = self.replicas.keys().copied().collect();
        for id in ids {
            self.close(id);
        }
        self.forget();
        // The queue keeps the unshares ahead of this.
        self.transmits.push_back(Transmit::Leave);
        self.events.push_back(Event::Left { reason: None });
    }

    pub fn local_change(&mut self, id: SharedId, changes: &ChangeSet) {
        if let Some(len) = self.waiting.get_mut(&id) {
            for op in changes.changes() {
                match op {
                    Operation::Retain(_) => {}
                    Operation::Insert(text) => *len += text.chars().count(),
                    Operation::Delete(n) => *len -= n,
                }
            }
            return;
        }
        let Some(replica) = self.replicas.get_mut(&id) else {
            return;
        };
        let update = replica.from_local(changes);
        self.broadcast(Channel::Document(id.topic()), &Message::Edit { update });
    }

    // What the transport receives.

    pub fn receive(&mut self, channel: Channel, message: &[u8]) {
        let message = match wire::decode(message) {
            Ok(message) => message,
            Err(err) => {
                log::warn!("dropping malformed message: {err:#}");
                return;
            }
        };

        match (channel, message) {
            (Channel::Session, Message::DocumentsRequest) => {
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
                if self.stop_syncing(id) {
                    self.transmits.push_back(Transmit::Unsubscribe(id.topic()));
                    self.events.push_back(Event::Unshared {
                        id,
                        reason: Reason::OwnerUnshared,
                    });
                }
            }
            (Channel::Document(topic), Message::SnapshotRequest { .. }) => {
                let id = SharedId::from(topic);
                // Only the owner answers, so each request gets one snapshot.
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
            (Channel::Document(topic), Message::Snapshot { replica, .. }) => {
                self.import(SharedId::from(topic), &replica, true)
            }
            (Channel::Document(topic), Message::Edit { update }) => {
                self.import(SharedId::from(topic), &update, false)
            }
            (channel, _) => log::warn!("dropping message sent to the wrong channel {channel:?}"),
        }
    }

    /// The transport left a channel on its own.
    pub fn exited(&mut self, channel: Channel, reason: String) {
        match channel {
            Channel::Session => {
                // The transport has already shut down,
                // so there's nothing to transmit.
                self.forget();
                self.events.push_back(Event::Left {
                    reason: Some(reason),
                });
            }
            Channel::Document(topic) => {
                // The transport has already unsubscribed.
                let id = SharedId::from(topic);
                if self.stop_syncing(id) {
                    self.events.push_back(Event::Unshared {
                        id,
                        reason: Reason::TopicLost(reason),
                    });
                }
            }
        }
    }

    pub fn documents(&self) -> &HashMap<SharedId, SharedMeta> {
        &self.documents
    }

    // Outputs.

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// Applies another peer's update to our replica.
    fn import(&mut self, id: SharedId, update: &[u8], snapshot: bool) {
        let Some(replica) = self.replicas.get_mut(&id) else {
            log::debug!("dropping update for unknown document {}", id.fmt_short());
            return;
        };
        let changes = match replica.from_remote(update) {
            Ok(changes) => changes,
            Err(err) => {
                log::warn!("dropping update for {}: {err:#}", id.fmt_short());
                return;
            }
        };

        let changes = match self.waiting.get(&id) {
            // Edits before the snapshot stay in the replica,
            // and reach the buffer along with the snapshot.
            Some(_) if !snapshot => return,
            // The buffer only holds what was typed while waiting, so replace
            // it with the replica, which now holds the snapshot and every
            // edit imported before it.
            Some(&typed) => {
                self.waiting.remove(&id);
                vec![(0, typed, Some(replica.text().into()))]
            }
            None => changes,
        };
        if !changes.is_empty() {
            self.events.push_back(Event::Changes { id, changes });
        }
    }

    /// Drops our replica of a document, and unshares it if we own it.
    /// Returns whether we were syncing it.
    fn stop_syncing(&mut self, id: SharedId) -> bool {
        if self.owns(id) {
            log::info!("unsharing {}", id.fmt_short());
            self.documents.remove(&id);
            self.broadcast(Channel::Session, &Message::Unshare { id });
        }
        self.waiting.remove(&id);
        self.replicas.remove(&id).is_some()
    }

    fn forget(&mut self) {
        self.documents.clear();
        self.replicas.clear();
        self.waiting.clear();
    }

    fn owns(&self, id: SharedId) -> bool {
        self.documents
            .get(&id)
            .is_some_and(|meta| meta.owner == self.me)
    }

    /// Tells the session about a document, but not what it contains.
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
}
