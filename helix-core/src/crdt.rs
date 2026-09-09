//! Bridges Helix's [`ChangeSet`]s to [`cola`], a text CRDT.
//!
//! Cola counts in whatever unit you decide and never checks. Helix indexes
//! chars, so every `usize` crossing this boundary is a char index.

use anyhow::Result;
use cola::{EncodedReplica, Insertion, ReplicaId};
use serde::{Deserialize, Serialize};

use crate::{transaction::Operation, ChangeSet, Rope, Transaction};

/// Addresses a document across the peers of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShareId(u64);

impl ShareId {
    pub fn random() -> Self {
        Self(rand::random())
    }

    pub fn fmt_short(&self) -> String {
        format!("{:08x}", self.0 as u32)
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
    replica: cola::Replica,
}

impl Replica {
    pub fn new(id: ReplicaId, text: &Rope) -> Self {
        Self {
            replica: cola::Replica::new(id, text.len_chars()),
        }
    }

    /// Forks, so the id must differ from every other replica in the session.
    pub fn decode(id: ReplicaId, encoded: &[u8]) -> Result<Self> {
        let encoded = EncodedReplica::from_bytes(encoded);
        Ok(Self {
            replica: cola::Replica::decode(id, &encoded)?,
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
