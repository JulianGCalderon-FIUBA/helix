//! Bridges Helix's [`ChangeSet`]s to [`cola`], a text CRDT.
//!
//! Cola counts in whatever unit you decide and never checks. Helix indexes
//! chars, so every `usize` crossing this boundary is a char index.
//!
//! Cola never sees the text. It tracks where each edit landed relative to the
//! others, and tells us where a remote edit falls in our text. Applying the
//! edit, and carrying the inserted text around, is up to us.

use anyhow::Result;
use cola::{EncodedReplica, Insertion, ReplicaId};
use helix_core::{ChangeSet, Operation, Rope, Transaction};
use serde::{Deserialize, Serialize};

/// A local edit, as other replicas can integrate it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteOperation {
    /// Cola's Insertion only says where and how long, so the inserted text
    /// travels next to it.
    Insert {
        insertion: Insertion,
        text: String,
    },
    Delete(cola::Deletion),
}

/// Cola needs every replica of a document to have its own id, which is how
/// it orders concurrent edits. Random, so peers need no coordination.
///
/// Private, since callers have no reason to pick one.
fn replica_id() -> ReplicaId {
    // Cola panics on a zero id.
    rand::random_range(1..=ReplicaId::MAX)
}

/// One peer's CRDT state for one document.
///
/// Wraps cola's replica so the rest of the code deals in Helix's types.
pub struct Replica {
    replica: cola::Replica,
}

impl Replica {
    /// For a document we share: cola only needs its length.
    pub fn new(text: &Rope) -> Self {
        Self {
            replica: cola::Replica::new(replica_id(), text.len_chars()),
        }
    }

    /// For a document someone else shared: forks their replica, taking a new
    /// id for ourselves.
    pub fn decode(encoded: &[u8]) -> Result<Self> {
        let encoded = EncodedReplica::from_bytes(encoded);
        Ok(Self {
            replica: cola::Replica::decode(replica_id(), &encoded)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.replica.encode().as_bytes().to_vec()
    }

    /// Translate local transactions to CRDT operations.
    ///
    /// Must run after the change is applied to the text, as cola expects
    /// offsets into the text as it is once each edit is made.
    pub fn from_local(&mut self, changes: &ChangeSet) -> Vec<RemoteOperation> {
        let mut ops = Vec::new();
        // A ChangeSet's operations are relative to the old text, while cola
        // wants each edit's offset in the text after the edits before it. pos
        // tracks that: how far into the new text we are.
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
                    // The inserted text is now part of the new text.
                    pos += len;
                }
                // pos stays put: the text after the deleted range moves back
                // to where it started.
                Operation::Delete(n) => {
                    ops.push(RemoteOperation::Delete(self.replica.deleted(pos..pos + n)));
                }
            }
        }

        ops
    }

    /// Translate CRDT operations to local transactions.
    ///
    /// `None` means Cola backlogged the op, not that it failed. That happens
    /// when the op arrives before an edit it depends on. Cola would hand it
    /// back through `backlogged_insertions` and `backlogged_deletions` once
    /// the missing edit arrives, but we never ask, so for now it is lost.
    pub fn from_remote(&mut self, text: &Rope, op: &RemoteOperation) -> Option<Transaction> {
        let transaction = match op {
            RemoteOperation::Insert { insertion, text: s } => {
                // Where the insertion falls in our text, which differs from
                // where it happened if we edited concurrently.
                let at = self.replica.integrate_insertion(insertion)?;
                Transaction::change(text, [(at, at, Some(s.as_str().into()))].into_iter())
            }
            RemoteOperation::Delete(deletion) => {
                // Several ranges, as text we inserted concurrently may have
                // split the deleted range. Empty when backlogged, or when the
                // text was already deleted.
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
        // Marked so the hook that broadcasts local edits skips it, instead of
        // echoing it back to the session.
        Some(transaction.as_remote())
    }
}
