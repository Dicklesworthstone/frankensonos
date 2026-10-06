//! `fsonos` — the FrankenSonos CLI and daemon entrypoint.
//!
//! Subcommands (wired to real behavior as the lanes land):
//!   discover   list players found on the LAN
//!   zones      show current zone-group topology
//!   play       render a source URI on a zone
//!   serve      run the long-lived daemon (HTTP API + MCP server + GENA sink)
//!   dj         start/stop/skip the classical DJ
//!
//! Today it parses arguments and reports that behavior arrives with FND-DEPS +
//! the feature lanes, so the binary builds and runs from commit #1.

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Discover => pending("discover (lane: proto FND-DEPS)"),
        Command::Zones => pending("zones (lane: core)"),
        Command::Play { zone, source_uri } => pending(&format!(
            "play {source_uri} on {zone} (lanes: proto + core + spotify)"
        )),
        Command::Serve { http } => pending(&format!("serve on {http} (lanes: api + mcp + core)")),
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

fn pending(what: &str) -> anyhow::Result<()> {
    tracing::info!("FrankenSonos: `{what}` is not wired yet — see the plan and beads.");
    println!("not-yet-implemented: {what}");
    Ok(())
}
