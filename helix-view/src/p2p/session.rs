//! Collaboration session layer, on top of the networking layer.

use std::path::PathBuf;

use anyhow::{bail, ensure, Result};
use helix_core::{Rope, Transaction};
use helix_event::register_hook;

use super::{
    crdt::Replica,
    net::{EndpointId, Event, Service, Topic},
    wire::{self, Message, SharedId},
};
use crate::{
    editor::Action,
    events::{DocumentDidChange, DocumentDidClose},
    Document, DocumentId, Editor,
};

/// A document someone announced to the session, which we may not have opened.
pub struct SharedFile {
    pub owner: EndpointId,
    /// The path relative to the owner's workspace.
    pub path: Option<PathBuf>,
}

/// A document shared in the session, with metadata.
pub struct Shared {
    pub id: SharedId,
    pub owner: EndpointId,
    /// The path relative to the owner's workspace.
    pub path: Option<PathBuf>,
    pub replica: Replica,
}

impl Shared {
    pub fn new(owner: EndpointId, path: Option<PathBuf>, text: &Rope) -> Self {
        Self {
            id: SharedId::random(),
            owner,
            path,
            replica: Replica::new(text),
        }
    }

    pub fn label(&self) -> String {
        match &self.path {
            Some(path) => format!("{} ({})", path.display(), self.id.fmt_short()),
            None => format!("buffer ({})", self.id.fmt_short()),
        }
    }
}

/// Broadcasts local edits to shared documents, and stops syncing closed ones.
pub fn register_hooks(p2p: Service) {
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if let Some(shared) = event.doc.shared.take() {
            event.editor.close_shared(shared);
        }
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.ghost_transaction {
            return Ok(());
        }
        let Some(shared) = event.doc.shared.as_mut() else {
            return Ok(());
        };

        let update = shared.replica.from_local(event.changes);
        p2p.broadcast(
            Topic::Other(shared.id.topic()),
            wire::encode(&Message::Edit {
                id: shared.id,
                update,
            }),
        );

        Ok(())
    });
}

impl Editor {
    /// Shares a document with the session.
    pub fn share_document(&mut self, doc_id: DocumentId) -> Result<()> {
        let owner = self.p2p.id();
        let doc = self
            .documents
            .get_mut(&doc_id)
            .expect("document should exist");
        ensure!(doc.shared.is_none(), "buffer is already shared");

        let path = doc.path().map(|path| {
            let (workspace, _) = helix_loader::find_workspace();
            path.strip_prefix(&workspace).unwrap_or(path).to_path_buf()
        });

        let shared = Shared::new(owner, path, doc.text());
        log::info!("sharing {}", shared.label());
        // Peers that open the document join its topic through us.
        self.p2p.subscribe(shared.id.topic(), Vec::new());
        let id = shared.id;
        let file = SharedFile {
            owner,
            path: shared.path.clone(),
        };
        doc.shared = Some(shared);
        self.announce(id, &file);
        self.shared_files.insert(id, file);
        Ok(())
    }

    /// Tells the session about a document, but not what it contains.
    fn announce(&self, id: SharedId, file: &SharedFile) {
        let message = Message::Share {
            id,
            owner: file.owner,
            path: file.path.clone(),
        };
        self.p2p.broadcast(Topic::Session, wire::encode(&message));
    }

    /// Opens our copy of a shared document, or switches to it if open.
    ///
    /// The buffer starts empty, and fills in once the owner answers
    /// with a snapshot.
    pub fn open_shared(&mut self, id: SharedId, action: Action) -> Result<()> {
        let open = self
            .documents()
            .find(|doc| doc.shared_id() == Some(id))
            .map(Document::id);
        if let Some(doc_id) = open {
            self.switch(doc_id, action);
            return Ok(());
        }
        let Some(file) = self.shared_files.get(&id) else {
            bail!("{} is no longer shared", id.fmt_short());
        };

        let shared = Shared {
            id,
            owner: file.owner,
            path: file.path.clone(),
            replica: Replica::empty(),
        };
        log::info!("opening {}", shared.label());
        // The owner is the one peer we know is in the topic. Once we
        // connect to someone there, we ask for a snapshot.
        self.p2p.subscribe(id.topic(), vec![shared.owner]);

        let doc_id = self.new_file(action);
        doc_mut!(self, &doc_id).shared = Some(shared);
        Ok(())
    }

    pub fn leave_session(&mut self) {
        // Leaving drops every topic, so peers would keep listing our
        // documents. The broadcasts go out before the node leaves.
        let me = self.p2p.id();
        for (id, file) in &self.shared_files {
            if file.owner == me {
                self.p2p
                    .broadcast(Topic::Session, wire::encode(&Message::Unshare { id: *id }));
            }
        }
        self.p2p.leave();
        self.unshare_all_documents();
    }

    /// Leaves a closed document's topic. If the document is ours,
    /// nobody can open it anymore, so we unshare it from everyone.
    fn close_shared(&mut self, shared: Shared) {
        log::info!("closing {}", shared.label());
        self.p2p.unsubscribe(shared.id.topic());
        if shared.owner == self.p2p.id() {
            self.shared_files.remove(&shared.id);
            let message = Message::Unshare { id: shared.id };
            self.p2p.broadcast(Topic::Session, wire::encode(&message));
        }
    }

