use std::path::PathBuf;

use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{crdt::RemoteOperation, net::EndpointId};

/// Identifies a single document across all peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SharedId(u64);

impl SharedId {
    pub fn random() -> Self {
        Self(rand::random())
    }

    pub fn fmt_short(&self) -> String {
        format!("{:08x}", self.0 as u32)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Share {
        id: SharedId,
        owner: EndpointId,
        path: Option<PathBuf>,
        text: String,
        replica: Vec<u8>,
    },
    Edit {
        id: SharedId,
        op: RemoteOperation,
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
