//! Bridges Helix's ChangeSets to CRDT operations.

use anyhow::{ensure, Context, Result};
use helix_core::{Change, ChangeSet, Operation, Range, Rope, Selection};
use loro::{
    cursor::{Cursor, Side},
    event::Diff,
    ExportMode, LoroDoc, LoroText, TextDelta,
};

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

    /// Translate a local selection to one that other replicas can place,
    /// even if they have seen different edits.
    ///
    /// Positions are sent as Loro cursors, which point at characters
    /// instead of offsets, so concurrent edits don't shift them.
    pub fn encode_selection(&self, selection: &Selection) -> Result<Vec<u8>> {
        let cursor = |pos| {
            self.text
                .get_cursor(pos, Side::Middle)
                .with_context(|| format!("no cursor at {pos}"))
        };
        // The selection can be ahead of the replica, after edits that
        // were not synced (#44).
        let len = self.text.len_unicode();
        let ranges = selection
            .iter()
            .map(|range| {
                ensure!(range.to() <= len, "selection is out of bounds");
                Ok((cursor(range.anchor)?, cursor(range.head)?))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(postcard::to_stdvec(&(ranges, selection.primary_index()))?)
    }

    /// Translate a remote selection to positions in this replica.
    ///
    /// Fails when it points at edits this replica hasn't seen yet.
    pub fn decode_selection(&self, selection: &[u8]) -> Result<Selection> {
        let (ranges, primary): (Vec<(Cursor, Cursor)>, usize) = postcard::from_bytes(selection)?;
        ensure!(primary < ranges.len(), "primary range is out of bounds");

        let pos = |cursor| -> Result<usize> { Ok(self.doc.get_cursor_pos(cursor)?.current.pos) };
        let ranges = ranges
            .iter()
            .map(|(anchor, head)| Ok(Range::new(pos(anchor)?, pos(head)?)))
            .collect::<Result<_>>()?;
        Ok(Selection::new(ranges, primary))
    }
}
