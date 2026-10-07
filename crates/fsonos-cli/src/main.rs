//! `fsonos` — the FrankenSonos CLI and daemon entrypoint.
//!
//! Direct mode: each command surveys the LAN itself (SSDP plus `--seed` /
//! `FSONOS_SEEDS`), plans with the shared `fsonos-api` layer, and sends the
//! SOAP actions through `fsonos-core`. `--json` prints the result DTO.
//!
//!   discover   list the players on the LAN
//!   zones      show the zone groups and what each is doing
//!   play       play a renderer URI in a room's group
//!   pause / resume / next / previous   transport for a room's group
//!   volume     set (0-100) or change (+N / -N) a room's or group's volume
//!   mute       mute or unmute a room
//!   group / ungroup   move a room into another's group, or out of its own
//!   dj         the classical DJ (not wired to the speakers yet)
//!   serve      run the long-lived daemon (HTTP API + MCP server + GENA sink)
//!   mcp        serve the MCP tools over stdio (for a local agent)
//!   sim        run a virtual Sonos house on loopback (feature `sim`)

mod config;
mod direct;
#[cfg(feature = "sim")]
mod sim;

use anyhow::Context as _;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use clap::{Parser, Subcommand, ValueEnum};
use fsonos_api::plan::{self, DjAction as PlanDj, TransportAction};
use fsonos_api::{
    Failure, GroupRequest, MuteRequest, OutcomeDto, PlayRequest, VolumeRequest, ZoneRequest,
};
use fsonos_core::HouseholdState;
use serde::Serialize;
use std::process::ExitCode;

use crate::direct::Direct;

#[derive(Parser)]
#[command(
    name = "fsonos",
    version,
    about = "FrankenSonos — your Sonos, your way"
)]
struct Cli {
    #[command(flatten)]
    global: config::GlobalArgs,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the Sonos players on the LAN.
    Discover,
    /// Show the zone groups and what each is doing.
    Zones,
    /// Play a renderer URI (a radio or HTTP stream, a favorite) in a room's
    /// group.
    Play {
        /// Room name (`Room@S1` / `Room@S2` picks a household).
        zone: String,
        /// Source URI or open.spotify.com link.
        source_uri: String,
        /// Title to show for it.
        #[arg(long)]
        title: Option<String>,
    },
    /// Pause a room's group.
    Pause { zone: String },
    /// Resume a room's group.
    Resume { zone: String },
    /// Skip to the next track in a room's group.
    Next { zone: String },
    /// Go back a track in a room's group.
    Previous { zone: String },
    /// Set a room's volume (0-100) or change it (+N / -N).
    Volume {
        zone: String,
        /// `30` sets, `+5` / `-5` changes.
        #[arg(allow_negative_numbers = true)]
        level: String,
        /// Apply to the room's whole group.
        #[arg(long)]
        group: bool,
    },
    /// Mute or unmute a room.
    Mute {
        zone: String,
        #[arg(value_enum, default_value_t = Switch::On)]
        state: Switch,
    },
    /// Move a room into the group another room plays in.
    Group {
        /// The room that moves.
        zone: String,
        /// Any room in the group it joins.
        to: String,
    },
    /// Take a room out of its group.
    Ungroup { zone: String },
    /// Run the long-lived daemon (HTTP API + MCP server + event sink).
    Serve(config::ServeArgs),
    /// Serve the MCP tools over stdio (for a local agent).
    Mcp,
    /// Run a virtual Sonos house on loopback to try FrankenSonos without
    /// speakers.
    #[cfg(feature = "sim")]
    Sim(sim::SimArgs),
    /// Classical DJ controls.
    Dj {
        #[command(subcommand)]
        action: DjAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Switch {
    On,
    Off,
}

#[derive(Subcommand)]
enum DjAction {
    Start { zone: String },
    Skip { zone: String },
    Stop { zone: String },
}

/// Exit codes follow `docs/ERRORS.md`: a [`Failure`] exits with its code's
/// number (2 usage, 3 not found, 4 unreachable, 5 policy), anything else 1.
fn main() -> ExitCode {
    // Logs go to stderr: stdout carries the MCP stdio protocol.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if let Some(failure) = err.downcast_ref::<Failure>() {
                eprintln!("{}", failure.cli_text());
                ExitCode::from(failure.exit_code())
            } else {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let global = &cli.global;
    match cli.command {
        Command::Serve(args) => serve(global, &args),
        Command::Mcp => run_mcp_stdio(global),
        #[cfg(feature = "sim")]
        Command::Sim(args) => sim::run(&args),
        Command::Discover => {
            let found = Direct::survey(global)?.discover();
            for (addr, why) in &found.unreachable {
                tracing::warn!("{addr}: found but unreadable: {why}");
            }
            if let Some(err) = &found.ssdp_error {
                tracing::warn!("SSDP failed, only seeds were tried: {err}");
            }
            emit(global.json, &found, direct::discover_text)
        }
        Command::Zones => {
            let zones = Direct::survey(global)?.zones()?;
            emit(global.json, &zones, |z| direct::zones_text(z))
        }
        control => {
            let direct = Direct::survey(global)?;
            let outcome = direct.run(|households| plan_for(&control, households))?;
            emit(global.json, &outcome, |o: &OutcomeDto| {
                format!(
                    "{}
",
                    o.done
                )
            })
        }
    }
}

/// Print `value` as pretty JSON, or as `text` renders it.
fn emit<T: Serialize + ?Sized>(
    json: bool,
    value: &T,
    text: impl Fn(&T) -> String,
) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        print!("{}", text(value));
    }
    Ok(())
}

