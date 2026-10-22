//! Glue between the editor and the helix-p2p session.

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
    /// Local changes to shared buffers cannot be applied directly by the hook.
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

    pub fn session(&mut self) -> &mut Session {
        // The session must be up to date before taking any more inputs.
        while let Ok((id, changes)) = self.local_changes.try_recv() {
            self.session.local_change(id, &changes);
        }
        &mut self.session
    }

    pub(crate) fn local_change(&mut self, id: SharedId, changes: ChangeSet) {
        self.session.local_change(id, &changes);
        self.session();
    }

    pub fn documents(&self) -> &HashMap<SharedId, SharedMeta> {
        self.session.documents()
    }

    fn transmit(&self, transmit: Transmit) {
        match transmit {
            Transmit::Subscribe(topic, bootstrap) => self.service.subscribe(topic, bootstrap),
            Transmit::Unsubscribe(topic) => self.service.unsubscribe(topic),
            Transmit::Broadcast(channel, message) => self.service.broadcast(channel, message),
            Transmit::Request(peer, message) => self.service.request(peer, message),
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
    pub fn with_session<T>(&mut self, f: impl FnOnce(&mut Session) -> T) -> T {
        let result = f(self.p2p.session());
        self.drain_session_queue();
        result
    }

    pub(crate) fn drain_session_queue(&mut self) {
        while let Some(transmit) = self.p2p.session.poll_transmit() {
            self.p2p.transmit(transmit);
        }
        while let Some(event) = self.p2p.session.poll_event() {
            self.apply_session_event(event);
        }
    }

    pub fn handle_p2p_event(&mut self, event: net::Event) {
        match event {
            net::Event::Received(channel, message) => {
                self.with_session(|session| session.receive(channel, &message))
            }
            net::Event::Exit(channel, reason) => {
                self.with_session(|session| session.transport_exit(channel, reason))
            }
            net::Event::Request(request, reply) => {
                let _ = reply.send(self.p2p.session().respond(&request));
            }
            net::Event::Response(request, response) => {
                self.with_session(|session| session.receive_response(&request, response))
            }
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

        let id = self.with_session(|session| session.share(path, &text));
        doc_mut!(self, &doc_id).shared = Some(id);
        Ok(id)
    }

    pub fn open_shared_document(&mut self, id: SharedId, action: Action) -> Result<()> {
        if let Some(doc_id) = self.shared_document(id).map(Document::id) {
            self.switch(doc_id, action);
            return Ok(());
        }

        self.with_session(|session| session.open(id))?;
        let doc_id = self.new_file(action);
        doc_mut!(self, &doc_id).shared = Some(id);
        Ok(())
    }

    pub fn join_session(&mut self) {
        self.with_session(|session| session.join());
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