    /// Unshares all documents, but keeps the buffers.
    fn unshare_all_documents(&mut self) {
        self.shared_files.clear();
        for doc in self.documents_mut() {
            doc.shared = None;
        }
    }

    /// Called by the application for every p2p event.
    pub fn handle_p2p_event(&mut self, event: Event) {
        match event {
            Event::NeighborUp(Topic::Session, peer) => {
                // Announce our documents to late joiners.
                let me = self.p2p.id();
                for (id, file) in &self.shared_files {
                    if file.owner == me {
                        log::debug!("announcing {} to {}", id.fmt_short(), peer.fmt_short());
                        self.announce(*id, file);
                    }
                }
            }
            Event::NeighborUp(Topic::Other(topic), _) => {
                // We are connected to the document's topic, so the request
                // reaches the owner. Asking on every new neighbor also
                // catches us up on edits we missed while disconnected.
                let me = self.p2p.id();
                if let Some(shared) = self.documents().find_map(|doc| {
                    let shared = doc.shared.as_ref()?;
                    (shared.id.topic() == topic && shared.owner != me).then_some(shared)
                }) {
                    log::debug!("requesting snapshot of {}", shared.label());
                    let message = Message::SnapshotRequest { id: shared.id };
                    self.p2p
                        .broadcast(Topic::Other(topic), wire::encode(&message));
                }
            }
            Event::Received(_, bytes) => match wire::decode(&bytes) {
                Ok(Message::Share { id, owner, path }) => {
                    let file = SharedFile { owner, path };
                    if self.shared_files.insert(id, file).is_none() {
                        log::info!("{} shared {}", owner.fmt_short(), id.fmt_short());
                    }
                }
                Ok(Message::Unshare { id }) => self.unshare(id),
                Ok(Message::SnapshotRequest { id }) => self.send_snapshot(id),
                // A snapshot is just a larger update, and importing what we
                // already have is a no-op.
                Ok(Message::Snapshot { id, replica }) => self.apply_remote_update(id, &replica),
                Ok(Message::Edit { id, update }) => self.apply_remote_update(id, &update),
                Err(err) => log::warn!("dropping malformed message: {err:#}"),
            },
            Event::Quit(Topic::Session, reason) => {
                self.unshare_all_documents();
                self.set_error(format!("quit session: {reason}"));
            }
            Event::Quit(Topic::Other(topic), reason) => {
                // Our copy no longer syncs, so it stops being shared.
                let shared = self
                    .documents_mut()
                    .find(|doc| doc.shared_id().map(|id| id.topic()) == Some(topic))
                    .and_then(|doc| doc.shared.take());
                if let Some(shared) = shared {
                    self.set_error(format!("stopped sharing {}: {reason}", shared.label()));
                }
            }
        }
    }

    /// Forgets a document its owner stopped sharing. Our copy, if any,
    /// stays open as a regular buffer.
    fn unshare(&mut self, id: SharedId) {
        self.shared_files.remove(&id);
        let shared = self
            .documents_mut()
            .find(|doc| doc.shared_id() == Some(id))
            .and_then(|doc| doc.shared.take());
        if let Some(shared) = shared {
            self.p2p.unsubscribe(id.topic());
            self.set_status(format!("owner stopped sharing {}", shared.label()));
        }
    }

    /// Answers a snapshot request, if the document is ours.
    ///
    /// Only the owner answers, so that a request gets a single snapshot.
    fn send_snapshot(&self, id: SharedId) {
        let me = self.p2p.id();
        let Some(shared) = self
            .documents()
            .filter_map(|doc| doc.shared.as_ref())
            .find(|shared| shared.id == id && shared.owner == me)
        else {
            return;
        };

        log::debug!("sending snapshot of {}", shared.label());
        let message = Message::Snapshot {
            id,
            replica: shared.replica.encode(),
        };
        self.p2p
            .broadcast(Topic::Other(id.topic()), wire::encode(&message));
    }

    /// Applies another peer's edit to our copy of the document.
    fn apply_remote_update(&mut self, id: SharedId, update: &[u8]) {
        let Some(doc_id) = self
            .documents()
            .find(|doc| doc.shared_id() == Some(id))
            .map(Document::id)
        else {
            log::debug!("dropping edit for unknown shared buffer {}", id.fmt_short());
            return;
        };

        let view_id = self.get_synced_view_id(doc_id);
        let doc = doc_mut!(self, &doc_id);

        // By taking out the shared document, we ensure that
        // the remote edit is not broadcasted again.
        let Some(mut shared) = doc.shared.take() else {
            return;
        };
        match shared.replica.from_remote(update) {
            Ok(changes) => {
                let transaction = Transaction::change(doc.text(), changes.into_iter());
                doc.apply(&transaction, view_id);
            }
            Err(err) => log::warn!("dropping edit for {}: {err:#}", id.fmt_short()),
        }
        doc.shared = Some(shared);
    }
}
