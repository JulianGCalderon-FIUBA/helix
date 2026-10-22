//! The messages peers exchange, and how they turn into bytes.

use std::{fmt::Display, path::PathBuf};

use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

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
/// Nonces let requests be sent again.
///
/// Snapshots don't go over gossip, but between the owner and the peer
/// opening the document, so they need no nonce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
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
    SnapshotRequest {
        id: SharedId,
    },
    Snapshot {
        replica: Vec<u8>,
    },
}

pub fn encode(message: &Message) -> Bytes {
    postcard::to_stdvec(message)
        .expect("message should serialize")
        .into()
}

pub fn decode(body: &[u8]) -> Result<Message> {
    Ok(postcard::from_bytes(body)?)
}
