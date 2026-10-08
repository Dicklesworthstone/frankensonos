//! `fsonos` — the FrankenSonos CLI and daemon entrypoint.
//!
//! Direct mode: each command surveys the LAN itself (SSDP plus `--seed` /
//! `FSONOS_SEEDS`), plans with the shared `fsonos-api` layer, and sends the
//! SOAP actions through `fsonos-core`. `--json` prints the result DTO.
//!
//!   discover   list the players on the LAN
//!   zones      show the zone groups and what each is doing
//!   status     what a room is doing (track, transport, volume)
//!   favorites  the household's Sonos favorites, numbered
//!   log / undo the action log, and undoing the newest action
//!   doctor     diagnose the setup (exit 0 / 6 warnings / 7 failures)
//!   play       play a source URI, or `--favorite <name>`, in a room's group
//!   pause / resume / next / previous   transport for a room's group
//!   volume     set (0-100) or change (+N / -N) a room's or group's volume
//!   mute       mute or unmute a room
//!   group / ungroup   move a room into another's group, or out of its own
//!   dj         the classical DJ (not wired to the speakers yet)
//!   serve      run the long-lived daemon (HTTP API + MCP over HTTP)
//!   mcp        serve the MCP tools over stdio (for a local agent)
//!   tailscale  put Tailscale Serve in front of the daemon (setup / status /
//!              teardown): HTTPS for the tailnet, never Funnel
//!   sim        run a virtual Sonos house on loopback (feature `sim`)

mod config;
mod confine;
mod daemon;
mod direct;
mod doctor;
#[cfg(feature = "sim")]
mod sim;
mod tailscale_cmd;

use anyhow::Context as _;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use clap::{Parser, Subcommand, ValueEnum};
use fsonos_api::plan::{self, DjAction as PlanDj, TransportAction};
use fsonos_api::{
    ErrorCode, Failure, GroupRequest, HitDto, MuteRequest, OutcomeDto, PlayFavoriteRequest,
    PlayRequest, SearchRequest, VolumeRequest, ZoneRequest,
};
use fsonos_core::HouseholdState;
use serde::Serialize;
use std::fmt::Write as _;
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
    /// What a room is doing: its group, the track or station, its volume.
    Status { zone: String },
    /// List the Sonos favorites of a room's household, numbered.
    Favorites { zone: String },
    /// Diagnose the setup: speakers, Spotify linkage, listener addresses,
    /// the daemon. Exits 0 (all pass), 6 (warnings) or 7 (a failure).
    Doctor(doctor::DoctorArgs),
    /// The action log, newest first: who did what, the policy's verdict, and
    /// whether it can be undone.
    Log {
        /// Only this client's actions (`cli`, `mcp-stdio`, a tailnet login).
        #[arg(long)]
        client: Option<String>,
        /// Only the last `30m`, `2h`, `1d`, `90s`...
        #[arg(long, value_name = "AGE")]
        since: Option<String>,
        /// At most this many.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Undo the newest action: restore the volumes, grouping and what was
    /// playing in the zones it changed.
    Undo {
        /// Only the CLI's own newest action, not an agent's.
        #[arg(long)]
        mine: bool,
    },
    /// Play a source URI, or one of the household's favorites, in a room's
    /// group.
    Play {
        /// Room name (`Room@S1` / `Room@S2` picks a household).
        zone: String,
        /// Source URI or open.spotify.com track link.
        source_uri: Option<String>,
        /// Play this favorite instead: a title (a unique prefix or all its
        /// words will do), its number from `fsonos favorites`, or an id.
        #[arg(long, conflicts_with = "source_uri")]
        favorite: Option<String>,
        /// Search the library and the room's favorites, and play the best
        /// match (title words, a composer or performer, "bwv 988").
        #[arg(long, value_name = "QUERY", conflicts_with_all = ["source_uri", "favorite"])]
        search: Option<String>,
        /// With --search: list the matches instead (`--pick`), or play the
        /// Nth of them (`--pick 3`).
        #[arg(
            long,
            value_name = "N",
            requires = "search",
            num_args = 0..=1,
            default_missing_value = "0"
        )]
        pick: Option<usize>,
        /// Title to show for a source URI.
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
    /// Front the daemon with Tailscale Serve (HTTPS on the tailnet, never
    /// Funnel): `setup`, `status`, `teardown`.
    Tailscale(tailscale_cmd::TailscaleArgs),
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

    let cli = Cli::parse();
    if let Command::Doctor(args) = &cli.command {
        return match doctor::run(&cli.global, args) {
            Ok(code) => code,
            Err(err) => report_error(&err),
        };
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => report_error(&err),
    }
}

