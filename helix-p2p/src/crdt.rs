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

#[cfg(test)]
mod tests {
    use helix_core::Transaction;

    use super::*;

    struct Buffer {
        replica: Replica,
        text: Rope,
    }

    impl Buffer {
        fn new(text: &str) -> Self {
            let text = Rope::from(text);
            Self {
                replica: Replica::new(&text),
                text,
            }
        }

        /// Another buffer that starts from this one's snapshot.
        fn fork(&self) -> Self {
            let mut buffer = Self {
                replica: Replica::empty(),
                text: Rope::new(),
            };
            buffer.receive(&self.replica.encode());
            buffer
        }

        fn edit(&mut self, changes: Vec<Change>) -> Vec<u8> {
            let transaction = Transaction::change(&self.text, changes.into_iter());
            transaction.apply(&mut self.text);
            self.replica.from_local(transaction.changes())
        }

        fn receive(&mut self, update: &[u8]) {
            let changes = self.replica.from_remote(update).unwrap();
            Transaction::change(&self.text, changes.into_iter()).apply(&mut self.text);
        }
    }

    fn assert_syncs(text: &str, changes: Vec<Change>, expected: &str) {
        let mut local = Buffer::new(text);
        let mut remote = local.fork();

        let update = local.edit(changes);
        remote.receive(&update);

        assert_eq!(local.text, expected);
        assert_eq!(remote.text, expected);
    }

    #[test]
    fn snapshot_fills_empty_buffer() {
        let buffer = Buffer::new("hello\nworld\n");
        assert_eq!(buffer.fork().text, "hello\nworld\n");
    }

    #[test]
    fn insert_at_start() {
        assert_syncs("hello", vec![(0, 0, Some("> ".into()))], "> hello");
    }

    #[test]
    fn insert_at_end() {
        assert_syncs("hello", vec![(5, 5, Some(" world".into()))], "hello world");
    }

    #[test]
    fn insert_into_empty() {
        assert_syncs("", vec![(0, 0, Some("hello".into()))], "hello");
    }

    #[test]
    fn delete() {
        assert_syncs("hello world", vec![(5, 11, None)], "hello");
    }

    #[test]
    fn delete_everything() {
        assert_syncs("hello", vec![(0, 5, None)], "");
    }

    #[test]
    fn replace() {
        assert_syncs(
            "hello world",
            vec![(6, 11, Some("there".into()))],
            "hello there",
        );
    }

    #[test]
    fn several_changes_at_once() {
        assert_syncs(
            "one two three",
            vec![
                (0, 3, Some("1".into())),
                (4, 7, None),
                (8, 13, Some("3".into())),
            ],
            "1  3",
        );
    }

    #[test]
    fn multiline() {
        assert_syncs(
            "a\nb\nc\n",
            vec![(2, 4, None), (6, 6, Some("d\n".into()))],
            "a\nc\nd\n",
        );
    }

    // Positions are in chars on both sides, not in bytes or UTF-16 units.
    #[test]
    fn multibyte_chars() {
        assert_syncs(
            "héllo 👋 wörld",
            vec![(1, 2, Some("e".into())), (8, 13, Some("🌍".into()))],
            "hello 👋 🌍",
        );
    }

    #[test]
    fn successive_edits() {
        let mut local = Buffer::new("");
        let mut remote = local.fork();

        for changes in [
            vec![(0, 0, Some("fn main() {}".into()))],
            vec![(11, 11, Some("\n".into()))],
            vec![(11, 11, Some("\n    todo!();".into()))],
            vec![(3, 7, Some("run".into()))],
        ] {
            let update = local.edit(changes);
            remote.receive(&update);
            assert_eq!(remote.text, local.text);
        }
        assert_eq!(remote.text, "fn run() {\n    todo!();\n}");
    }

    #[test]
    fn concurrent_edits() {
        let mut a = Buffer::new("hello world");
        let mut b = a.fork();

        let from_a = a.edit(vec![(0, 0, Some("> ".into()))]);
        let from_b = b.edit(vec![(6, 11, Some("there".into()))]);
        a.receive(&from_b);
        b.receive(&from_a);

        assert_eq!(a.text, "> hello there");
        assert_eq!(b.text, "> hello there");
    }

    #[test]
    fn concurrent_deletes_overlap() {
        let mut a = Buffer::new("one two three");
        let mut b = a.fork();

        let from_a = a.edit(vec![(0, 8, None)]);
        let from_b = b.edit(vec![(4, 13, None)]);
        a.receive(&from_b);
        b.receive(&from_a);

        assert_eq!(a.text, "");
        assert_eq!(b.text, "");
    }

    #[test]
    fn early_update_waits_for_dependencies() {
        let mut local = Buffer::new("");
        let mut remote = local.fork();

        let first = local.edit(vec![(0, 0, Some("one".into()))]);
        let second = local.edit(vec![(3, 3, Some(" two".into()))]);

        assert!(remote.replica.from_remote(&second).unwrap().is_empty());
        remote.receive(&first);
        assert_eq!(remote.text, "one two");
    }

    #[test]
    fn repeated_update_changes_nothing() {
        let mut local = Buffer::new("hello");
        let mut remote = local.fork();

        let update = local.edit(vec![(5, 5, Some("!".into()))]);
        remote.receive(&update);

        assert!(remote.replica.from_remote(&update).unwrap().is_empty());
    }
}
