//! The messages peers exchange, and how they turn into bytes.

use std::{fmt::Display, path::PathBuf};

use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::net::{EndpointId, TopicId};

/// Identifies a single document across all peers.
///
/// It doubles as the id of the document's own topic, where its edits
/// travel, so that only the peers that opened it receive them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SharedId(TopicId);

impl SharedId {
    pub fn random() -> Self {
        Self(TopicId::from_bytes(rand::random()))
    }

    pub fn topic(&self) -> TopicId {
        self.0
    }

    pub fn fmt_short(&self) -> impl Display {
        self.0.fmt_short()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Sent on the session topic.
    Share {
        id: SharedId,
        owner: EndpointId,
        path: Option<PathBuf>,
    },
    /// Sent on the document's topic, by peers waiting for its content.
    SnapshotRequest { id: SharedId },
    /// Sent on the document's topic, by the owner, answering a request.
    Snapshot { id: SharedId, replica: Vec<u8> },
    /// Sent on the document's topic.
    Edit { id: SharedId, update: Vec<u8> },
}

pub fn encode(message: &Message) -> Bytes {
    postcard::to_stdvec(message)
        .expect("message should serialize")
        .into()
}

pub fn decode(body: &[u8]) -> Result<Message> {
    Ok(postcard::from_bytes(body)?)
}
