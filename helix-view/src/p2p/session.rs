//! What it means to share documents in a session, on top of [`net`](super::net).

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

/// A document shared in the session, and who shared it.
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

    /// The Share that lets a peer open this document, as of `text`.
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
///
/// Takes its own handle to the service, since hooks cannot reach the editor.
pub fn register_hooks(p2p: Service) {
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        // Remote edits came from the session already.
        if event.ghost_transaction || event.remote_transaction {
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
    pub fn share(&mut self, doc_id: DocumentId) -> Result<()> {
        let owner = self.p2p.id();
        let doc = self
            .documents
            .get_mut(&doc_id)
            .expect("document should exist");
        ensure!(doc.shared.is_none(), "buffer is already shared");

        // Peers see the path relative to the workspace,
        // or in full when the file is outside of it.
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
        // Leaving drops every peer, so nothing is shared any more.
        for doc in self.documents_mut() {
            doc.shared = None;
        }
        self.p2p.close();
    }

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
                Ok(Message::Edit { id, op }) => self.apply_remote(id, &op),
                Err(err) => self.set_error(format!("bad message: {err:#}")),
            },
            Event::Error(err) => self.set_error(err),
        }
    }

    fn open_shared(
        &mut self,
        id: SharedId,
        owner: EndpointId,
        path: Option<PathBuf>,
        text: &str,
        replica: &[u8],
    ) {
        // Everyone re-shares their buffers to each newcomer, so we see the
        // same Share many times.
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
        self.documents.get_mut(&doc_id).unwrap().shared = Some(Shared {
            id,
            owner,
            path,
            replica,
        });

        self.set_status(status);
    }

    fn apply_remote(&mut self, id: SharedId, op: &RemoteOperation) {
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

        // apply reads the document's selection for view_id, which a
        // buffer that view has never displayed does not have yet.
        doc.ensure_view_init(view_id);

        // Taken out so the replica can change while we read the text.
        let Some(mut shared) = doc.shared.take() else {
            return;
        };
        if let Some(transaction) = shared.replica.from_remote(doc.text(), op) {
            doc.apply(&transaction, view_id);
        }
        doc.shared = Some(shared);
    }
}
