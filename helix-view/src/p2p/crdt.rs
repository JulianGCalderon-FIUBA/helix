//! Bridges Helix's ChangeSets to CRDT operations.

use anyhow::Result;
use cola::{EncodedReplica, Insertion, ReplicaId};
use helix_core::{ChangeSet, Operation, Rope, Transaction};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteOperation {
    Insert { insertion: Insertion, text: String },
    Delete(cola::Deletion),
}

fn replica_id() -> ReplicaId {
    rand::random_range(1..=ReplicaId::MAX)
}

pub struct Replica {
    replica: cola::Replica,
}

impl Replica {
    pub fn new(text: &Rope) -> Self {
        Self {
            replica: cola::Replica::new(replica_id(), text.len_chars()),
        }
    }

    pub fn decode(encoded: &[u8]) -> Result<Self> {
        let encoded = EncodedReplica::from_bytes(encoded);
        let replica = cola::Replica::decode(replica_id(), &encoded)?;
        Ok(Self { replica })
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
    /// Returns None when Cola backlogged the operation.
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
