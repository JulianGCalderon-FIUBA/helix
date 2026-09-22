use std::path::PathBuf;

use anyhow::Result;
use helix_core::crdt::{EndpointId, RemoteOperation, SharedId};
use iroh::EndpointAddr;
use iroh_gossip::TopicId;
use iroh_tickets::{ParseError, Ticket};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// A file shared in the session, and the topic its edits travel on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announcement {
    pub id: SharedId,
    pub topic: TopicId,
    pub owner: EndpointId,
    pub path: Option<PathBuf>,
}

impl Announcement {
    /// Announce a file on a topic of its own. The topic is random so that
    /// nobody outside the session can guess it.
    pub fn new(id: SharedId, owner: EndpointId, path: Option<PathBuf>) -> Self {
        Self {
            id,
            topic: TopicId::from_bytes(rand::random()),
            owner,
            path,
        }
    }
}

/// Sent on the session topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionMessage {
    Announce(Announcement),
}

/// Sent on a file's topic, which already identifies the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileMessage {
    Snapshot { text: String, replica: Vec<u8> },
    Edit(RemoteOperation),
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(message)?)
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(postcard::from_bytes(body)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTicket {
    pub topic: TopicId,
    pub addr: EndpointAddr,
}

impl Ticket for SessionTicket {
    const KIND: &'static str = "helix";

    fn encode_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("ticket should serialize")
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}
