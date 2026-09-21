use std::path::PathBuf;

use anyhow::Result;
use helix_core::crdt::{EndpointId, RemoteOperation, SharedId};
use iroh::EndpointAddr;
use iroh_gossip::TopicId;
use iroh_tickets::{ParseError, Ticket};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Share {
        id: SharedId,
        // Whoever sends a Share is not necessarily the one who owns it.
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

/// Everything needed to join a session: which swarm, and one member to reach it through.
///
/// The topic is random, so the ticket doubles as the invitation.
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
