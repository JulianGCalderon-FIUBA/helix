//! Glue between the editor and the helix-p2p session.

use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use helix_core::{ChangeSet, Transaction};
use helix_event::register_hook;
use helix_p2p::{
    net::{self, Channel, Service},
    session::{Event, Session, SharedMeta, Transmit},
    wire::SharedId,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    editor::Action,
    events::{DocumentDidChange, DocumentDidClose},
    Document, DocumentId, Editor,
};

pub struct P2p {
    pub service: Service,
    /// Present only while we are in a session.
    session: Option<Session>,
    pub(crate) incoming: UnboundedReceiverStream<net::Event>,
    /// Local changes to shared buffers cannot be applied directly by the hook.
    local_changes_tx: UnboundedSender<(SharedId, ChangeSet)>,
    pub(crate) local_changes: UnboundedReceiver<(SharedId, ChangeSet)>,
}

impl P2p {
    pub fn new() -> Self {
        let (service, incoming) = Service::new();
        let (local_changes_tx, local_changes) = unbounded_channel();
        Self {
            session: None,
            service,
            incoming,
            local_changes_tx,
            local_changes,
        }
    }

    pub fn session(&mut self) -> Option<&mut Session> {
        // The session must be up to date before taking any more inputs.
        while let Ok((id, changes)) = self.local_changes.try_recv() {
            if let Some(session) = &mut self.session {
                session.local_change(id, &changes);
            }
        }
        self.session.as_mut()
    }

    pub(crate) fn local_change(&mut self, id: SharedId, changes: ChangeSet) {
        if let Some(session) = &mut self.session {
            session.local_change(id, &changes);
        }
        self.session();
    }

    pub fn documents(&self) -> Option<&HashMap<SharedId, SharedMeta>> {
        self.session.as_ref().map(Session::documents)
    }

    fn transmit(&self, transmit: Transmit) {
        match transmit {
            Transmit::Subscribe(topic, bootstrap) => self.service.subscribe(topic, bootstrap),
            Transmit::Unsubscribe(topic) => self.service.unsubscribe(topic),
            Transmit::Broadcast(channel, message) => self.service.broadcast(channel, message),
            Transmit::Shutdown => self.service.shutdown(),
        }
    }
}

impl Default for P2p {
    fn default() -> Self {
        Self::new()
    }
}

pub fn register_hooks(p2p: &P2p) {
    // Stops syncing closed documents.
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if let Some(id) = event.doc.shared {
            event.editor.with_session(|session| session.close(id));
        }
        Ok(())
    });

    // Queues local edits to shared buffers.
    let local_changes = p2p.local_changes_tx.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        // TODO: ghost transactions still change the buffer (#44).
        if event.ghost_transaction {
            return Ok(());
        }
        if let Some(id) = event.doc.shared {
            let _ = local_changes.send((id, event.changes.clone()));
        }
        Ok(())
    });
}

impl Editor {
    /// Gives the session as input and afterwards drains the output queue.
    /// Returns None if we are not in a session.
    pub fn with_session<T>(&mut self, f: impl FnOnce(&mut Session) -> T) -> Option<T> {
        let result = f(self.p2p.session()?);
        self.drain_session_queue();
        Some(result)
    }

    pub(crate) fn drain_session_queue(&mut self) {
        while let Some(transmit) = self.p2p.session.as_mut().and_then(Session::poll_transmit) {
            self.p2p.transmit(transmit);
        }
        while let Some(event) = self.p2p.session.as_mut().and_then(Session::poll_event) {
            self.apply_session_event(event);
        }
    }

    pub fn handle_p2p_event(&mut self, event: net::Event) {
        match event {
            net::Event::Received(channel, message) => {
                self.with_session(|session| session.receive(channel, &message));
            }
            net::Event::Exit(Channel::Session, reason) => {
                // The transport has already left every topic.
                self.drop_session();
                self.set_error(format!("lost session: {reason}"));
            }
            net::Event::Exit(channel, reason) => {
                self.with_session(|session| session.transport_exit(channel, reason));
            }
        }
    }

    /// Starts a session, unless we are already in one.
    pub fn create_session(&mut self) {
        if self.p2p.session.is_none() {
            self.p2p.session = Some(Session::new(self.p2p.service.id(), rand::random()));
        }
    }

    /// Starts syncing with the session we have just joined.
    pub fn join_session(&mut self) {
        self.p2p.session = Some(Session::joined(self.p2p.service.id(), rand::random()));
        self.drain_session_queue();
    }

    pub fn leave_session(&mut self) -> Result<()> {
        self.with_session(Session::shutdown)
            .context("not in a session")?;
        self.drop_session();
        Ok(())
    }

    fn drop_session(&mut self) {
        self.p2p.session = None;
        // Documents are no longer synced.
        for doc in self.documents_mut() {
            doc.shared = None;
        }
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

        let id = self
            .with_session(|session| session.share(path, &text))
            .context("not in a session")?;
        doc_mut!(self, &doc_id).shared = Some(id);
        Ok(id)
    }

    pub fn open_shared_document(&mut self, id: SharedId, action: Action) -> Result<()> {
        if let Some(doc_id) = self.shared_document(id).map(Document::id) {
            self.switch(doc_id, action);
            return Ok(());
        }

        self.with_session(|session| session.open(id))
            .context("not in a session")??;
        let doc_id = self.new_file(action);
        doc_mut!(self, &doc_id).shared = Some(id);
        Ok(())
    }

    fn apply_session_event(&mut self, event: Event) {
        match event {
            Event::Changes { id, changes } => {
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
}
