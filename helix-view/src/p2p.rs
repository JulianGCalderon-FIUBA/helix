//! Glue between the editor and the helix-p2p session.

use std::{future::Future, iter};

use anyhow::{ensure, Context, Result};
use helix_core::{Change, ChangeSet, Transaction};
use helix_event::register_hook;
use helix_p2p::{
    net::{self, Channel, Service},
    session::{Event, Session, SessionTicket, Ticket, Transmit},
    wire::SharedId,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};

use crate::{
    editor::Action,
    events::{DocumentDidChange, DocumentDidClose},
    Document, DocumentId, Editor,
};

/// What happened to a shared buffer locally. The document hook can't reach
/// the session, so it queues these.
#[derive(Debug)]
pub enum LocalEvent {
    Change(SharedId, ChangeSet),
}

/// Something p2p needs the editor to handle.
#[derive(Debug)]
pub enum P2pEvent {
    Local(LocalEvent),
    Net(net::Event),
}

pub struct P2p {
    transport: Service,
    session: Option<Session>,
    incoming: UnboundedReceiverStream<net::Event>,
    local_events_tx: UnboundedSender<LocalEvent>,
    local_events: UnboundedReceiver<LocalEvent>,
}

impl P2p {
    pub fn new() -> Self {
        let (transport, incoming) = Service::new();
        let (local_events_tx, local_events) = unbounded_channel();
        Self {
            transport,
            session: None,
            incoming,
            local_events_tx,
            local_events,
        }
    }

    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    pub(crate) async fn next_event(&mut self) -> P2pEvent {
        // Local edits come first, so that remote edits apply on top of them.
        tokio::select! {
            biased;
            Some(event) = self.local_events.recv() => P2pEvent::Local(event),
            Some(event) = self.incoming.next() => P2pEvent::Net(event),
            else => unreachable!("local event queue closed"),
        }
    }

    fn session_mut(&mut self) -> Option<&mut Session> {
        // Queued local edits may not have reached `wait_event` yet, but the
        // session must be up to date before taking any other input.
        while let Ok(event) = self.local_events.try_recv() {
            self.apply_local_event(event);
        }
        self.session.as_mut()
    }

    fn apply_local_event(&mut self, event: LocalEvent) {
        let Some(session) = &mut self.session else {
            return;
        };
        match event {
            LocalEvent::Change(id, changes) => session.local_change(id, &changes),
        }
    }

    fn transmit(&self, transmit: Transmit) {
        match transmit {
            Transmit::Subscribe(topic, bootstrap) => {
                let future = self.transport.subscribe(topic, bootstrap);
                drop(future)
            }
            Transmit::Unsubscribe(topic) => self.transport.unsubscribe(topic),
            Transmit::Send(channel, message) => self.transport.send(channel, message),
        }
    }
}

impl Default for P2p {
    fn default() -> Self {
        Self::new()
    }
}

pub fn register_hooks(p2p: &P2p) {
    // Stops syncing closed documents. This hook has the editor, so it doesn't
    // need to queue.
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if let Some(id) = event.doc.shared {
            let _ = event.editor.with_session(|session| session.close(id));
        }
        Ok(())
    });

    // Queues local edits to shared buffers.
    let local_events = p2p.local_events_tx.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        // TODO: ghost transactions still change the buffer (#44).
        if event.ghost_transaction {
            return Ok(());
        }
        if let Some(id) = event.doc.shared {
            let _ = local_events.send(LocalEvent::Change(id, event.changes.clone()));
        }
        Ok(())
    });
}

impl Editor {
    fn p2p_mut(&mut self) -> Result<&mut P2p> {
        self.p2p.as_mut().context("p2p is disabled")
    }

    pub fn session(&self) -> Result<&Session> {
        let p2p = self.p2p.as_ref().context("p2p is disabled")?;
        p2p.session().context("not in a session")
    }

    /// Gives the session as input and afterwards drains the output queue.
    pub fn with_session<T>(&mut self, f: impl FnOnce(&mut Session) -> T) -> Result<T> {
        let session = self.p2p_mut()?.session_mut().context("not in a session")?;
        let result = f(session);
        self.drain_session_queue();
        Ok(result)
    }

    /// Sends the session's messages and applies its events. Runs after every
    /// input to the session.
    fn drain_session_queue(&mut self) {
        let Some(p2p) = &mut self.p2p else {
            return;
        };
        while let Some(transmit) = p2p.session.as_mut().and_then(Session::poll_transmit) {
            p2p.transmit(transmit);
        }
        let events: Vec<_> = iter::from_fn(|| p2p.session.as_mut()?.poll_event()).collect();
        for event in events {
            self.apply_session_event(event);
        }
    }

    pub fn handle_p2p_event(&mut self, event: P2pEvent) {
        match event {
            P2pEvent::Local(event) => self.handle_p2p_local_event(event),
            P2pEvent::Net(event) => self.handle_p2p_net_event(event),
        }
    }

    fn handle_p2p_local_event(&mut self, event: LocalEvent) {
        let Some(p2p) = &mut self.p2p else {
            return;
        };
        p2p.apply_local_event(event);
        self.drain_session_queue();
    }