/// The shared request a control subcommand stands for, planned against
/// `households`.
fn plan_for(
    command: &Command,
    households: &[HouseholdState],
) -> Result<fsonos_api::Command, Failure> {
    let zone = |zone: &String| ZoneRequest { zone: zone.clone() };
    match command {
        Command::Play {
            zone,
            source_uri,
            title,
        } => plan::plan_play(
            households,
            &PlayRequest {
                zone: zone.clone(),
                source_uri: source_uri.clone(),
                title: title.clone(),
            },
        ),
        Command::Pause { zone: z } => {
            plan::plan_transport(households, &zone(z), TransportAction::Pause)
        }
        Command::Resume { zone: z } => {
            plan::plan_transport(households, &zone(z), TransportAction::Resume)
        }
        Command::Next { zone: z } => {
            plan::plan_transport(households, &zone(z), TransportAction::Next)
        }
        Command::Previous { zone: z } => {
            plan::plan_transport(households, &zone(z), TransportAction::Previous)
        }
        Command::Volume { zone, level, group } => {
            plan::plan_volume(households, &volume_request(zone, level, *group)?)
        }
        Command::Mute { zone, state } => plan::plan_mute(
            households,
            &MuteRequest {
                zone: zone.clone(),
                mute: *state == Switch::On,
            },
        ),
        Command::Group { zone, to } => plan::plan_group(
            households,
            &GroupRequest {
                zone: zone.clone(),
                to: to.clone(),
            },
        ),
        Command::Ungroup { zone: z } => plan::plan_ungroup(households, &zone(z)),
        Command::Dj { action } => {
            let (z, action) = match action {
                DjAction::Start { zone } => (zone, PlanDj::Start),
                DjAction::Skip { zone } => (zone, PlanDj::Skip),
                DjAction::Stop { zone } => (zone, PlanDj::Stop),
            };
            plan::plan_dj(households, &zone(z), action)
        }
        Command::Discover | Command::Zones | Command::Serve(_) | Command::Mcp => {
            unreachable!("not a control command")
        }
        #[cfg(feature = "sim")]
        Command::Sim(_) => unreachable!("not a control command"),
    }
}

