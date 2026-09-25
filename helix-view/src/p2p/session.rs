//! Collaboration session layer, on top of the networking layer.

use std::path::PathBuf;

use anyhow::{ensure, Result};
use helix_core::Rope;
use helix_event::register_hook;

use super::{
    crdt::Replica,
    net::{EndpointId, Event, Service},
    wire::{self, Message, SharedId},
};
use crate::{editor::Action, events::DocumentDidChange, Document, DocumentId, Editor};

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

    pub fn to_message(&self) -> Message {
        Message::Share {
            id: self.id,
            owner: self.owner,
            path: self.path.clone(),
            replica: self.replica.encode(),
        }
    }

    pub fn label(&self) -> String {
        match &self.path {
            Some(path) => format!("{} ({})", path.display(), self.id.fmt_short()),
            None => format!("buffer ({})", self.id.fmt_short()),
        }
    }
}

/// Broadcasts local edits to shared documents.
pub fn register_hooks(p2p: Service) {
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.ghost_transaction {
            return Ok(());
        }
        let Some(shared) = event.doc.shared.as_mut() else {
            return Ok(());
        };

        let update = shared.replica.from_local(event.changes);
        p2p.broadcast(wire::encode(&Message::Edit {
            id: shared.id,
            update,
        }));

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
        self.p2p.broadcast(wire::encode(&shared.to_message()));
        doc.shared = Some(shared);
        Ok(())
    }

    pub fn leave_session(&mut self) {
        self.p2p.leave();
        self.unshare_all_documents();
    }

    /// Unshares all documents, but keeps the buffers.
    fn unshare_all_documents(&mut self) {
        for doc in self.documents_mut() {
            doc.shared = None;
        }
    }

    /// Called by the application for every p2p event.
    pub fn handle_p2p_event(&mut self, event: Event) {
        match event {
            Event::NeighborUp(peer) => {
                // Offer every shared buffer to late joiners.
                for doc in self.documents() {
                    if let Some(shared) = &doc.shared {
                        log::debug!("offering {} to {}", shared.label(), peer.fmt_short());
                        self.p2p.broadcast(wire::encode(&shared.to_message()));
                    }
                }
            }
            Event::Received(bytes) => match wire::decode(&bytes) {
                Ok(Message::Share {
                    id,
                    owner,
                    path,
                    replica,
                }) => self.open_shared(id, owner, path, &replica),
                Ok(Message::Edit { id, update }) => self.apply_remote_update(id, &update),
                Err(err) => log::warn!("dropping malformed message: {err:#}"),
            },
            Event::Quit(reason) => {
                self.unshare_all_documents();
                self.set_error(format!("quit session: {reason}"));
            }
        }
    }

    /// Opens our own copy of a document another peer shared.
    fn open_shared(
        &mut self,
        id: SharedId,
        owner: EndpointId,
        path: Option<PathBuf>,
        replica: &[u8],
    ) {
        if self.documents().any(|doc| doc.shared_id() == Some(id)) {
            log::trace!("ignoring known shared buffer {}", id.fmt_short());
            return;
        }

        let replica = match Replica::decode(replica) {
            Ok(replica) => replica,
            Err(err) => {
                log::error!(
                    "failed to decode replica for buffer {}: {err:#}",
                    id.fmt_short()
                );
                return;
            }
        };

        let doc_id = self.new_file_from_string(Action::Load, &replica.text());
        let doc = self
            .documents
            .get_mut(&doc_id)
            .expect("document should exist");
        let shared = Shared {
            id,
            owner,
            path,
            replica,
        };
        log::info!("{} shared {}", owner.fmt_short(), shared.label());
        doc.shared = Some(shared);
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
        match shared.replica.from_remote(doc.text(), update) {
            Ok(Some(transaction)) => {
                doc.apply(&transaction, view_id);
            }
            Ok(None) => log::trace!("pending edit for {}", id.fmt_short()),
            Err(err) => log::warn!("dropping edit for {}: {err:#}", id.fmt_short()),
        }
        doc.shared = Some(shared);
    }
}