    fn handle_p2p_net_event(&mut self, event: net::Event) {
        match event {
            // Losing the session's topic loses the session.
            net::Event::Exit(Channel::Topic(topic))
                if self.session().is_ok_and(|session| session.topic() == topic) =>
            {
                let _ = self.leave_session();
                self.set_error("lost connection to the session");
            }
            net::Event::Received(channel, message) => {
                let _ = self.with_session(|session| session.receive(channel, &message));
            }
            // The session stops syncing whatever depended on the channel.
            net::Event::Exit(channel) => {
                let _ = self.with_session(|session| session.transport_exit(channel));
            }
        }
    }

    pub fn session_ticket(&mut self) -> Result<impl Future<Output = String> + 'static> {
        let name = self.config().p2p.name.clone();
        let p2p = self.p2p_mut()?;
        let session = p2p.session.get_or_insert_with(|| {
            let session = Session::create(p2p.transport.id(), name, rand::random());
            drop(p2p.transport.subscribe(session.topic(), Vec::new()));
            session
        });
        let topic = session.topic();
        let addr = p2p.transport.address();
        Ok(async move {
            let addr = addr.await;
            SessionTicket { topic, addr }.encode_string()
        })
    }

    pub fn join_session(
        &mut self,
        ticket: &str,
    ) -> Result<impl Future<Output = Result<()>> + 'static> {
        let name = self.config().p2p.name.clone();
        let p2p = self.p2p_mut()?;
        ensure!(p2p.session.is_none(), "already in a session");
        let SessionTicket { topic, addr } = SessionTicket::decode_string(ticket)?;
        ensure!(
            addr.id != p2p.transport.id(),
            "cannot join your own session"
        );

        let bootstrap = addr.id;
        p2p.transport.add_address(addr);
        let joined = p2p.transport.subscribe(topic, vec![bootstrap]);
        let session = Session::join(p2p.transport.id(), name, topic, rand::random());
        p2p.session = Some(session);
        self.drain_session_queue();
        Ok(joined)
    }

    pub fn leave_session(&mut self) -> Result<()> {
        let p2p = self.p2p_mut()?;
        let session = p2p.session.take().context("not in a session")?;
        for transmit in session.leave() {
            p2p.transmit(transmit);
        }
        p2p.transport.reset();

        for doc in self.documents_mut() {
            doc.shared = None;
        }
        Ok(())
    }

    pub fn shared_document(&self, id: SharedId) -> Option<&Document> {
        self.documents().find(|doc| doc.shared == Some(id))
    }

    pub fn share_document(&mut self, doc_id: DocumentId) -> Result<SharedId> {
        let doc = doc!(self, &doc_id);
        ensure!(doc.shared.is_none(), "buffer is already shared");

        let path = doc.path().map(|path| {
            let (workspace, _) = helix_loader::find_workspace();
            path.strip_prefix(&workspace).unwrap_or(path).to_path_buf()
        });
        let text = doc.text().clone();

        let id = self.with_session(|session| session.share(path, &text))?;
        doc_mut!(self, &doc_id).shared = Some(id);
        Ok(id)
    }

    pub fn unshare_document(&mut self, doc_id: DocumentId) -> Result<()> {
        let id = doc!(self, &doc_id).shared.context("buffer is not shared")?;
        self.with_session(|session| session.close(id))?;
        doc_mut!(self, &doc_id).shared = None;
        Ok(())
    }

    pub fn open_shared_document(&mut self, id: SharedId, action: Action) -> Result<()> {
        if let Some(doc_id) = self.shared_document(id).map(Document::id) {
            self.switch(doc_id, action);
            return Ok(());
        }

        self.with_session(|session| session.open(id))??;
        let doc_id = self.new_file(action);
        doc_mut!(self, &doc_id).shared = Some(id);

        let session = self.session()?;
        let meta = &session.documents()[&id];
        let status = format!("opening {} from {}", meta.label(), session.name(meta.owner));
        self.set_status(status);
        Ok(())
    }

    fn apply_session_event(&mut self, event: Event) {
        match event {
            Event::Changes { id, changes } => self.apply_remote_changes(id, changes),
            Event::Opened { id, text } => {
                let Some(doc) = self.shared_document(id) else {
                    return;
                };
                let len = doc.text().len_chars();
                self.apply_remote_changes(id, vec![(0, len, Some(text.into()))]);
            }
            Event::Unshared { id, reason } => {
                let Some(doc) = self.documents_mut().find(|doc| doc.shared == Some(id)) else {
                    return;
                };
                doc.shared = None;
                let name = doc.display_name().into_owned();
                self.set_status(format!("stopped syncing {name}: {reason}"));
            }
        }
    }

    fn apply_remote_changes(&mut self, id: SharedId, changes: Vec<Change>) {
        let Some(doc_id) = self.shared_document(id).map(Document::id) else {
            return;
        };
        let view_id = self.get_synced_view_id(doc_id);
        let doc = doc_mut!(self, &doc_id);

        // Take the marker off while applying, so that the hook
        // doesn't send the remote edit back out as a local one.
        doc.shared = None;
        let transaction = Transaction::change(doc.text(), changes.into_iter());
        doc.apply(&transaction, view_id);
        doc.shared = Some(id);
    }
}
