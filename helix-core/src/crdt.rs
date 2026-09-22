//! Bridges Helix's [`ChangeSet`]s to [`cola`], a text CRDT.
//!
//! Cola counts in whatever unit you decide and never checks. Helix indexes
//! chars, so every `usize` crossing this boundary is a char index.

use std::path::{Path, PathBuf};

use anyhow::Result;
use cola::{EncodedReplica, Insertion, ReplicaId};
pub use iroh_base::EndpointId;
use serde::{Deserialize, Serialize};

use crate::{transaction::Operation, ChangeSet, Rope, Transaction};

/// Identifies a single document across all peers.
///
/// As many bytes as a gossip topic id, so that the document's topic can
/// simply be its id. Being random, nobody outside the session can guess it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SharedId([u8; 32]);

impl SharedId {
    pub fn random() -> Self {
        Self(rand::random())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn fmt_short(&self) -> String {
        self.0[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteOperation {
    Insert { insertion: Insertion, text: String },
    Delete(cola::Deletion),
}

pub fn replica_id() -> ReplicaId {
    // Cola panics on a zero id.
    rand::random_range(1..=ReplicaId::MAX)
}

pub struct Replica {
    shared_id: SharedId,
    owner: EndpointId,
    path: Option<PathBuf>,
    replica: cola::Replica,
}

impl Replica {
    pub fn new(
        replica_id: ReplicaId,
        owner: EndpointId,
        path: Option<PathBuf>,
        text: &Rope,
    ) -> Self {
        Self {
            shared_id: SharedId::random(),
            owner,
            path,
            replica: cola::Replica::new(replica_id, text.len_chars()),
        }
    }

    pub fn shared_id(&self) -> SharedId {
        self.shared_id
    }

    pub fn owner(&self) -> EndpointId {
        self.owner
    }

    /// The path relative to the owner's workspace.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn decode(
        shared_id: SharedId,
        owner: EndpointId,
        path: Option<PathBuf>,
        replica_id: ReplicaId,
        encoded: &[u8],
    ) -> Result<Self> {
        let encoded = EncodedReplica::from_bytes(encoded);
        Ok(Self {
            shared_id,
            owner,
            path,
            replica: cola::Replica::decode(replica_id, &encoded)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.replica.encode().as_bytes().to_vec()
    }

    /// Translate local transactions to CRDT operations.
    pub fn from_local(&mut self, changes: &ChangeSet) -> Vec<RemoteOperation> {
        let mut ops = Vec::new();
        let mut pos = 0;

        for op in changes.changes() {
            match op {
                Operation::Retain(n) => pos += n,
                Operation::Insert(text) => {
                    let len = text.chars().count();
                    ops.push(RemoteOperation::Insert {
                        insertion: self.replica.inserted(pos, len),
                        text: text.to_string(),
                    });
                    pos += len;
                }
                Operation::Delete(n) => {
                    ops.push(RemoteOperation::Delete(self.replica.deleted(pos..pos + n)));
                }
            }
        }

        ops
    }

    /// Translate CRDT operations to local transactions.
    ///
    /// `None` means Cola backlogged the op, not that it failed.
    pub fn from_remote(&mut self, text: &Rope, op: &RemoteOperation) -> Option<Transaction> {
        let transaction = match op {
            RemoteOperation::Insert { insertion, text: s } => {
                let at = self.replica.integrate_insertion(insertion)?;
                Transaction::change(text, [(at, at, Some(s.as_str().into()))].into_iter())
            }
            RemoteOperation::Delete(deletion) => {
                let ranges = self.replica.integrate_deletion(deletion);
                if ranges.is_empty() {
                    return None;
                }
                Transaction::delete(
                    text,
                    ranges.into_iter().map(|range| (range.start, range.end)),
                )
            }
        };
        Some(transaction.as_remote())
    }
}