/// Print `err` the `docs/ERRORS.md` way and pick its exit code.
fn report_error(err: &anyhow::Error) -> ExitCode {
    if let Some(failure) = err.downcast_ref::<Failure>() {
        eprintln!("{}", failure.cli_text());
        ExitCode::from(failure.exit_code())
    } else {
        eprintln!("error: {err:#}");
        ExitCode::FAILURE
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let global = &cli.global;
    match cli.command {
        Command::Serve(args) => daemon::run(global, &args),
        Command::Mcp => run_mcp_stdio(global),
        Command::Tailscale(args) => tailscale_cmd::run(global, &args),
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
        Command::Status { zone } => {
            let state = Direct::survey(global)?.status(&zone)?;
            emit(global.json, &state, direct::status_text)
        }
        Command::Favorites { zone } => {
            let favorites = Direct::survey(global)?.favorites(&zone)?;
            emit(global.json, &favorites, |f| direct::favorites_text(f))
        }
        Command::Log {
            client,
            since,
            limit,
        } => {
            let since = since.as_deref().map(seconds_ago).transpose()?;
            let query = fsonos_api::ActionsQuery {
                client,
                since,
                limit: Some(limit),
            };
            let actions = direct::actions(global, &query)?;
            emit(global.json, &actions, |a| direct::actions_text(a))
        }
        Command::Undo { mine } => {
            let undone = Direct::survey(global)?.undo(mine)?;
            emit(global.json, &undone, |u: &fsonos_api::UndoDto| {
                format!("{}\n", u.summary)
            })
        }
        Command::Play {
            zone,
            search: Some(query),
            pick,
            ..
        } => play_search(global, zone, query, pick),
        Command::Play {
            zone,
            favorite: Some(favorite),
            ..
        } => {
            let req = PlayFavoriteRequest { zone, favorite };
            let outcome = Direct::survey(global)?.play_favorite(&req)?;
            emit(global.json, &outcome, |o: &OutcomeDto| {
                format!("{}\n", o.done)
            })
        }
        control => {
            let direct = Direct::survey(global)?;
            let outcome = direct.run(tool_name(&control), |households| {
                plan_for(&control, households)
            })?;
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

/// `fsonos play <room> --search <query> [--pick [N]]`: play the best match
/// (or the Nth), or with a bare `--pick` list the matches.
fn play_search(
    global: &config::GlobalArgs,
    zone: String,
    query: String,
    pick: Option<usize>,
) -> anyhow::Result<()> {
    let direct = Direct::survey(global)?;
    let req = SearchRequest {
        query,
        zone: Some(zone.clone()),
        limit: Some(if pick.is_some() { 10 } else { 1 }),
    };
    let hits = direct.search(&req)?;
    if hits.is_empty() {
        return Err(Failure::new(
            ErrorCode::NoMatch,
            format!(
                "nothing in the library or {zone}'s favorites matches {:?}",
                req.query
            ),
        )
        .into());
    }
    if pick == Some(0) {
        return emit(global.json, &hits, |hits: &Vec<HitDto>| {
            hits.iter()
                .enumerate()
                .fold(String::new(), |mut out, (i, h)| {
                    let _ = write!(out, "{}. {}", i + 1, h.title);
                    if let Some(by) = &h.subtitle {
                        let _ = write!(out, " ({by})");
                    }
                    let _ = writeln!(out, " [{}]", h.kind);
                    out
                })
        });
    }
    let n = pick.unwrap_or(1);
    let hit = hits.get(n - 1).ok_or_else(|| {
        Failure::invalid(format!("--pick {n}, but only {} matched", hits.len()))
            .with_hint("List the matches with --pick and choose one of their numbers.")
    })?;
    let outcome = match (&hit.source_uri, &hit.favorite) {
        (Some(uri), _) => {
            let play = PlayRequest {
                zone,
                source_uri: uri.clone(),
                title: Some(hit.title.clone()),
            };
            direct.run("play", |h| plan::plan_play(h, &play))?
        }
        (None, Some(id)) => direct.play_favorite(&PlayFavoriteRequest {
            zone,
            favorite: id.clone(),
        })?,
        (None, None) => {
            return Err(
                Failure::new(ErrorCode::Internal, "a search hit with nothing to play").into(),
            );
        }
    };
    emit(global.json, &outcome, |o: &OutcomeDto| {
        format!("{}\n", o.done)
    })
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
            ..
        } => {
            let source_uri = source_uri.clone().ok_or_else(|| {
                Failure::invalid("nothing to play")
                    .with_hint("Give a source URI, or --favorite <name> (see fsonos favorites).")
            })?;
            plan::plan_play(
                households,
                &PlayRequest {
                    zone: zone.clone(),
                    source_uri,
                    title: title.clone(),
                },
            )
        }
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
        Command::Discover
        | Command::Zones
        | Command::Doctor(_)
        | Command::Status { .. }
        | Command::Favorites { .. }
        | Command::Log { .. }
        | Command::Undo { .. }
        | Command::Serve(_)
        | Command::Tailscale(_)
        | Command::Mcp => {
            unreachable!("not a control command")
        }
        #[cfg(feature = "sim")]
        Command::Sim(_) => unreachable!("not a control command"),
    }
}

/// The house-policy tool name a control subcommand runs as (the MCP tool of
/// the same action), so policy rules and the action log read alike.
fn tool_name(command: &Command) -> &'static str {
    match command {
        Command::Play { .. } => "play",
        Command::Pause { .. } => "pause",
        Command::Resume { .. } => "resume",
        Command::Next { .. } => "next",
        Command::Previous { .. } => "previous",
        Command::Volume { .. } => "set_volume",
        Command::Mute { .. } => "mute",
        Command::Group { .. } => "group",
        Command::Ungroup { .. } => "ungroup",
        Command::Dj { action } => match action {
            DjAction::Start { .. } => "dj_start",
            DjAction::Skip { .. } => "dj_skip",
            DjAction::Stop { .. } => "dj_stop",
        },
        _ => "cli",
    }
}

/// `30m`, `2h`, `1d`, `90s` (or bare seconds) before now, as unix seconds.
fn seconds_ago(age: &str) -> Result<i64, Failure> {
    let age = age.trim();
    let (digits, unit) = age.split_at(age.find(|c: char| !c.is_ascii_digit()).unwrap_or(age.len()));
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => 0,
    };
    let amount: i64 = digits.parse().unwrap_or(0);
    if scale == 0 || digits.is_empty() {
        return Err(Failure::invalid(format!("--since {age:?} is not an age"))
            .with_hint("Use a number with s, m, h or d, e.g. --since 2h."));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    Ok(now - amount.saturating_mul(scale))
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

/// The single-threaded asupersync runtime every surface runs under.
fn runtime() -> anyhow::Result<Runtime> {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().context("create I/O reactor")?)
        .blocking_threads(0, 16)
        .build()
        .context("build asupersync runtime")
}

/// Serve the MCP tools over stdio, acting on this LAN's speakers as the
/// `mcp-stdio` client of the house policy.
fn run_mcp_stdio(global: &config::GlobalArgs) -> anyhow::Result<()> {
    let data_dir = daemon::data_dir(global)?;
    let surface = daemon::surface(global, daemon::policy(&data_dir)?)?;
    let surface = std::sync::Arc::new(daemon::with_action_log(surface, &data_dir, "mcp"));
    let backend =
        fsonos_mcp::tools::Backend::shared(surface, fsonos_core::policy::Client::McpStdio);
    if !fsonos_mcp::tools::install(backend) {
        anyhow::bail!("the MCP backend is already installed");
    }
    runtime()?.block_on(async {
        let cx = asupersync::Cx::current().context("runtime installs an ambient Cx")?;
        fsonos_mcp::server().run_stdio_with_cx(&cx).await
    })
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
