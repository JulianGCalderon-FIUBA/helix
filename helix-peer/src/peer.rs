//! A peer without an editor: the real transport and session, and the text and
//! selection a user would see. It drives the session the way the editor does.

use std::{collections::HashMap, future::Future};

use anyhow::{Context, Result};
use helix_core::{diff::compare_ropes, Rope, Selection, Transaction};
use helix_p2p::{
    crdt::Replica,
    net::{self, Channel, Service},
    session::{Event, Session, SessionTicket, Ticket, Transmit},
    wire::SharedId,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

pub struct Buffer {
    pub text: Rope,
    pub selection: Selection,
}

impl Buffer {
    fn new(text: Rope) -> Self {
        Self {
            text,
            selection: Selection::point(0),
        }
    }

    fn apply(&mut self, transaction: &Transaction) -> bool {
        if !transaction.apply(&mut self.text) {
            return false;
        }
        self.selection = self.selection.clone().map(transaction.changes());
        true
    }
}

/// What a peer holds for a document, to compare with other peers.
pub struct Snapshot {
    pub id: SharedId,
    pub text: String,
    /// None while opening.
    pub replica: Option<String>,
    pub ops: usize,
}

pub struct Peer {
    pub name: String,
    transport: Service,
    session: Option<Session>,
    buffers: HashMap<SharedId, Buffer>,
    /// The buffer the user is in.
    focus: Option<SharedId>,
}

impl Peer {
    pub fn new(name: String) -> (Self, UnboundedReceiverStream<net::Event>) {
        let (transport, events) = Service::new();
        let peer = Self {
            name,
            transport,
            session: None,
            buffers: HashMap::new(),
            focus: None,
        };
        (peer, events)
    }

    pub fn host(&mut self) -> impl Future<Output = String> + 'static {
        let me = self.transport.id();
        let session = Session::create(me, Some(self.name.clone()), rand::random());
        let topic = session.topic();
        drop(self.transport.subscribe(topic, Vec::new()));
        self.session = Some(session);
        let addr = self.transport.address();
        async move {
            let addr = addr.await;
            SessionTicket { topic, addr }.encode_string()
        }
    }

    pub fn join(&mut self, ticket: &str) -> Result<()> {
        let SessionTicket { topic, addr } = SessionTicket::decode_string(ticket)?;
        let bootstrap = addr.id;
        self.transport.add_address(addr);
        drop(self.transport.subscribe(topic, vec![bootstrap]));
        let me = self.transport.id();
        let session = Session::join(me, Some(self.name.clone()), topic, rand::random());
        self.session = Some(session);
        self.drain();
        Ok(())
    }

    pub fn share(&mut self, text: &str) -> Result<SharedId> {
        let text = Rope::from(text);
        let id = self.session_mut()?.share(None, &text);
        self.buffers.insert(id, Buffer::new(text));
        self.drain();
        Ok(id)
    }

    /// Focuses the document, opening it first if needed.
    pub fn open(&mut self, id: SharedId) -> Result<()> {
        if !self.buffers.contains_key(&id) {
            self.session_mut()?.open(id)?;
            self.buffers.insert(id, Buffer::new(Rope::new()));
        }
        self.focus = Some(id);
        self.sync_focus();
        self.drain();
        Ok(())
    }

    pub fn close(&mut self, id: SharedId) {
        if let Some(session) = &mut self.session {
            session.close(id);
        }
        self.buffers.remove(&id);
        if self.focus == Some(id) {
            self.focus = None;
        }
        self.sync_focus();
        self.drain();
    }

    pub fn focused(&self) -> Option<(SharedId, &Buffer)> {
        let id = self.focus?;
        Some((id, self.buffers.get(&id)?))
    }

    /// Edits the focused buffer, like the user typing.
    pub fn apply(&mut self, transaction: &Transaction) {
        let Some(id) = self.focus else {
            return;
        };
        let Some(buffer) = self.buffers.get_mut(&id) else {
            return;
        };
        if !buffer.apply(transaction) {
            return;
        }
        if let Some(session) = &mut self.session {
            session.local_change(id, transaction.changes());
        }
        self.sync_focus();
        self.drain();
    }

    pub fn select(&mut self, selection: Selection) {
        let Some(buffer) = self.focus.and_then(|id| self.buffers.get_mut(&id)) else {
            return;
        };
        buffer.selection = selection;
        self.sync_focus();
        self.drain();
    }

    /// Where the other peers' cursors are in the focused buffer.
    pub fn peer_cursors(&self) -> Vec<usize> {
        let (Some(session), Some((id, buffer))) = (&self.session, self.focused()) else {
            return Vec::new();
        };
        let len = buffer.text.len_chars();
        let peers = session.peer_selections(id).into_iter();
        peers
            .map(|(_, selection)| selection.primary().head.min(len))
            .collect()
    }

    pub fn is_synced(&self, id: SharedId) -> bool {
        let synced = |session: &Session| session.synced_replica(id).is_some();
        self.buffers.contains_key(&id) && self.session.as_ref().is_some_and(synced)
    }

    pub fn snapshot(&self) -> Vec<Snapshot> {
        self.buffers
            .iter()
            .map(|(&id, buffer)| {
                let replica = self.session.as_ref().and_then(|s| s.synced_replica(id));
                Snapshot {
                    id,
                    text: buffer.text.to_string(),
                    replica: replica.map(Replica::text),
                    ops: replica.map_or(0, Replica::len_ops),
                }
            })
            .collect()
    }

    pub fn handle(&mut self, event: net::Event) {
        let Some(session) = &mut self.session else {
            return;
        };
        match event {
            net::Event::Exit(Channel::Topic(topic)) if topic == session.topic() => {
                log::error!("{} lost the session", self.name);
                self.session = None;
                self.buffers.clear();
                self.focus = None;
            }
            net::Event::Received(channel, message) => session.receive(channel, &message),
            net::Event::Exit(channel) => session.transport_exit(channel),
        }
        self.drain();
    }

    fn session_mut(&mut self) -> Result<&mut Session> {
        self.session.as_mut().context("not in a session")
    }

    /// The editor syncs focus after every key.
    fn sync_focus(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        let focus = self.focus.and_then(|id| {
            let buffer = self.buffers.get(&id)?;
            let cursor = buffer.selection.primary().cursor(buffer.text.slice(..));
            Some((id, cursor, buffer.selection.clone()))
        });
        session.update_focus(focus);
    }

    /// Sends the session's messages and applies its events, after every input.
    fn drain(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        while let Some(transmit) = session.poll_transmit() {
            match transmit {
                Transmit::Subscribe(topic, bootstrap) => {
                    drop(self.transport.subscribe(topic, bootstrap))
                }
                Transmit::Unsubscribe(topic) => self.transport.unsubscribe(topic),
                Transmit::Send(channel, message) => self.transport.send(channel, message),
            }
        }
        while let Some(event) = session.poll_event() {
            match event {
                Event::Changes { id, changes } => {
                    if let Some(buffer) = self.buffers.get_mut(&id) {
                        let transaction = Transaction::change(&buffer.text, changes.into_iter());
                        buffer.apply(&transaction);
                    }
                }
                Event::Reset { id, text } => {
                    if let Some(buffer) = self.buffers.get_mut(&id) {
                        // Keeps the selection where the text didn't change.
                        let transaction = compare_ropes(&buffer.text, &Rope::from(text));
                        buffer.apply(&transaction);
                    }
                }
                Event::Unshared { id, reason } => {
                    log::warn!("{} stopped syncing {}: {reason}", self.name, id.fmt_short());
                    self.buffers.remove(&id);
                    if self.focus == Some(id) {
                        self.focus = None;
                    }
                }
                Event::Joined { .. } | Event::Left { .. } => {}
            }
        }
    }
}
