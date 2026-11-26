//! Runs a simulated user on every peer, then checks that buffers converge
//! without drift. Peers run in this process, each with its own endpoint, so
//! they still talk over the real network.

mod peer;
mod user;

use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{bail, ensure, Context, Result};
use helix_p2p::{net, wire::SharedId};
use parking_lot::Mutex;
use tokio::time::{sleep, timeout, Instant};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};

use crate::{
    peer::{Peer, Snapshot},
    user::User,
};

const HELP: &str = "\
USAGE:
    hx-swarm [FLAGS]

FLAGS:
    -h, --help          Print help information
    --peers <n>         Number of peers (default: 4)
    --docs <n>          Number of shared documents (default: 2)
    --steps <n>         Actions per user (default: 300)
    --interval <ms>     Average pause between steps (default: 30)
    --seed <n>          Seed for the users (default: time)
    --settle <secs>     Time without progress before failing (default: 30)
    --out <dir>         Directory for the log and diverged buffers
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
                arg => bail!("unexpected argument: {arg}"),
            }
        }
        ensure!(args.peers >= 2, "--peers must be at least 2");
        ensure!(args.docs >= 1, "--docs must be at least 1");
        Ok(args)
    }
}

type Peers = [Arc<Mutex<Peer>>];

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
    Logger::init(&out.join("swarm.log"))?;

    // A peer that panics would stop its network task silently, so a panic
    // anywhere fails the run, like a crash in the editor.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        hook(info);
        std::process::exit(101);
    }));

    let peers: Vec<_> = (0..args.peers)
        .map(|i| {
            let (peer, events) = Peer::new(format!("bot{i}"));
            let peer = Arc::new(Mutex::new(peer));
            tokio::spawn(receive(peer.clone(), events));
            peer
        })
        .collect();
    let names: Vec<_> = peers.iter().map(|peer| peer.lock().name.clone()).collect();
    let start = Instant::now();

    let ids = start_session(&peers, args.docs, settle).await?;
    open_documents(&peers, &names, &ids).await?;
    println!("{:.1?} opened", start.elapsed());

    let interval = Duration::from_millis(args.interval);
    let users: Vec<_> = peers
        .iter()
        .enumerate()
        .map(|(i, peer)| {
            // The first peer owns every document, and closing one would unshare it.
            let closes = i > 0;
            let seed = args.seed.wrapping_add(i as u64);
            let user = User::new(seed, interval, ids.clone(), closes);
            tokio::spawn(run_user(peer.clone(), user, args.steps))
        })
        .collect();
    for user in users {
        user.await?;
    }
    println!("{:.1?} users done", start.elapsed());

    // Users may have closed documents, so every peer reopens them to compare.
    open_documents(&peers, &names, &ids).await?;

    let problems = wait_for_convergence(&peers, &names, &ids, settle).await;
    if problems.is_empty() {
        println!("{:.1?} converged", start.elapsed());
        return Ok(());
    }
    for problem in &problems {
        println!("{problem}");
    }
    write_buffers(&peers, &names, &out)?;
    bail!(
        "{} problems, buffers written to {}",
        problems.len(),
        out.display()
    )
}

async fn receive(peer: Arc<Mutex<Peer>>, mut events: UnboundedReceiverStream<net::Event>) {
    while let Some(event) = events.next().await {
        peer.lock().handle(event);
    }
}

/// The first peer hosts and shares every document, and the rest join.
/// Returns the documents' ids.
async fn start_session(peers: &Peers, docs: usize, settle: Duration) -> Result<Vec<SharedId>> {
    let ticket = peers[0].lock().host();
    let ticket = timeout(settle, ticket).await.context("timed out hosting")?;
    // So that a person can join from hx and watch the users type.
    println!("ticket {ticket}");

    let mut ids = Vec::new();
    for i in 0..docs {
        let text: String = (0..20)
            .map(|line| format!("{i}:{line} the quick brown fox jumps over the lazy dog\n"))
            .collect();
        ids.push(peers[0].lock().share(&text)?);
    }
    for peer in &peers[1..] {
        peer.lock().join(&ticket)?;
    }
    Ok(ids)
}

async fn run_user(peer: Arc<Mutex<Peer>>, mut user: User, steps: usize) {
    for _ in 0..steps {
        for step in user.action() {
            step.perform(&mut peer.lock());
            sleep(user.pause()).await;
        }
    }
}

