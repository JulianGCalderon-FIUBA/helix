//! Runs a bot on every peer, then checks that buffers converge without drift.

mod bot;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};

use anyhow::{bail, ensure, Context, Result};
use futures_util::future::try_join_all;
use helix_peer::{Buffer, Request, Response, State};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{sleep, timeout, Instant},
};

use crate::bot::Bot;

const HELP: &str = "\
USAGE:
    hx-swarm [FLAGS]

Runs the hx-peer next to it, so build both with `cargo build -p helix-peer`.

FLAGS:
    -h, --help          Print help information
    --peers <n>         Number of peers (default: 4)
    --docs <n>          Number of shared documents (default: 2)
    --steps <n>         Actions per bot (default: 300)
    --interval <ms>     Average pause between keys (default: 30)
    --seed <n>          Seed for the bots (default: time)
    --settle <secs>     Time without progress before failing (default: 30)
    --out <dir>         Directory for logs and diverged buffers
    --local             Connect peers directly, without relays
";

const POLL: Duration = Duration::from_millis(500);

struct Args {
    display_help: bool,
    peers: usize,
    docs: usize,
    steps: usize,
    interval: u64,
    seed: u64,
    settle: u64,
    out: Option<PathBuf>,
    local: bool,
}

impl Args {
    fn parse_args() -> Result<Args> {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;
        let mut args = Args {
            display_help: false,
            peers: 4,
            docs: 2,
            steps: 300,
            interval: 30,
            seed: now.as_millis() as u64,
            settle: 30,
            out: None,
            local: false,
        };
        let mut argv = std::env::args();
        argv.next();

        while let Some(arg) = argv.next() {
            let mut value = || {
                argv.next()
                    .with_context(|| format!("{arg} must specify a value"))
            };
            match arg.as_str() {
                "-h" | "--help" => args.display_help = true,
                "--peers" => args.peers = value()?.parse()?,
                "--docs" => args.docs = value()?.parse()?,
                "--steps" => args.steps = value()?.parse()?,
                "--interval" => args.interval = value()?.parse()?,
                "--seed" => args.seed = value()?.parse()?,
                "--settle" => args.settle = value()?.parse()?,
                "--out" => args.out = Some(value()?.into()),
                "--local" => args.local = true,
                arg => bail!("unexpected argument: {arg}"),
            }
        }
        ensure!(args.peers >= 2, "--peers must be at least 2");
        ensure!(args.docs >= 1, "--docs must be at least 1");
        Ok(args)
    }
}

