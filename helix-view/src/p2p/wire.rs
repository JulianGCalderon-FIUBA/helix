use std::path::PathBuf;

use super::crdt::{EndpointId, RemoteOperation, SharedId};
use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

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
