//! `fsonos` — the FrankenSonos CLI and daemon entrypoint.
//!
//! Subcommands (wired to real behavior as the lanes land):
//!   discover   list players found on the LAN
//!   zones      show current zone-group topology
//!   play       render a source URI on a zone
//!   serve      run the long-lived daemon (HTTP API + MCP server + GENA sink)
//!   mcp        serve the MCP tools over stdio (for a local agent)
//!   dj         start/stop/skip the classical DJ
//!
//! Commands not yet wired report which lane delivers them.

mod config;

use anyhow::Context as _;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "fsonos",
    version,
    about = "FrankenSonos — your Sonos, your way"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List Sonos players discovered on the LAN.
    Discover,
    /// Show current zone-group topology.
    Zones,
    /// Render a source URI on a zone.
    Play {
        /// Room/zone name.
        zone: String,
        /// Source URI (e.g. spotify:track:...).
        source_uri: String,
    },
    /// Run the long-lived daemon (HTTP API + MCP server + event sink).
    Serve(config::ServeArgs),
    /// Serve the MCP tools over stdio (for a local agent).
    Mcp,
    /// Classical DJ controls.
    Dj {
        #[command(subcommand)]
        action: DjAction,
    },
}

#[derive(Subcommand)]
enum DjAction {
    Start { zone: String },
    Skip { zone: String },
    Stop { zone: String },
}

fn main() -> anyhow::Result<()> {
    // Logs go to stderr: stdout carries the MCP stdio protocol.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Discover => pending("discover (lane: proto FND-DEPS)"),
        Command::Zones => pending("zones (lane: core)"),
        Command::Play { zone, source_uri } => pending(&format!(
            "play {source_uri} on {zone} (lanes: proto + core + spotify)"
        )),
        Command::Serve(args) => serve(&args),
        Command::Mcp => run_mcp_stdio(),
        Command::Dj { action } => {
            let what = match action {
                DjAction::Start { zone } => format!("dj start {zone}"),
                DjAction::Skip { zone } => format!("dj skip {zone}"),
                DjAction::Stop { zone } => format!("dj stop {zone}"),
            };
            pending(&format!("{what} (lane: spotify dj)"))
        }
    }
}

/// Vet the configuration, then run the daemon. The listeners have no
/// authentication, so an unsafe bind address stops startup here.
fn serve(args: &config::ServeArgs) -> anyhow::Result<()> {
    for (listener, addr) in [("HTTP API", args.http), ("MCP server", args.mcp_http)] {
        match config::check_control_bind(listener, addr, args.allow_unsafe_bind) {
            Ok(None) => {}
            Ok(Some(warning)) => tracing::warn!("{warning}"),
            Err(refusal) => anyhow::bail!(refusal),
        }
    }
    let data_dir = args
        .data_dir()
        .context("no data directory: set FSONOS_DATA_DIR or HOME")?;
    pending(&format!(
        "serve (api {}, mcp {}, data {}) (lanes: api + mcp + core)",
        args.http,
        args.mcp_http,
        data_dir.display()
    ))
}

/// The single-threaded asupersync runtime every surface runs under.
fn runtime() -> anyhow::Result<Runtime> {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().context("create I/O reactor")?)
        .blocking_threads(0, 16)
        .build()
        .context("build asupersync runtime")
}

fn run_mcp_stdio() -> anyhow::Result<()> {
    runtime()?.block_on(async {
        let cx = asupersync::Cx::current().context("runtime installs an ambient Cx")?;
        fsonos_mcp::server().run_stdio_with_cx(&cx).await
    })
}

#[allow(clippy::unnecessary_wraps)] // stands in for commands that will be fallible
fn pending(what: &str) -> anyhow::Result<()> {
    tracing::info!("FrankenSonos: `{what}` is not wired yet — see the plan and beads.");
    println!("not-yet-implemented: {what}");
    Ok(())
}
