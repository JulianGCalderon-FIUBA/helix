//! The messages peers exchange, and how they turn into bytes.
//!
//! Encoded with postcard, a compact binary format. Every peer runs this same
//! code, so there is no versioning.

use std::path::PathBuf;

use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{crdt::RemoteOperation, net::EndpointId};

/// Identifies a single document across all peers.
///
/// Documents already have a DocumentId, but that one is only unique within a
/// single editor, so a peer's ids mean nothing to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SharedId(u64);

impl SharedId {
    /// Minted by whoever shares the document. Random, so peers need no
    /// coordination to pick one, and 64 bits make collisions unlikely.
    pub fn random() -> Self {
        Self(rand::random())
    }

    /// For display only, so half the bits are enough.
    pub fn fmt_short(&self) -> String {
        format!("{:08x}", self.0 as u32)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Offers a document to the session. Enough to open it from scratch.
    Share {
        id: SharedId,
        /// Carried explicitly because the sender is not necessarily the
        /// owner: every peer re-shares what it has to newcomers.
        owner: EndpointId,
        /// Relative to the owner's workspace. `None` for scratch buffers.
        path: Option<PathBuf>,
        /// The document's text. The replica only tracks the CRDT's metadata,
        /// not the text itself, so both are needed.
        text: String,
        /// The sender's encoded replica, which the receiver forks its own
        /// from, so that both start from the same CRDT state.
        replica: Vec<u8>,
    },
    /// One edit to a shared document.
    Edit {
        /// Which document the edit is for, as a peer can share several.
        id: SharedId,
        op: RemoteOperation,
    },
}

/// Returns Bytes, which is what net broadcasts.
///
/// Postcard only fails on types it cannot represent, and none of ours are, so
/// this does not return a Result.
pub fn encode(message: &Message) -> Bytes {
    postcard::to_stdvec(message)
        .expect("message should serialize")
        .into()
}

/// Can fail, unlike encode: the bytes come from the network.
pub fn decode(body: &[u8]) -> Result<Message> {
    Ok(postcard::from_bytes(body)?)
}
