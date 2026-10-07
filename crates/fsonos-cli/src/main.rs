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
    Serve {
        /// HTTP API bind address.
        #[arg(long, default_value = "127.0.0.1:8099")]
        http: String,
    },
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
        Command::Serve { http } => pending(&format!("serve on {http} (lanes: api + mcp + core)")),
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