/// `30` sets the volume; `+5` / `-5` change it. Ranges are checked when the
/// request is planned.
fn volume_request(zone: &str, level: &str, group: bool) -> Result<VolumeRequest, Failure> {
    let level = level.trim();
    let number = |digits: &str| {
        digits.parse::<i64>().map_err(|_| {
            Failure::invalid(format!("volume {level:?} is not a number"))
                .with_hint("Give 0-100 to set the volume, or +N / -N to change it.")
        })
    };
    let (volume, delta) = if let Some(up) = level.strip_prefix('+') {
        (None, Some(number(up)?))
    } else if level.starts_with('-') {
        (None, Some(number(level)?))
    } else {
        (Some(number(level)?), None)
    };
    Ok(VolumeRequest {
        zone: zone.to_string(),
        volume,
        delta,
        group,
    })
}

/// Vet the configuration, then run the daemon. The listeners have no
/// authentication, so an unsafe bind address stops startup here.
fn serve(global: &config::GlobalArgs, args: &config::ServeArgs) -> anyhow::Result<()> {
    for (listener, addr) in [("HTTP API", args.http), ("MCP server", args.mcp_http)] {
        match config::check_control_bind(listener, addr, args.allow_unsafe_bind) {
            Ok(None) => {}
            Ok(Some(warning)) => tracing::warn!("{warning}"),
            Err(refusal) => return Err(refusal.into()),
        }
    }
    let data_dir = data_dir(global)?;
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

/// The data directory, or `INVALID_ARGUMENT` when none can be determined.
fn data_dir(global: &config::GlobalArgs) -> Result<std::path::PathBuf, Failure> {
    global.data_dir().ok_or_else(|| {
        Failure::invalid("no data directory is configured and HOME is unset")
            .with_hint("Set FSONOS_DATA_DIR (or HOME) and start again.")
    })
}

/// Serve the MCP tools over stdio, acting on this LAN's speakers as the
/// `mcp-stdio` client of the house policy.
fn run_mcp_stdio(global: &config::GlobalArgs) -> anyhow::Result<()> {
    let policy = fsonos_core::policy::Policy::load(&data_dir(global)?).map_err(|e| {
        Failure::invalid(e.to_string()).with_hint("Fix policy.toml in the data directory.")
    })?;
    let seeds = global.seed_addrs()?;
    let wait = global.wait();
    let survey: fsonos_mcp::tools::Survey = Box::new(move |transport| {
        Ok(fsonos_core::inventory::survey(transport, &seeds, wait)?.households)
    });
    let lan = global.lan()?;
    let backend = fsonos_mcp::tools::Backend::new(
        Box::new(lan),
        survey,
        policy,
        fsonos_core::policy::Client::McpStdio,
        Box::new(fsonos_core::clock::SystemClock),
    );
    if !fsonos_mcp::tools::install(backend) {
        anyhow::bail!("the MCP backend is already installed");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_api::plan::VolumeScope;
    use fsonos_api::{Command as Planned, VolumeChange};
    use fsonos_core::Room;
    use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(std::iter::once("fsonos").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
            .command
    }

    /// One S2 household: `Den` coordinating `Kitchen`.
    fn house() -> Vec<HouseholdState> {
        let id = |s: &str| PlayerId(s.into());
        let player = |pid: &str, room: &str| Player {
            id: id(pid),
            room_name: room.into(),
            ip: "192.0.2.10".parse().unwrap(),
            model: String::new(),
            generation: Generation::S2,
        };
        let room = |name: &str, pid: &str| Room {
            name: name.into(),
            primary: id(pid),
            players: vec![id(pid)],
            missing: Vec::new(),
            coordinator: id("RINCON_DEN"),
        };
        vec![HouseholdState {
            players: vec![player("RINCON_DEN", "Den"), player("RINCON_KIT", "Kitchen")],
            groups: vec![ZoneGroup {
                coordinator: id("RINCON_DEN"),
                members: vec![id("RINCON_DEN"), id("RINCON_KIT")],
            }],
            rooms: vec![room("Den", "RINCON_DEN"), room("Kitchen", "RINCON_KIT")],
            ..Default::default()
        }]
    }

    #[test]
    fn volume_levels_set_or_change() {
        let set = volume_request("Den", "30", false).unwrap();
        assert_eq!((set.volume, set.delta), (Some(30), None));
        let up = volume_request("Den", "+5", true).unwrap();
        assert_eq!((up.volume, up.delta, up.group), (None, Some(5), true));
        let down = volume_request("Den", " -12 ", false).unwrap();
        assert_eq!((down.volume, down.delta), (None, Some(-12)));
        let bad = volume_request("Den", "loud", false).unwrap_err();
        assert_eq!(bad.exit_code(), 2);
        assert!(bad.detail.contains("\"loud\" is not a number"));
    }

    #[test]
    fn negative_volume_is_a_value_not_a_flag() {
        let Command::Volume { level, group, .. } = parse(&["volume", "Kitchen", "-5"]) else {
            panic!("expected volume")
        };
        assert_eq!((level.as_str(), group), ("-5", false));
    }

    #[test]
    fn subcommands_plan_to_the_right_players() {
        let houses = house();
        let plan = |args: &[&str]| plan_for(&parse(args), &houses).unwrap();
        assert_eq!(
            plan(&["pause", "kitchen"]),
            Planned::Transport {
                coordinator: PlayerId("RINCON_DEN".into()),
                action: TransportAction::Pause
            }
        );
        assert_eq!(
            plan(&["volume", "Kitchen", "-5"]),
            Planned::Volume {
                target: PlayerId("RINCON_KIT".into()),
                scope: VolumeScope::Room,
                change: VolumeChange::Adjust(-5)
            }
        );
        assert_eq!(
            plan(&["volume", "Kitchen", "40", "--group"]),
            Planned::Volume {
                target: PlayerId("RINCON_DEN".into()),
                scope: VolumeScope::Group,
                change: VolumeChange::Set(40)
            }
        );
        assert_eq!(
            plan(&["mute", "Kitchen"]),
            Planned::Mute {
                target: PlayerId("RINCON_KIT".into()),
                mute: true
            }
        );
        assert_eq!(
            plan(&["mute", "Kitchen", "off"]),
            Planned::Mute {
                target: PlayerId("RINCON_KIT".into()),
                mute: false
            }
        );
        assert_eq!(
            plan(&["ungroup", "Kitchen"]),
            Planned::Leave {
                member: PlayerId("RINCON_KIT".into())
            }
        );
        assert!(matches!(
            plan(&["group", "Kitchen", "Den"]),
            Planned::Nothing { .. }
        ));
        assert!(matches!(
            plan(&["play", "Den", "x-rincon-mp3radio://stream.example.org/a.mp3", "--title", "Radio"]),
            Planned::Play { title: Some(t), .. } if t == "Radio"
        ));
    }

    #[test]
    fn global_options_go_anywhere() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "zones",
            "--json",
            "--seed",
            "192.0.2.10",
            "--wait",
            "1",
        ])
        .unwrap();
        assert!(cli.global.json);
        assert_eq!(
            cli.global.seed,
            ["192.0.2.10".parse::<std::net::IpAddr>().unwrap()]
        );
        assert_eq!(cli.global.wait().as_secs(), 1);
    }

    #[test]
    fn unknown_rooms_fail_with_suggestions() {
        let err = plan_for(&parse(&["pause", "Kitchn"]), &house()).unwrap_err();
        assert_eq!(
            (err.code, err.exit_code()),
            (fsonos_api::ErrorCode::UnknownRoom, 3)
        );
        assert_eq!(err.suggestions, ["Kitchen@S2"]);
    }
}
