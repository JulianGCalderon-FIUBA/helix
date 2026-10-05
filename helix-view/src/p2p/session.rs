//! Collaboration session layer, on top of the networking layer.

use std::path::PathBuf;

use anyhow::{bail, ensure, Result};
use helix_core::Transaction;
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

/// What the session knows about a shared document, opened or not.
#[derive(Clone)]
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

/// Our copy of a shared document.
pub struct Shared {
    pub meta: SharedMeta,
    pub replica: Replica,
    /// Whether we asked for a snapshot, or need none because we own it.
    snapshot_requested: bool,
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
            Topic::Other(shared.meta.id.topic()),
            wire::encode(&Message::Edit {
                id: shared.meta.id,
                update,
            }),
        );

        Ok(())
    });
}

impl Editor {
    /// Our open copy of a shared document.
    pub fn shared_document(&self, id: SharedId) -> Option<&Document> {
        self.documents().find(|doc| doc.shared_id() == Some(id))
    }

    pub fn shared_document_mut(&mut self, id: SharedId) -> Option<&mut Document> {
        self.documents_mut().find(|doc| doc.shared_id() == Some(id))
    }

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

        let meta = SharedMeta {
            id: SharedId::random(),
            owner,
            path,
        };
        log::info!("sharing {}", meta.label());
        self.p2p.subscribe(meta.id.topic(), Vec::new());
        doc.shared = Some(Shared {
            meta: meta.clone(),
            replica: Replica::new(doc.text()),
            snapshot_requested: true,
        });
        self.announce(&meta);
        self.shared_files.insert(meta.id, meta);
        Ok(())
    }

    /// Tells the session about a document, but not what it contains.
    fn announce(&self, meta: &SharedMeta) {
        let message = Message::Share {
            id: meta.id,
            owner: meta.owner,
            path: meta.path.clone(),
        };
        self.p2p.broadcast(Topic::Session, wire::encode(&message));
    }

    /// Opens our copy of a shared document, or switches to it if open.
    ///
    /// The buffer starts empty, and fills in once the owner answers
    /// with a snapshot.
    pub fn open_shared(&mut self, id: SharedId, action: Action) -> Result<()> {
        if let Some(doc_id) = self.shared_document(id).map(Document::id) {
            self.switch(doc_id, action);
            return Ok(());
        }
        let Some(meta) = self.shared_files.get(&id) else {
            bail!("{} is no longer shared", id.fmt_short());
        };

        let shared = Shared {
            meta: meta.clone(),
            replica: Replica::empty(),
            snapshot_requested: false,
        };
        log::info!("opening {}", shared.meta.label());
        // The owner is the one peer we know is in the topic.
        self.p2p.subscribe(id.topic(), vec![shared.meta.owner]);

        let doc_id = self.new_file(action);
        doc_mut!(self, &doc_id).shared = Some(shared);
        Ok(())
    }

    pub fn leave_session(&mut self) {
        // Unshare first, so the broadcasts go out before the node leaves.
        self.unshare_all_documents();
        self.p2p.leave();
    }

    /// Leaves a closed document's topic. If the document is ours,
    /// nobody can open it anymore, so we unshare it.
    fn close_shared(&mut self, shared: Shared) {
        log::info!("closing {}", shared.meta.label());
        if shared.meta.owner == self.p2p.id() {
            self.unshare(shared.meta.id);
        } else {
            self.p2p.unsubscribe(shared.meta.id.topic());
        }
    }

    /// Stops sharing a document. Our copy, if open, stays as a regular
    /// buffer, and is returned.
    ///
    /// If the document is ours, peers are told to unshare it as well.
    fn unshare(&mut self, id: SharedId) -> Option<Shared> {
        let meta = self.shared_files.remove(&id);
        let shared = self
            .shared_document_mut(id)
            .and_then(|doc| doc.shared.take());
        self.p2p.unsubscribe(id.topic());

        if meta.is_some_and(|meta| meta.owner == self.p2p.id()) {
            let message = Message::Unshare { id };
            self.p2p.broadcast(Topic::Session, wire::encode(&message));
        }
        shared
    }

    /// Unshares all documents, but keeps the buffers.
    fn unshare_all_documents(&mut self) {
        let ids: Vec<_> = self.shared_files.keys().copied().collect();
        for id in ids {
            self.unshare(id);
        }
    }

    /// Called by the application for every p2p event.
    pub fn handle_p2p_event(&mut self, event: Event) {
        match event {
            Event::NeighborUp(Topic::Session, peer) => {
                // Announce every document we know of to late joiners, not only
                // ours: the owner only notices a joiner it connects to directly.
                for meta in self.shared_files.values() {
                    log::debug!("announcing {} to {}", meta.label(), peer.fmt_short());
                    self.announce(meta);
                }
            }
            Event::NeighborUp(Topic::Other(topic), _) => {
                self.request_snapshot(SharedId::from(topic))
            }
            Event::Received(_, bytes) => match wire::decode(&bytes) {
                Ok(Message::Share { id, owner, path }) => {
                    let meta = SharedMeta { id, owner, path };
                    if self.shared_files.insert(id, meta).is_none() {
                        log::info!("{} shared {}", owner.fmt_short(), id.fmt_short());
                    }
                }
                Ok(Message::Unshare { id }) => {
                    if let Some(shared) = self.unshare(id) {
                        self.set_status(format!("owner stopped sharing {}", shared.meta.label()));
                    }
                }
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
                if let Some(shared) = self.unshare(SharedId::from(topic)) {
                    self.set_error(format!("stopped sharing {}: {reason}", shared.meta.label()));
                }
            }
        }
    }

    /// Asks for a snapshot of a document we opened, the first time we
    /// connect to someone in its topic. Broadcasting earlier reaches nobody.
    fn request_snapshot(&mut self, id: SharedId) {
        let Some(shared) = self
            .shared_document_mut(id)
            .and_then(|doc| doc.shared.as_mut())
        else {
            return;
        };
        if std::mem::replace(&mut shared.snapshot_requested, true) {
            return;
        }

        log::debug!("requesting snapshot of {}", id.fmt_short());
        let message = Message::SnapshotRequest { id };
        self.p2p
            .broadcast(Topic::Other(id.topic()), wire::encode(&message));
    }

    /// Answers a snapshot request, if the document is ours.
    ///
    /// Only the owner answers, so that a request gets a single snapshot.
    fn send_snapshot(&self, id: SharedId) {
        let Some(shared) = self
            .shared_document(id)
            .and_then(|doc| doc.shared.as_ref())
            .filter(|shared| shared.meta.owner == self.p2p.id())
        else {
            return;
        };

        log::debug!("sending snapshot of {}", shared.meta.label());
        let message = Message::Snapshot {
            id,
            replica: shared.replica.encode(),
        };
        self.p2p
            .broadcast(Topic::Other(id.topic()), wire::encode(&message));
    }

    /// Applies another peer's edit to our copy of the document.
    fn apply_remote_update(&mut self, id: SharedId, update: &[u8]) {
        let Some(doc_id) = self.shared_document(id).map(Document::id) else {
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
