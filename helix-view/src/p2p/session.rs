use std::path::PathBuf;

use helix_core::Rope;

use super::{
    crdt::Replica,
    net::EndpointId,
    wire::{Message, SharedId},
};

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
