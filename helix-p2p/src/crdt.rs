//! Bridges Helix's ChangeSets to CRDT operations.

use anyhow::Result;
use helix_core::{Change, ChangeSet, Operation, Rope};
use loro::{event::Diff, ExportMode, LoroDoc, LoroText, TextDelta};

const TEXT_ID: &str = "text";

pub struct Replica {
    doc: LoroDoc,
    text: LoroText,
}

impl Replica {
    pub fn new(text: &Rope) -> Self {
        let doc = LoroDoc::new();
        let replica = Self {
            text: doc.get_text(TEXT_ID),
            doc,
        };
        replica
            .text
            .insert(0, &text.to_string())
            .expect("insert should not fail");
        replica.doc.commit();
        replica
    }

    /// A replica with no content yet, to fill in with a snapshot.
    pub fn empty() -> Self {
        let doc = LoroDoc::new();
        Self {
            text: doc.get_text(TEXT_ID),
            doc,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        self.doc
            .export(ExportMode::Snapshot)
            .expect("export should not fail")
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
            .expect("export should not fail")
    }

    /// Translate a CRDT update to local changes.
    ///
    /// The changes are empty when Loro keeps the update pending
    /// until its dependencies arrive.
    pub fn from_remote(&mut self, update: &[u8]) -> Result<Vec<Change>> {
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

        Ok(changes)
    }
}
