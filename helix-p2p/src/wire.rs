//! The messages peers exchange, and how they turn into bytes.

use std::{fmt::Display, path::PathBuf};

use anyhow::Result;
use bytes::Bytes;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::net::{EndpointId, TopicId};

/// Identifies a single document across all peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SharedId(TopicId);

impl SharedId {
    pub fn topic(&self) -> TopicId {
        self.0
    }

    pub fn fmt_short(&self) -> impl Display {
        self.0.fmt_short()
    }
}

impl From<TopicId> for SharedId {
    fn from(topic: TopicId) -> Self {
        Self(topic)
    }
}

/// Messages on a document's topic are about that document.
///
/// Gossip drops a message whose content it has seen recently.
/// Nonces let the same message be sent again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipMessage {
    Share {
        id: SharedId,
        owner: EndpointId,
        path: Option<PathBuf>,
    },
    Unshare {
        id: SharedId,
    },
    DocumentsRequest {
        nonce: u64,
    },
    Edit {
        update: Vec<u8>,
    },
    /// The whole selection, so only the latest one matters.
    Selection {
        nonce: u64,
        peer: EndpointId,
        selection: Vec<u8>,
    },
}

/// Messages over a direct connection between two peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PeerMessage {
    SnapshotRequest {
        request: u64,
        id: SharedId,
    },
    Snapshot {
        request: u64,
        replica: Result<Vec<u8>, String>,
    },
}

pub fn encode(message: &impl Serialize) -> Bytes {
    postcard::to_stdvec(message)
        .expect("message should serialize")
        .into()
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(postcard::from_bytes(body)?)
}
