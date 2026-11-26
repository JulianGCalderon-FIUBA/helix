//! A simulated user. Each action becomes steps that run one at a time with
//! pauses in between, so remote edits land mid-word, like with people. Steps
//! work from the cursor at the time they run, which the peer maps through
//! remote edits.

use std::time::Duration;

use helix_core::{movement, Range, Rope, Selection, SmallVec, Transaction};
use helix_p2p::wire::SharedId;
use rand::{rngs::StdRng, seq::IndexedRandom, Rng, SeedableRng};

use crate::peer::Peer;

const WORDS: &[&str] = &[
    "hello",
    "world",
    "fn",
    "let x = 1;",
    "foo",
    "bar",
    "the",
    "fox",
    "é",
    "ñ",
    "👋",
    "日本",
];
const PASTES: &[&str] = &[
    "fn main() {\n    println!(\"hello\");\n}\n",
    "// TODO\n",
    "the quick brown fox\njumps over the lazy dog\n",
];

#[derive(Debug, Clone)]
pub enum Step {
    /// Inserts at every cursor, one character per step when typing.
    Insert(String),
    /// Deletes the character before every cursor.
    Backspace,
    /// Deletes what every range selects.
    Delete,
    /// Moves to the start of the line at this fraction of the buffer.
    Jump(f64),
    /// Moves near another peer's cursor, at this offset.
    JumpNearPeer(usize),
    /// Selects up to the end of the next word.
    SelectWord,
    SelectLine,
    /// Selects every occurrence of the word, like search and replace.
    SelectAll(&'static str),
    AddCursorBelow,
    KeepPrimary,
    Open(SharedId),
    Close,
}

pub struct User {
    rng: StdRng,
    interval: Duration,
    docs: Vec<SharedId>,
    /// Closing an owned document would unshare it.
    closes: bool,
}

impl User {
    pub fn new(seed: u64, interval: Duration, docs: Vec<SharedId>, closes: bool) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            interval,
            docs,
            closes,
        }
    }

    pub fn action(&mut self) -> Vec<Step> {
        let mut steps = Vec::new();
        match self.rng.random_range(0..100) {
            0..30 => self.typing(&mut steps),
            30..40 => {
                let count = self.rng.random_range(1..8);
                steps.extend((0..count).map(|_| Step::Backspace));
            }
            40..48 => steps.extend([Step::SelectWord, Step::Delete]),
            48..52 => steps.extend([Step::SelectLine, Step::Delete]),
            52..60 => {
                steps.extend([Step::SelectWord, Step::Delete]);
                self.typing(&mut steps);
            }
            60..65 => {
                let paste = self.pick(PASTES);
                steps.push(Step::Insert(paste.into()));
            }
            65..78 => steps.push(Step::Jump(self.rng.random())),
            78..85 => steps.push(Step::JumpNearPeer(self.rng.random_range(0..20))),
            85..90 => {
                let cursors = self.rng.random_range(1..4);
                steps.extend((0..cursors).map(|_| Step::AddCursorBelow));
                self.typing(&mut steps);
                steps.push(Step::KeepPrimary);
            }
            90..94 => {
                let word = self.pick(&["the", "fox", "hello", "o"]);
                steps.extend([Step::SelectAll(word), Step::Delete]);
                self.typing(&mut steps);
                steps.push(Step::KeepPrimary);
            }
            94..98 => steps.push(Step::Open(*self.docs.choose(&mut self.rng).unwrap())),
            _ if self.closes => {
                let next = *self.docs.choose(&mut self.rng).unwrap();
                steps.extend([Step::Close, Step::Open(next)]);
            }
            _ => steps.push(Step::Jump(self.rng.random())),
        }
        steps
    }

    /// Users type at an uneven pace.
    pub fn pause(&mut self) -> Duration {
        self.interval.mul_f64(self.rng.random_range(0.0..2.0))
    }

    /// Types a few words a character at a time, with the odd typo fixed.
    fn typing(&mut self, steps: &mut Vec<Step>) {
        for _ in 0..self.rng.random_range(1..5) {
            let word = self.pick(WORDS);
            steps.extend(word.chars().map(|c| Step::Insert(c.into())));
            if self.rng.random_bool(0.1) {
                steps.extend([Step::Insert("x".into()), Step::Backspace]);
            }
            let separator = if self.rng.random_bool(0.2) { '\n' } else { ' ' };
            steps.push(Step::Insert(separator.into()));
        }
    }

    fn pick(&mut self, choices: &[&'static str]) -> &'static str {
        choices
            .choose(&mut self.rng)
            .expect("choices are not empty")
    }
}

impl Step {
    pub fn perform(self, peer: &mut Peer) {
        match self {
            Step::Open(id) => {
                if let Err(err) = peer.open(id) {
                    log::warn!("{} failed to open {}: {err:#}", peer.name, id.fmt_short());
                }
                return;
            }
            Step::Close => {
                if let Some((id, _)) = peer.focused() {
                    peer.close(id);
                }
                return;
            }
            Step::JumpNearPeer(offset) => {
                if let Some(&cursor) = peer.peer_cursors().first() {
                    let len = peer.focused().map_or(0, |(_, b)| b.text.len_chars());
                    peer.select(Selection::point((cursor + offset).min(len)));
                }
                return;
            }
            _ => {}
        }

        let Some((_, buffer)) = peer.focused() else {
            return;
        };
        let text = &buffer.text;
        let selection = &buffer.selection;
        let slice = text.slice(..);
        match self {
            Step::Insert(insert) => {
                peer.apply(&Transaction::insert(text, selection, insert.into()));
            }
            Step::Backspace => {
                let transaction = Transaction::change_by_selection(text, selection, |range| {
                    let from = range.head.saturating_sub(1);
                    (from, range.head, None)
                });
                peer.apply(&transaction);
            }
            Step::Delete => {
                let transaction = Transaction::change_by_selection(text, selection, |range| {
                    (range.from(), range.to(), None)
                });
                peer.apply(&transaction);
            }
            Step::Jump(fraction) => {
                let line = (fraction * text.len_lines() as f64) as usize;
                let line = line.min(text.len_lines().saturating_sub(1));
                peer.select(Selection::point(text.line_to_char(line)));
            }
            Step::SelectWord => {
                let range = movement::move_next_word_end(slice, selection.primary(), 1);
                peer.select(Selection::single(range.anchor, range.head));
            }
            Step::SelectLine => {
                let line = text.char_to_line(selection.primary().head);
                let end = text.line_to_char((line + 1).min(text.len_lines()));
                peer.select(Selection::single(text.line_to_char(line), end));
            }
            Step::SelectAll(word) => {
                let ranges: SmallVec<_> = occurrences(text, word).collect();
                if !ranges.is_empty() {
                    peer.select(Selection::new(ranges, 0));
                }
            }
            Step::AddCursorBelow => {
                let head = selection.primary().head;
                let line = text.char_to_line(head);
                if line + 1 < text.len_lines() {
                    let column = head - text.line_to_char(line);
                    let below = text.line_to_char(line + 1);
                    let width = text.line(line + 1).len_chars().saturating_sub(1);
                    let range = Range::point(below + column.min(width));
                    peer.select(selection.clone().push(range));
                }
            }
            Step::KeepPrimary => peer.select(selection.clone().into_single()),
            Step::Open(_) | Step::Close | Step::JumpNearPeer(_) => unreachable!(),
        }
    }
}

fn occurrences<'a>(text: &'a Rope, word: &'a str) -> impl Iterator<Item = Range> + 'a {
    let string = text.to_string();
    let matches: Vec<_> = string.match_indices(word).map(|(byte, _)| byte).collect();
    matches.into_iter().map(move |byte| {
        let from = text.byte_to_char(byte);
        Range::new(from, from + word.chars().count())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_steps() {
        let docs = vec![SharedId::from(helix_p2p::net::TopicId::from_bytes([0; 32]))];
        let mut user = User::new(0, Duration::ZERO, docs, true);
        for _ in 0..10_000 {
            assert!(!user.action().is_empty());
        }
    }

    #[test]
    fn finds_occurrences() {
        let text = Rope::from("a fox, a fox 👋 fox");
        let ranges: Vec<_> = occurrences(&text, "fox")
            .map(|r| (r.from(), r.to()))
            .collect();
        assert_eq!(ranges, [(2, 5), (9, 12), (15, 18)]);
    }
}
