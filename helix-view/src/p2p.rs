//! Glue between the editor and the helix-p2p session.
//!
//! The editor thread drives the session: it passes in user actions, local
//! edits and whatever the transport receives, then drains what comes out.

use std::collections::HashMap;

use anyhow::{ensure, Result};
use helix_core::{ChangeSet, Transaction};
use helix_event::register_hook;
use helix_p2p::{
    net::{self, Service},
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
    session: Session,
    pub(crate) incoming: UnboundedReceiverStream<net::Event>,
    /// Local edits to shared buffers, which the hook can't hand to the
    /// session directly because it only sees the document.
    local_changes_tx: UnboundedSender<(SharedId, ChangeSet)>,
    pub(crate) local_changes: UnboundedReceiver<(SharedId, ChangeSet)>,
}

impl P2p {
    pub fn new() -> Self {
        let (service, incoming) = Service::new();
        let (local_changes_tx, local_changes) = unbounded_channel();
        Self {
            session: Session::new(service.id(), rand::random()),
            service,
            incoming,
            local_changes_tx,
            local_changes,
        }
    }

    /// The session, once it has seen every local edit.
    ///
    /// Remote edits are diffed against the replicas, so the session must
    /// have every edit to the buffers before it takes any other input.
    pub fn session(&mut self) -> &mut Session {
        while let Ok((id, changes)) = self.local_changes.try_recv() {
            self.session.local_change(id, &changes);
        }
        &mut self.session
    }

    /// Feeds a local edit taken off the queue, and every one after it.
    pub(crate) fn local_change(&mut self, id: SharedId, changes: ChangeSet) {
        self.session.local_change(id, &changes);
        self.session();
    }

    /// Every document announced to the session, opened or not.
    pub fn documents(&self) -> &HashMap<SharedId, SharedMeta> {
        self.session.documents()
    }

    /// Hands what the session wants sent to the transport.
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

/// Queues local edits to shared buffers, and stops syncing closed ones.
pub fn register_hooks(p2p: &P2p) {
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if let Some(id) = event.doc.shared {
            event.editor.p2p_input(|session| session.close(id));
        }
        Ok(())
    });

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
    /// Gives the session an input, then carries out what comes out of it.
    pub fn p2p_input<T>(&mut self, input: impl FnOnce(&mut Session) -> T) -> T {
        let result = input(self.p2p.session());
        self.drain_p2p();
        result
    }

    /// Carries out everything the session has queued.
    ///
    /// Runs right after each input, so that remote changes reach the
    /// buffer before anything else can edit it.
    pub(crate) fn drain_p2p(&mut self) {
        while let Some(transmit) = self.p2p.session.poll_transmit() {
            self.p2p.transmit(transmit);
        }
        while let Some(event) = self.p2p.session.poll_event() {
            self.apply_p2p_event(event);
        }
    }

    /// Called by the application for every transport event.
    pub fn handle_p2p_event(&mut self, event: net::Event) {
        match event {
            net::Event::Received(channel, message) => {
                self.p2p_input(|session| session.receive(channel, &message))
            }
            net::Event::Exit(channel, reason) => {
                self.p2p_input(|session| session.transport_exit(channel, reason))
            }
        }
    }

    /// Our open copy of a shared document.
    pub fn shared_document(&self, id: SharedId) -> Option<&Document> {
        self.documents().find(|doc| doc.shared == Some(id))
    }

    /// Shares a document with the session.
    pub fn share_document(&mut self, doc_id: DocumentId) -> Result<SharedId> {
        let doc = doc!(self, &doc_id);
        ensure!(doc.shared.is_none(), "buffer is already shared");

        let path = doc.path().map(|path| {
            let (workspace, _) = helix_loader::find_workspace();
            path.strip_prefix(&workspace).unwrap_or(path).to_path_buf()
        });
        let text = doc.text().clone();

        let id = self.p2p_input(|session| session.share(path, &text));
        doc_mut!(self, &doc_id).shared = Some(id);
        Ok(id)
    }

    /// Opens our copy of a shared document, or switches to it if open.
    ///
    /// The buffer starts empty, and fills in once the owner answers
    /// with a snapshot.
    pub fn open_shared_document(&mut self, id: SharedId, action: Action) -> Result<()> {
        if let Some(doc_id) = self.shared_document(id).map(Document::id) {
            self.switch(doc_id, action);
            return Ok(());
        }

        self.p2p_input(|session| session.open(id))?;
        let doc_id = self.new_file(action);
        doc_mut!(self, &doc_id).shared = Some(id);
        Ok(())
    }

    /// Starts over in the session the transport has just joined.
    pub fn join_session(&mut self) {
        // Documents from the old session are no longer synced.
        self.clear_shared_markers();
        self.p2p.session.request_documents();
        self.drain_p2p();
    }

    fn apply_p2p_event(&mut self, event: Event) {
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

    fn clear_shared_markers(&mut self) {
        for doc in self.documents_mut() {
            doc.shared = None;
        }
    }
}