/// Opens every document on every peer and waits for the snapshots. Guests
/// only learn of documents once their join completes in the background, so
/// opening is retried.
async fn open_documents(peers: &Peers, names: &[String], ids: &[SharedId]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut synced = true;
        for peer in peers {
            let mut peer = peer.lock();
            for &id in ids {
                synced &= peer.open(id).is_ok() && peer.is_synced(id);
            }
        }
        if synced {
            return Ok(());
        }
        if Instant::now() > deadline {
            let problems = find_problems(names, &collect_snapshots(peers), ids);
            bail!("failed to open documents: {}", problems.join(", "));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn collect_snapshots(peers: &Peers) -> Vec<Vec<Snapshot>> {
    peers.iter().map(|peer| peer.lock().snapshot()).collect()
}

/// Waits while peers make progress, for up to ten times the timeout.
async fn wait_for_convergence(
    peers: &Peers,
    names: &[String],
    ids: &[SharedId],
    timeout: Duration,
) -> Vec<String> {
    let cap = Instant::now() + timeout * 10;
    let mut deadline = Instant::now() + timeout;
    let mut last = 0;
    loop {
        let snapshots = collect_snapshots(peers);
        let problems = find_problems(names, &snapshots, ids);
        let now = Instant::now();
        if problems.is_empty() || now > deadline.min(cap) {
            return problems;
        }
        // Peers make progress as they apply each other's edits.
        let ops = snapshots.iter().flatten().map(|buffer| buffer.ops).sum();
        if ops != last {
            deadline = now + timeout;
            last = ops;
        }
        sleep(POLL).await;
    }
}

/// Finds peers that lost a document, buffers that drifted from their
/// replica, and peers that hold different text.
fn find_problems(names: &[String], snapshots: &[Vec<Snapshot>], ids: &[SharedId]) -> Vec<String> {
    let mut problems = Vec::new();
    for &id in ids {
        let short = id.fmt_short();
        let mut groups: Vec<(&Snapshot, Vec<&str>)> = Vec::new();
        for (name, snapshots) in names.iter().zip(snapshots) {
            let Some(buffer) = snapshots.iter().find(|buffer| buffer.id == id) else {
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
fn write_buffers(peers: &Peers, names: &[String], out: &Path) -> Result<()> {
    for (name, snapshots) in names.iter().zip(collect_snapshots(peers)) {
        for buffer in snapshots {
            let file = out.join(format!("{}-{name}", buffer.id.fmt_short()));
            std::fs::write(file.with_extension("txt"), &buffer.text)?;
            if let Some(replica) = &buffer.replica {
                std::fs::write(file.with_extension("replica.txt"), replica)?;
            }
        }
    }
    Ok(())
}

/// Writes warnings and errors from every peer to one file.
struct Logger(std::sync::Mutex<File>);

impl Logger {
    fn init(path: &Path) -> Result<()> {
        let logger = Logger(std::sync::Mutex::new(File::create(path)?));
        log::set_boxed_logger(Box::new(logger))?;
        log::set_max_level(log::LevelFilter::Warn);
        Ok(())
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if let Ok(mut file) = self.0.lock() {
            let target = record.target();
            drop(writeln!(
                file,
                "{target} [{}] {}",
                record.level(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use helix_p2p::net::TopicId;

    use super::*;

    fn doc() -> SharedId {
        SharedId::from(TopicId::from_bytes([0; 32]))
    }

    fn buffer(text: &str, replica: Option<&str>, ops: usize) -> Snapshot {
        Snapshot {
            id: doc(),
            text: text.into(),
            replica: replica.map(Into::into),
            ops,
        }
    }

    fn problems(buffers: Vec<Option<Snapshot>>) -> Vec<String> {
        let names: Vec<_> = (0..buffers.len()).map(|i| format!("bot{i}")).collect();
        let snapshots: Vec<_> = buffers
            .into_iter()
            .map(|buffer| buffer.into_iter().collect())
            .collect();
        find_problems(&names, &snapshots, &[doc()])
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
        let short = doc().fmt_short();
        let buffers = vec![
            None,
            Some(buffer("", None, 0)),
            Some(buffer("a", Some("b"), 1)),
        ];
        assert_eq!(
            problems(buffers),
            [
                format!("bot0 lost {short}"),
                format!("bot1 is still opening {short}"),
                format!("bot2 drifted on {short}"),
            ]
        );
    }

    #[test]
    fn diverged() {
        let short = doc().fmt_short();
        let buffers = vec![
            Some(buffer("a", Some("a"), 2)),
            Some(buffer("b", Some("b"), 2)),
            Some(buffer("a", Some("a"), 2)),
        ];
        assert_eq!(
            problems(buffers),
            [format!(
                r#"{short} diverged: ["bot0", "bot2"] with 2 ops, ["bot1"] with 2 ops"#
            )]
        );
    }
}
