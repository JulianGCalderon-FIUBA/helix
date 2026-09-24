//! Collaboration session layer, on top of the networking layer.

use std::path::PathBuf;

use anyhow::{ensure, Result};
use helix_core::Rope;
use helix_event::register_hook;

use super::{
    crdt::{RemoteOperation, Replica},
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

    pub fn to_message(&self, text: &Rope) -> Message {
        Message::Share {
            id: self.id,
            owner: self.owner,
            path: self.path.clone(),
            text: text.to_string(),
            replica: self.replica.encode(),
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

        let id = shared.id;
        for op in shared.replica.from_local(event.changes) {
            p2p.broadcast(wire::encode(&Message::Edit { id, op }));
        }

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
        self.p2p
            .broadcast(wire::encode(&shared.to_message(doc.text())));
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
                self.set_status(format!("connected with {}", peer.fmt_short()));

                // Offer every shared buffer to late joiners.
                for doc in self.documents() {
                    if let Some(shared) = &doc.shared {
                        self.p2p
                            .broadcast(wire::encode(&shared.to_message(doc.text())));
                    }
                }
            }
            Event::Received(bytes) => match wire::decode(&bytes) {
                Ok(Message::Share {
                    id,
                    owner,
                    path,
                    text,
                    replica,
                }) => self.open_shared(id, owner, path, &text, &replica),
                Ok(Message::Edit { id, op }) => self.apply_remote_operation(id, &op),
                Err(err) => self.set_error(format!("bad message: {err:#}")),
            },
            Event::Quit(reason) => {
                self.unshare_all_documents();
                self.set_error(reason);
            }
            Event::Error(err) => self.set_error(err),
        }
    }

    /// Opens our own copy of a document another peer shared.
    fn open_shared(
        &mut self,
        id: SharedId,
        owner: EndpointId,
        path: Option<PathBuf>,
        text: &str,
        replica: &[u8],
    ) {
        // We ignore known shared documents.
        if self.documents().any(|doc| doc.shared_id() == Some(id)) {
            return;
        }

        let replica = match Replica::decode(replica) {
            Ok(replica) => replica,
            Err(err) => {
                self.set_error(format!("failed to join shared buffer: {err:#}"));
                return;
            }
        };

        let status = match &path {
            Some(path) => format!("{} shared {}", owner.fmt_short(), path.display()),
            None => format!("{} shared a buffer ({})", owner.fmt_short(), id.fmt_short()),
        };

        let doc_id = self.new_file_from_string(Action::Load, text);
        let doc = self
            .documents
            .get_mut(&doc_id)
            .expect("document should exist");
        doc.shared = Some(Shared {
            id,
            owner,
            path,
            replica,
        });

        self.set_status(status);
    }

    /// Applies another peer's edit to our copy of the document.
    fn apply_remote_operation(&mut self, id: SharedId, op: &RemoteOperation) {
        let view_id = self
            .tree
            .traverse()
            .find(|(_, view)| {
                self.documents.get(&view.doc).and_then(Document::shared_id) == Some(id)
            })
            .map_or(self.tree.focus, |(view_id, _)| view_id);

        let Some(doc) = self
            .documents
            .values_mut()
            .find(|doc| doc.shared_id() == Some(id))
        else {
            return;
        };

        doc.ensure_view_init(view_id);

        // Taken while applying, so the hook sees an unshared document and
        // doesn't broadcast the remote edit back.
        let Some(mut shared) = doc.shared.take() else {
            return;
        };
        if let Some(transaction) = shared.replica.from_remote(doc.text(), op) {
            doc.apply(&transaction, view_id);
        }
        doc.shared = Some(shared);
    }
}
