use std::{iter, time::Duration};

use helix_peer::Request;
use rand::{rngs::StdRng, seq::IndexedRandom, Rng, SeedableRng};

const INSERT: &[&str] = &["i", "a", "o", "O", "A", "I"];
const DELETE: &[&str] = &["d", "wd", "bd", "ed", "xd", "<A-d>", "2wd", "xxd"];
const MOVE: &[&str] = &[
    "h", "j", "k", "l", "w", "b", "e", "W", "B", "gg", "ge", "gh", "gl", "3l", "2j", "5k", "f ",
    "t;",
];
const SELECT: &[&str] = &["x", "v3l<esc>", ";", "<A-;>", "C", "<A-C>", ",", ","];
const HISTORY: &[&str] = &["u", "U"];
const YANK: &[&str] = &["yp", "yP", "xyp", "wyP"];
const MISC: &[&str] = &["J", "<gt>", "<lt>", "~", "ra", "rñ", "."];
// No regex prompts: a remote edit while one is open panics.
const MULTI: &[&str] = &["%<A-s>", "x<A-s>", "%<A-s>,"];
const TEXT: &[&str] = &[
    "hello",
    "world",
    "fn",
    "let x = 1;",
    "foo",
    "bar",
    " ",
    " ",
    " ",
    "<ret>",
    "<ret>",
    "<backspace>",
    "<backspace>",
    "<del>",
    "<C-w>",
    "<tab>",
    "(",
    "\"",
    "{",
    "[",
    "é",
    "ñ",
    "👋",
    "日本",
];

/// Acts like a user on a peer.
pub struct Bot {
    rng: StdRng,
    interval: Duration,
    docs: Vec<String>,
    /// Closing an owned document would unshare it.
    closes: bool,
}

impl Bot {
    pub fn new(seed: u64, interval: Duration, docs: Vec<String>, closes: bool) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            interval,
            docs,
            closes,
        }
    }

    /// One key per request, so that remote edits land in between.
    pub fn action(&mut self) -> Vec<Request> {
        let keys = match self.rng.random_range(0..100) {
            0..30 => format!("{}{}<esc>", self.pick(INSERT), self.text()),
            30..38 => format!("{}c{}<esc>", self.pick(&["w", "b", "e", "x"]), self.text()),
            38..52 => self.pick(DELETE).to_string(),
            52..70 => self.pick(MOVE).to_string(),
            70..80 => self.pick(SELECT).to_string(),
            80..85 => self.pick(HISTORY).to_string(),
            85..90 => self.pick(YANK).to_string(),
            90..95 => self.pick(MISC).to_string(),
            95..97 => self.pick(MULTI).to_string(),
            97..99 => {
                let id = self.docs.choose(&mut self.rng).expect("docs are not empty");
                return vec![Request::Open { id: id.clone() }];
            }
            _ if self.closes => ":bc!<ret>".to_string(),
            _ => self.pick(MOVE).to_string(),
        };
        // Leaves whatever mode or prompt the last action left open.
        let keys = format!("<esc>{keys}");
        split(&keys).map(|keys| Request::Keys { keys }).collect()
    }

    /// Users type at an uneven pace.
    pub fn pause(&mut self) -> Duration {
        self.interval.mul_f64(self.rng.random_range(0.0..2.0))
    }

    fn pick(&mut self, choices: &[&'static str]) -> &'static str {
        choices
            .choose(&mut self.rng)
            .expect("choices are not empty")
    }

    fn text(&mut self) -> String {
        let len = self.rng.random_range(1..8);
        (0..len).map(|_| self.pick(TEXT)).collect()
    }
}

/// Splits keys into single keys, such as `a` or `<esc>`.
fn split(mut keys: &str) -> impl Iterator<Item = String> + '_ {
    iter::from_fn(move || {
        let first = keys.chars().next()?;
        let len = match first {
            '<' => keys.find('>')? + 1,
            _ => first.len_utf8(),
        };
        let (key, rest) = keys.split_at(len);
        keys = rest;
        Some(key.to_string())
    })
}

#[cfg(test)]
mod tests {
    use helix_view::input::parse_macro;

    use super::*;

    #[test]
    fn splits_into_single_keys() {
        let keys: Vec<_> = split("a<esc>ñ<A-d>👋").collect();
        assert_eq!(keys, ["a", "<esc>", "ñ", "<A-d>", "👋"]);
    }

    #[test]
    fn every_action_is_single_keys() {
        let mut bot = Bot::new(0, Duration::ZERO, vec!["id".into()], true);
        for _ in 0..10_000 {
            for request in bot.action() {
                if let Request::Keys { keys } = request {
                    assert_eq!(parse_macro(&keys).unwrap().len(), 1, "{keys}");
                }
            }
        }
    }
}
