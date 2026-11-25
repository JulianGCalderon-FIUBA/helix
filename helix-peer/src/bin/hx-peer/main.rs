//! The editor without a terminal, driven by JSON lines on stdin.

mod peer;

use anyhow::{bail, Context, Result};

use crate::peer::Peer;

const HELP: &str = "\
USAGE:
    hx-peer [FLAGS]

FLAGS:
    -h, --help          Print help information
    --name <name>       Name shown to peers
    --log <file>        Specify a file to use for logging
    -v                  Increase logging verbosity each use for up to 3 times
";

#[derive(Default)]
struct Args {
    display_help: bool,
    name: Option<String>,
    log_file: Option<String>,
    verbosity: u64,
}

impl Args {
    fn parse_args() -> Result<Args> {
        let mut args = Args::default();
        let mut argv = std::env::args();
        argv.next();

        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "-h" | "--help" => args.display_help = true,
                "--name" => args.name = Some(argv.next().context("--name must specify a name")?),
                "--log" => {
                    args.log_file = Some(argv.next().context("--log must specify a path to write")?)
                }
                "-v" => args.verbosity += 1,
                arg => bail!("unexpected argument: {arg}"),
            }
        }
        Ok(args)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse_args().context("could not parse arguments")?;
    if args.display_help {
        print!("{HELP}");
        return Ok(());
    }

    if let Some(log_file) = args.log_file {
        let level = match args.verbosity {
            0 => log::LevelFilter::Warn,
            1 => log::LevelFilter::Info,
            2 => log::LevelFilter::Debug,
            _3_or_more => log::LevelFilter::Trace,
        };
        helix_term::logging::init_file(level, log_file.as_ref())?;
    }
    Peer::new(args.name).run().await
}