struct Peer {
    name: String,
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Peer {
    fn spawn(name: String, out: &Path, local: bool) -> Result<Self> {
        let exe = format!("hx-peer{}", std::env::consts::EXE_SUFFIX);
        let exe = std::env::current_exe()?.with_file_name(exe);
        let mut child = Command::new(&exe)
            .arg("--name")
            .arg(&name)
            .arg("--log")
            .arg(out.join(format!("{name}.log")))
            .args(local.then_some("--local"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start {}", exe.display()))?;
        Ok(Self {
            name,
            stdin: child.stdin.take().expect("stdin is piped"),
            stdout: BufReader::new(child.stdout.take().expect("stdout is piped")).lines(),
            _child: child,
        })
    }

    /// Writes the request as a line, and reads the response line.
    async fn call(&mut self, request: &Request) -> Result<Value> {
        let request = serde_json::to_string(request)?;
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        let response = self.stdout.next_line().await?;
        let response = response.with_context(|| format!("{} exited", self.name))?;
        match serde_json::from_str(&response)? {
            Response::Ok(value) => Ok(value),
            Response::Error(err) => bail!("{}: {err}", self.name),
        }
    }

    async fn state(&mut self) -> Result<State> {
        Ok(serde_json::from_value(self.call(&Request::State).await?)?)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse_args().context("could not parse arguments")?;
    if args.display_help {
        print!("{HELP}");
        return Ok(());
    }
    let settle = Duration::from_secs(args.settle);

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("hx-swarm-{}", args.seed)));
    std::fs::create_dir_all(&out)?;
    println!("seed {}, output in {}", args.seed, out.display());

    let mut peers: Vec<_> = (0..args.peers)
        .map(|i| Peer::spawn(format!("bot{i}"), &out, args.local))
        .collect::<Result<_>>()?;
    let start = Instant::now();

    let ids = start_session(&mut peers, args.docs, settle).await?;
    for peer in &mut peers {
        open_documents(peer, &ids).await?;
    }
    println!("{:.1?} opened", start.elapsed());

    let interval = Duration::from_millis(args.interval);
    let bots = peers.iter_mut().enumerate().map(|(i, peer)| {
        // The first peer owns every document, and closing one would unshare it.
        let closes = i > 0;
        let bot = Bot::new(
            args.seed.wrapping_add(i as u64),
            interval,
            ids.clone(),
            closes,
        );
        run_bot(peer, bot, args.steps)
    });
    try_join_all(bots).await?;
    println!("{:.1?} bots done", start.elapsed());

    // Bots may have closed documents, so every peer reopens them to compare.
    for peer in &mut peers {
        open_documents(peer, &ids).await?;
    }

    let problems = wait_for_convergence(&mut peers, &ids, settle).await?;
    if problems.is_empty() {
        println!("{:.1?} converged", start.elapsed());
        return Ok(());
    }
    for problem in &problems {
        println!("{problem}");
    }
    write_buffers(&mut peers, &out).await?;
    bail!(
        "{} problems, buffers written to {}",
        problems.len(),
        out.display()
    )
}

/// The first peer hosts and shares every document, and the rest join.
/// Returns the documents' ids.
async fn start_session(peers: &mut [Peer], docs: usize, settle: Duration) -> Result<Vec<String>> {
    let ticket = timeout(settle, peers[0].call(&Request::Host))
        .await
        .context("timed out hosting")??;
    let Value::String(ticket) = ticket else {
        bail!("host sent no ticket");
    };
    // So that a person can join from hx and watch the bots type.
    println!("ticket {ticket}");

    let mut ids = Vec::new();
    for i in 0..docs {
        let text: String = (0..20)
            .map(|line| format!("{i}:{line} the quick brown fox jumps over the lazy dog<ret>"))
            .collect();
        let keys = format!(":new<ret>i{text}<esc>:session-share<ret>");
        peers[0].call(&Request::Keys { keys }).await?;
        let state = peers[0].state().await?;
        ids.push(state.focus.context("failed to share a document")?);
    }

    let keys = format!(":session-join {ticket}<ret>");
    for peer in &mut peers[1..] {
        peer.call(&Request::Keys { keys: keys.clone() }).await?;
    }
    Ok(ids)
}

async fn run_bot(peer: &mut Peer, mut bot: Bot, steps: usize) -> Result<()> {
    for _ in 0..steps {
        for request in bot.action() {
            peer.call(&request).await?;
            sleep(bot.pause()).await;
        }
    }
    Ok(())
}

/// Opens every document and waits for its snapshot. Guests only learn of
/// documents once their join completes in the background, so this retries.
/// One at a time, since opening a buffer drops another that is still empty.
async fn open_documents(peer: &mut Peer, ids: &[String]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    for id in ids {
        loop {
            let opened = peer.call(&Request::Open { id: id.clone() }).await;
            let state = peer.state().await?;
            let synced = |buffer: &Buffer| buffer.id == *id && buffer.replica.is_some();
            if opened.is_ok() && state.buffers.iter().any(synced) {
                break;
            }
            if Instant::now() > deadline {
                let reason = opened
                    .err()
                    .map_or("no snapshot".into(), |err| err.to_string());
                bail!("{} failed to open {}: {reason}", peer.name, short_id(id));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(())
}

async fn collect_states(peers: &mut [Peer]) -> Result<Vec<State>> {
    try_join_all(peers.iter_mut().map(Peer::state)).await
}

/// Waits while peers make progress, for up to ten times the timeout.
async fn wait_for_convergence(
    peers: &mut [Peer],
    ids: &[String],
    timeout: Duration,
) -> Result<Vec<String>> {
    let names: Vec<_> = peers.iter().map(|peer| peer.name.clone()).collect();
    let cap = Instant::now() + timeout * 10;
    let mut deadline = Instant::now() + timeout;
    let mut last = Vec::new();
    loop {
        let states = collect_states(peers).await?;
        let problems = find_problems(&names, &states, ids);
        let now = Instant::now();
        if problems.is_empty() || now > deadline.min(cap) {
            return Ok(problems);
        }
        if states != last {
            deadline = now + timeout;
            last = states;
        }
        sleep(POLL).await;
    }
}

/// Finds peers that lost a document, buffers that drifted from their
/// replica, and peers that hold different text.
fn find_problems(names: &[String], states: &[State], ids: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    for id in ids {
        let short = short_id(id);
        let mut groups: Vec<(&Buffer, Vec<&str>)> = Vec::new();
        for (name, state) in names.iter().zip(states) {
            let Some(buffer) = state.buffers.iter().find(|buffer| buffer.id == *id) else {
                problems.push(format!("{name} lost {short}"));
                continue;
            };
            match &buffer.replica {
                None => problems.push(format!("{name} is still opening {short}")),
                Some(replica) if *replica != buffer.text => {
                    problems.push(format!("{name} drifted on {short}"))
                }
                Some(_) => match groups
                    .iter_mut()
                    .find(|(other, _)| other.text == buffer.text)
                {
                    Some((_, names)) => names.push(name),
                    None => groups.push((buffer, vec![name])),
                },
            }
        }
        if groups.len() > 1 {
            // Same ops but different text points at the CRDT.
            let groups: Vec<_> = groups
                .iter()
                .map(|(buffer, names)| format!("{names:?} with {} ops", buffer.ops))
                .collect();
            problems.push(format!("{short} diverged: {}", groups.join(", ")));
        }
    }
    problems
}

/// Writes every buffer and replica to disk, so that they can be diffed.
async fn write_buffers(peers: &mut [Peer], out: &Path) -> Result<()> {
    let states = collect_states(peers).await?;
    for (peer, state) in peers.iter().zip(&states) {
        for buffer in &state.buffers {
            let file = out.join(format!("{}-{}", short_id(&buffer.id), peer.name));
            std::fs::write(file.with_extension("txt"), &buffer.text)?;
            if let Some(replica) = &buffer.replica {
                std::fs::write(file.with_extension("replica.txt"), replica)?;
            }
        }
    }
    Ok(())
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(10)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str, replica: Option<&str>, ops: usize) -> Buffer {
        Buffer {
            id: "doc".into(),
            text: text.into(),
            replica: replica.map(Into::into),
            ops,
        }
    }

    fn problems(buffers: Vec<Option<Buffer>>) -> Vec<String> {
        let names: Vec<_> = (0..buffers.len()).map(|i| format!("bot{i}")).collect();
        let states: Vec<_> = buffers
            .into_iter()
            .map(|buffer| State {
                focus: None,
                buffers: buffer.into_iter().collect(),
            })
            .collect();
        find_problems(&names, &states, &["doc".into()])
    }

    #[test]
    fn converged() {
        let buffers = vec![
            Some(buffer("a", Some("a"), 1)),
            Some(buffer("a", Some("a"), 1)),
        ];
        assert!(problems(buffers).is_empty());
    }

    #[test]
    fn lost_opening_and_drifted() {
        let buffers = vec![
            None,
            Some(buffer("", None, 0)),
            Some(buffer("a", Some("b"), 1)),
        ];
        assert_eq!(
            problems(buffers),
            [
                "bot0 lost doc",
                "bot1 is still opening doc",
                "bot2 drifted on doc"
            ]
        );
    }

    #[test]
    fn diverged() {
        let buffers = vec![
            Some(buffer("a", Some("a"), 2)),
            Some(buffer("b", Some("b"), 2)),
            Some(buffer("a", Some("a"), 2)),
        ];
        assert_eq!(
            problems(buffers),
            [r#"doc diverged: ["bot0", "bot2"] with 2 ops, ["bot1"] with 2 ops"#]
        );
    }
}
