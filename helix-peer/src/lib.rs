//! Messages to hx-peer, one JSON object per line.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// `:session-new` would yank the ticket to the system clipboard.
    Host,
    /// Only the picker opens a shared document.
    Open {
        id: String,
    },
    Keys {
        keys: String,
    },
    State,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(serde_json::Value),
    Error(String),
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// The focused buffer's id, if it is shared.
    pub focus: Option<String>,
    pub buffers: Vec<Buffer>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Buffer {
    pub id: String,
    pub text: String,
    /// None while opening.
    pub replica: Option<String>,
    pub ops: usize,
}
