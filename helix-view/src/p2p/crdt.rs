//! Bridges Helix's ChangeSets to CRDT operations.

use anyhow::Result;
use helix_core::{ChangeSet, Operation, Rope, Transaction};
use loro::{event::Diff, ExportMode, LoroDoc, LoroText, TextDelta};

const TEXT: &str = "text";

pub struct Replica {
    doc: LoroDoc,
    text: LoroText,
}

impl Replica {
    pub fn new(text: &Rope) -> Self {
        let doc = LoroDoc::new();
        let replica = Self {
            text: doc.get_text(TEXT),
            doc,
        };
        replica
            .text
            .insert(0, &text.to_string())
            .expect("insert into empty text should succeed");
        replica.doc.commit();
        replica
    }

    /// Decodes a snapshot. The replica gets its own random peer id.
    pub fn decode(snapshot: &[u8]) -> Result<Self> {
        let doc = LoroDoc::from_snapshot(snapshot)?;
        Ok(Self {
            text: doc.get_text(TEXT),
            doc,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.doc
            .export(ExportMode::Snapshot)
            .expect("snapshot should export")
    }

    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// Translate local transactions to a CRDT update.
    pub fn from_local(&mut self, changes: &ChangeSet) -> Vec<u8> {
        let before = self.doc.oplog_vv();
        let mut pos = 0;

        for op in changes.changes() {
            match op {
                Operation::Retain(n) => pos += n,
                Operation::Insert(text) => {
                    self.text
                        .insert(pos, text)
                        .expect("insert should be in bounds");
                    pos += text.chars().count();
                }
                Operation::Delete(n) => {
                    self.text
                        .delete(pos, *n)
                        .expect("delete should be in bounds");
                }
            }
        }
        self.doc.commit();

        self.doc
            .export(ExportMode::updates(&before))
            .expect("update should export")
    }

    /// Translate a CRDT update to a local transaction.
    ///
    /// Returns None when the update changed nothing visible, for example
    /// when Loro keeps it pending until its dependencies arrive.
    pub fn from_remote(&mut self, text: &Rope, update: &[u8]) -> Result<Option<Transaction>> {
        let before = self.doc.state_frontiers();
        self.doc.import(update)?;
        let after = self.doc.state_frontiers();

        let mut changes = Vec::new();
        let mut pos = 0;
        for (_, diff) in self.doc.diff(&before, &after)?.iter() {
            let Diff::Text(deltas) = diff else {
                continue;
            };
            for delta in deltas {
                match delta {
                    TextDelta::Retain { retain, .. } => pos += retain,
                    TextDelta::Insert { insert, .. } => {
                        changes.push((pos, pos, Some(insert.as_str().into())))
                    }
                    TextDelta::Delete { delete } => {
                        changes.push((pos, pos + delete, None));
                        pos += delete;
                    }
                }
            }
        }

        if changes.is_empty() {
            return Ok(None);
        }
        Ok(Some(Transaction::change(text, changes.into_iter())))
    }
}
