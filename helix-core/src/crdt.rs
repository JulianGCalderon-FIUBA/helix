//! Bridges Helix's [`ChangeSet`]s to [`cola`], a text CRDT.
//!
//! cola counts in whatever unit you decide and never checks. Helix indexes
//! chars, so every `usize` crossing this boundary is a char index.

use anyhow::Result;
use cola::{EncodedReplica, Insertion, ReplicaId};
use serde::{Deserialize, Serialize};

use crate::{transaction::Operation, ChangeSet, Rope, Transaction};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteOperation {
    Insert { insertion: Insertion, text: String },
    Delete(cola::Deletion),
}

/// TODO: derive this from the peer's `EndpointId` so it survives a reconnect.
pub fn replica_id() -> ReplicaId {
    // cola panics on a zero id.
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

    /// Forks, so `id` must differ from every other replica in the session:
    /// cola breaks ties between concurrent insertions by comparing ids, and
    /// two replicas sharing one silently diverge.
    pub fn decode(id: ReplicaId, encoded: &[u8]) -> Result<Self> {
        let encoded = EncodedReplica::from_bytes(encoded);
        Ok(Self {
            replica: cola::Replica::decode(id, &encoded)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.replica.encode().as_bytes().to_vec()
    }

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

    /// `None` means cola backlogged the op, not that it failed.
    pub fn from_remote(&mut self, text: &Rope, op: &RemoteOperation) -> Option<Transaction> {
        match op {
            RemoteOperation::Insert { insertion, text: s } => {
                let at = self.replica.integrate_insertion(insertion)?;
                Some(Transaction::change(
                    text,
                    [(at, at, Some(s.as_str().into()))].into_iter(),
                ))
            }
            RemoteOperation::Delete(deletion) => {
                let ranges = self.replica.integrate_deletion(deletion);
                if ranges.is_empty() {
                    return None;
                }
                Some(Transaction::delete(
                    text,
                    ranges.into_iter().map(|range| (range.start, range.end)),
                ))
            }
        }
    }
}
