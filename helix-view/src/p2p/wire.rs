use std::path::PathBuf;

use super::crdt::{EndpointId, RemoteOperation, SharedId};
use anyhow::Result;
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

pub fn encode(message: &Message) -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(message)?)
}

pub fn decode(body: &[u8]) -> Result<Message> {
    Ok(postcard::from_bytes(body)?)
}
