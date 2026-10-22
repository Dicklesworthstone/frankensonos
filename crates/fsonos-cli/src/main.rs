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
//!   policy     the house policy in effect (show), and whether policy.toml
//!              can be used (check)
//!   doctor     diagnose the setup (exit 0 / 6 warnings / 7 failures)
//!   setup      a guided first run on the doctor's checks (--yes for scripts)
//!   play       play a source URI, or `--favorite <name>`, in a room's group
//!   pause / resume / next / previous   transport for a room's group
//!   queue-play play an existing queue position without adding tracks
//!   seek       move within the track a room's group is on (1:30, +30, -15)
//!   volume     set (0-100) or change (+N / -N) a room's or group's volume
//!   mute       mute or unmute a room
//!   group / ungroup   move a room into another's group, or out of its own
//!   sleep      pause a room's group after a while (serve fades it out)
//!   schedule   start the DJ, pause or set a volume at set times (serve
//!              runs them)
//!   scene      save, apply (undoable), list, show or rm the house's named
//!              states
//!   dj         the DJ: start, skip, stop, steer; status, why,
//!              moods
//!   serve      run the long-lived daemon (HTTP API + MCP over HTTP)
//!   mcp        serve the MCP tools over stdio (for a local agent)
//!   tailscale  put Tailscale Serve in front of the daemon (setup / status /
//!              teardown): HTTPS for the tailnet, never Funnel
//!   sim        run a virtual Sonos house on loopback (feature `sim`)

mod announce_cmd;
mod completions;
mod config;
mod confine;
mod daemon;
mod direct;
mod dj;
mod dj_cmd;
mod dj_view;
mod doctor;
mod eq_cmd;
mod events_cmd;
mod known_players;
mod play_mode_cmd;
mod policy_cmd;
mod queue_edit_cmd;
mod queue_play_cmd;
mod queue_search_cmd;
mod remote;
mod rooms_cmd;
mod scene_cmd;
mod schedule_cmd;
mod selftest_cmd;
mod service_cmd;
mod setup_cmd;
mod setup_spotify;
#[cfg(feature = "sim")]
mod sim;
mod streaming;
mod tailscale_cmd;

use anyhow::Context as _;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use clap::{Parser, Subcommand, ValueEnum};
use fsonos_api::plan::{self, TransportAction};
use fsonos_api::surface::queue::{QueueAddedDto, QueueDto, QueueRequest};
use fsonos_api::surface::schedules::{FadePauseRequest, FadeVolumeRequest};
use fsonos_api::{
    ErrorCode, Failure, GroupRequest, HitDto, MoveRequest, MuteRequest, OutcomeDto, PartyRequest,
    PlayFavoriteRequest, PlayRequest, SearchPlayRequest, SearchRequest, VolumeRequest, ZoneRequest,
};
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
    /// Show the zone groups and what each is doing; with a room, the zone
    /// it plays in.
    Zones { zone: Option<String> },
    /// Search the library (your tracks, saved albums and playlists) and,
    /// with --zone, a room's favorites: the best matches, best first.
    Search {
        /// What to look for: title words, a composer or performer, "bwv 988".
        #[arg(required = true)]
        query: Vec<String>,
        /// A room: its household's Sonos favorites are searched too.
        #[arg(long)]
        zone: Option<String>,
        /// At most this many (1-50).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=50))]
        limit: u32,
    },
    /// What a room is doing: its group, the track or station, its volume.
    Status { zone: String },
    /// List the Sonos favorites of a room's household, numbered.
    Favorites { zone: String },
    /// What played lately, newest first: when, the room whose group played
    /// it, and what.
    History {
        /// Only this room's group.
        zone: Option<String>,
        /// At most this many (1-200).
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=200))]
        limit: u32,
    },
    /// The queue a room's group plays from: its tracks, and which one plays.
    Queue {
        zone: String,
        /// Tracks to skip from the start.
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// At most this many (1-100).
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
    },
    /// Find title, artist or album words in the existing group queue. Returns
    /// absolute positions, exact renderer URIs and a revision for guarded actions.
    QueueSearch(queue_search_cmd::QueueSearchArgs),
    /// Play an existing queue position without adding or removing tracks.
    /// Use `queue <room>` or `queue-search` to find positions first; `undo`
    /// restores playback.
    QueuePlay(queue_play_cmd::QueuePlayArgs),
    /// Remove an identified consecutive range. Read queue --json first.
    /// Requires its revision and renderer URIs, or --snapshot. Not undoable.
    QueueRemove(queue_edit_cmd::QueueRemoveArgs),
    /// Move an identified range before an ORIGINAL queue position.
    /// Requires a revision-bound selection; never rebuilds the queue.
    QueueMove(queue_edit_cmd::QueueMoveArgs),
    /// Read or set the group's complete shuffle/repeat mode, without starting
    /// playback. No MODE is a read; --expected-mode rejects a stale setting.
    PlayMode(play_mode_cmd::PlayModeArgs),
    /// A room's tone: bass and treble (-10 to 10, 0 is flat) and loudness.
    /// `--bass`, `--treble` or `--loudness` change them, and `fsonos undo`
    /// puts them back.
    Eq(eq_cmd::EqArgs),
    /// Diagnose the setup: speakers, Spotify linkage, listener addresses,
    /// the daemon. Exits 0 (all pass), 6 (warnings) or 7 (a failure).
    Doctor(doctor::DoctorArgs),
    /// Live acceptance checks on this house's real players: every read the
    /// controller relies on, favorites and queues, and writes that change
    /// nothing audible (verified before and after). The report names no
    /// room, address or id, so it can be shared. Exits as `doctor` does.
    Selftest(selftest_cmd::SelftestArgs),
    /// A guided first run: the data directory, finding the players (and
    /// writing seeds.toml when multicast cannot), the households, Spotify,
    /// Tailscale, and what to run next. Exits as `doctor` does.
    Setup(setup_cmd::SetupArgs),
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
    /// The house's changes as they happen, through the daemon: a room's
    /// volume or play state, a player going offline, each logged action,
    /// the DJ's picks. `--json` prints an object per line.
    Events(events_cmd::EventsArgs),
    /// Undo the newest action: restore the volumes, grouping and what was
    /// playing in the zones it changed. To be sure what is undone,
    /// `--preview` shows the CLI's own newest undoable action, changing
    /// nothing, and `--id` then undoes exactly that one.
    Undo {
        /// Only the CLI's own newest action, not an agent's.
        #[arg(long, conflicts_with_all = ["preview", "id"])]
        mine: bool,
        /// Show the CLI's own newest undoable action; change nothing.
        #[arg(long, conflicts_with = "id")]
        preview: bool,
        /// Undo exactly the CLI's action with this id (the one `--preview`
        /// shows), and only while it is still the CLI's newest undoable one:
        /// otherwise nothing is restored (INVALID_ARGUMENT).
        #[arg(long, value_name = "ID")]
        id: Option<i64>,
        /// Put the zones back even where someone changed them since the
        /// action (in the Sonos app, or another caller); by default those
        /// are left as they are, and named.
        #[arg(long, conflicts_with_all = ["preview", "id"])]
        force: bool,
    },
    /// The house policy: what is in effect (show), and whether policy.toml
    /// can be used (check).
    Policy {
        #[command(subcommand)]
        action: policy_cmd::PolicyAction,
    },
    /// Play a source URI, or one of the household's favorites, in a room's
    /// group.
    // What --next and --queue add: a source URI or a favorite.
    #[command(group(clap::ArgGroup::new("item").args(["source_uri", "favorite"])))]
    Play {
        /// Room name (`Room@S1` / `Room@S2` picks a household).
        zone: String,
        /// Source URI, or a Spotify track, album or playlist (an
        /// open.spotify.com link will do); an album or playlist replaces the
        /// group's queue.
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
        // clap waives a `requires` once an argument that conflicts with the
        // required one is present (`--favorite` conflicts with `--search`
        // and the source URI), so each of these flags also conflicts with
        // the alternatives directly.
        #[arg(
            long,
            value_name = "N",
            requires = "search",
            conflicts_with_all = ["source_uri", "favorite"],
            num_args = 0..=1,
            default_missing_value = "0"
        )]
        pick: Option<usize>,
        /// Title to show for a source URI.
        #[arg(long)]
        title: Option<String>,
        /// Add the Spotify track, album or playlist, or the favorite, to the
        /// group's queue, right after the track playing, instead of playing
        /// it now (`fsonos undo` takes it off again).
        #[arg(long, requires = "item", conflicts_with_all = ["queue", "search"])]
        next: bool,
        /// Add it to the end of the group's queue instead.
        #[arg(long, requires = "item", conflicts_with = "search")]
        queue: bool,
    },
    /// Pause a room's group.
    Pause {
        zone: String,
        /// Fade the group out over this long first, then pause and put its
        /// volumes back (`10`, `10s` or `1m`; at most 60 s).
        #[arg(long, value_parser = parse_fade)]
        fade: Option<u32>,
    },
    /// Resume a room's group.
    Resume { zone: String },
    /// Skip to the next track in a room's group.
    Next { zone: String },
    /// Go back a track in a room's group.
    Previous { zone: String },
    /// Move within the track a room's group is on.
    Seek {
        zone: String,
        /// `1:30` or `90` from the track's start (`0` starts it over);
        /// `+30` / `-15` seconds from where it is now.
        #[arg(allow_hyphen_values = true)]
        position: String,
    },
    /// Set a room's volume (0-100) or change it (+N / -N).
    Volume {
        zone: String,
        /// `30` sets, `+5` / `-5` changes.
        #[arg(allow_negative_numbers = true)]
        level: String,
        /// Apply to the room's whole group.
        #[arg(long)]
        group: bool,
        /// Ramp each room to the level over this long rather than at once
        /// (`10`, `10s` or `1m`; at most 60 s), or `auto` to let each speaker
        /// ramp itself at its own pace (about 0.8 s a step).
        #[arg(long, value_parser = parse_ramp)]
        fade: Option<u32>,
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
    /// Take a room out of its group (`everywhere` breaks every group up).
    Ungroup { zone: String },
    /// Move the music a room plays to another room (handing the group over
    /// in one household; across households use --copy).
    Move {
        /// The room the music is in.
        zone: String,
        /// The room it moves to.
        to: String,
        /// Replay it there (same track and position) and stop it here: the
        /// only way between S1 and S2.
        #[arg(long)]
        copy: bool,
    },
    /// Party mode: group every room of a household, led by a room (its
    /// group) or a household's playing group (`fsonos party S2`).
    Party {
        /// A room to lead, or a household (S1, S2, or its id); none with a
        /// single household.
        target: Option<String>,
    },
    /// Say something with the local speech backend, then put the music back.
    Say(announce_cmd::SayArgs),
    /// Play a chime (bell, beep, rise) in rooms, then put the music back.
    Chime(announce_cmd::ChimeArgs),
    /// Play a local WAV in rooms, then put the music back.
    Announce(announce_cmd::AnnounceArgs),
    /// A completion script for your shell, completing room names too:
    /// `fsonos completions zsh > ~/.zfunc/_fsonos`.
    Completions { shell: completions::Shell },
    /// The cached room names, one per line, for the completion scripts.
    #[command(name = "__complete-rooms", hide = true)]
    CompleteRooms,
    /// Run a live computer-audio capture on this LAN host, until Ctrl-C.
    Stream(streaming::StreamArgs),
    /// Run the long-lived daemon (HTTP API + MCP server + event sink).
    Serve {
        #[command(flatten)]
        args: config::ServeArgs,
        #[command(flatten)]
        scheduler: schedule_cmd::SchedulerArgs,
    },
    /// Pause a room's group after a while: `fsonos sleep Bedroom 45m`,
    /// `--extend 15m`, `--cancel`, or with no duration show its timer.
    /// Through a running `fsonos serve` the group fades out first; on its
    /// own (or `--direct`) the CLI sets the speaker's own timer (no fade).
    Sleep(schedule_cmd::SleepArgs),
    /// Schedules, run by `fsonos serve`: add, list, rm, pause, resume.
    Schedule {
        #[command(subcommand)]
        action: schedule_cmd::ScheduleCommand,
    },
    /// Serve the MCP tools over stdio (for a local agent).
    Mcp {
        /// Which tools to offer: `essentials` (the 15 most agent tasks use,
        /// a much shorter tools/list) or `full` (every tool). The house
        /// policy applies the same either way.
        #[arg(long, value_enum, env = "FSONOS_MCP_PROFILE", default_value = "full")]
        profile: McpProfile,
    },
    /// Front the daemon with Tailscale Serve (HTTPS on the tailnet, never
    /// Funnel): `setup`, `status`, `teardown`.
    Tailscale(tailscale_cmd::TailscaleArgs),
    /// Run `fsonos serve` as a service of your user (a LaunchAgent on macOS,
    /// a systemd user unit on Linux): `install`, `status`, `uninstall`.
    Service(service_cmd::ServiceArgs),
    /// Run a virtual Sonos house on loopback to try FrankenSonos without
    /// speakers.
    #[cfg(feature = "sim")]
    Sim(sim::SimArgs),
    /// The DJ: start, skip, stop and steer it; what it plays, why,
    /// and its moods.
    Dj {
        #[command(subcommand)]
        action: dj_cmd::DjAction,
    },
    /// Scenes: the house's named states (grouping, volumes, what plays).
    /// `apply` sends only the steps the house needs; `fsonos undo` puts
    /// it back.
    Scene(scene_cmd::SceneArgs),
    /// Every room with its household, zone and aliases; `fsonos rooms alias
    /// add|rm` edits aliases.toml (comments and layout are kept).
    Rooms {
        #[command(subcommand)]
        alias: Option<RoomsCommand>,
    },
}

#[derive(Subcommand)]
enum RoomsCommand {
    /// The owner's names for rooms, in aliases.toml in the data directory.
    Alias {
        #[command(subcommand)]
        action: AliasAction,
    },
    /// The room `here` means: `fsonos rooms default Office`, or for one
    /// client's requests with `--client alice@example.com`. `--clear` drops
    /// it; with neither, show it.
    Default {
        room: Option<String>,
        /// Whose requests: a policy client (a tailnet login, `cli`, ...).
        #[arg(long)]
        client: Option<String>,
        #[arg(long, conflicts_with = "room")]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum AliasAction {
    /// Name one room, or a set: `fsonos rooms alias add downstairs Kitchen
    /// "Living Room"`. Any command's room then accepts the alias.
    Add {
        alias: String,
        #[arg(required = true)]
        rooms: Vec<String>,
    },
    /// Remove an alias.
    #[command(visible_alias = "remove")]
    Rm { alias: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Switch {
    On,
    Off,
}

/// The log filter: `RUST_LOG`, or `info`. The store's database logs every
/// statement at `info` (and runtime notices at `warn`), which would bury the
/// daemon's own log, so it is held to errors unless the filter names it.
fn log_spec(rust_log: Option<String>) -> String {
    let spec = rust_log.unwrap_or_else(|| "info".into());
    if spec.contains("fsqlite") {
        spec
    } else {
        format!("{spec},fsqlite=error")
    }
}

/// Exit codes follow `docs/ERRORS.md`: a [`Failure`] exits with its code's
/// number (2 usage, 3 not found, 4 unreachable, 5 policy), anything else 1.
fn main() -> ExitCode {
    // Logs go to stderr: stdout carries the MCP stdio protocol.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(log_spec(std::env::var("RUST_LOG").ok()))
                .unwrap_or_else(|_| "info,fsqlite=error".into()),
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
    if let Command::Selftest(args) = &cli.command {
        return match selftest_cmd::run(&cli.global, args) {
            Ok(code) => code,
            Err(err) => report_error(&err),
        };
    }
    if let Command::Setup(args) = &cli.command {
        return match setup_cmd::run(&cli.global, args) {
            Ok(code) => code,
            Err(err) => report_error(&err),
        };
    }
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => report_error(&err),
    }
}

/// Through a running daemon when it answers the command, else directly.
fn dispatch(cli: Cli) -> anyhow::Result<()> {
    if let Some(done) = completions::run(&cli) {
        return done;
    }
    if remote::run(&cli.global, &cli.command)? {
        return Ok(());
    }
    run(cli)
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
        Command::Serve { args, scheduler } => daemon::run(global, &args, &scheduler),
        Command::Sleep(args) => schedule_cmd::sleep(global, &args),
        Command::Eq(args) => eq_cmd::run(global, &args),
        Command::QueuePlay(args) => queue_play_cmd::run(global, &args),
        Command::QueueSearch(args) => queue_search_cmd::run(global, &args),
        Command::QueueRemove(args) => queue_edit_cmd::remove(global, &args),
        Command::QueueMove(args) => queue_edit_cmd::move_entries(global, &args),
        Command::PlayMode(args) => play_mode_cmd::run(global, &args),
        Command::Events(args) => events_cmd::run(global, &args),
        Command::Schedule { action } => schedule_cmd::schedule(global, &action),
        Command::Mcp { profile } => run_mcp_stdio(global, profile.into()),
        Command::Tailscale(args) => tailscale_cmd::run(global, &args),
        Command::Service(args) => service_cmd::run(global, &args),
        Command::Scene(args) => scene_cmd::run(global, &args),
        #[cfg(feature = "sim")]
        Command::Sim(args) => sim::run(&args, global.json),
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
        Command::Zones { zone } => zones(global, zone.as_deref()),
        Command::Search { query, zone, limit } => search(
            global,
            &search_request(&query, zone.as_deref(), limit),
            &Direct::survey(global)?,
        ),
        Command::Status { zone } => {
            let state = Direct::survey(global)?.status(&zone)?;
            emit(global.json, &state, direct::status_text)
        }
        Command::Rooms { alias } => rooms(global, alias),
        Command::Favorites { zone } => {
            let favorites = Direct::survey(global)?.favorites(&zone)?;
            emit(global.json, &favorites, |f| direct::favorites_text(f))
        }
        Command::Queue {
            zone,
            offset,
            limit,
        } => {
            let queue = Direct::survey(global)?.queue(&zone, offset, limit)?;
            emit(global.json, &queue, QueueDto::text)
        }
        Command::History { zone, limit } => {
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            let plays = Direct::survey(global)?.history(zone.as_deref(), limit)?;
            emit(global.json, &plays, |p| direct::history_text(p))
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
        Command::Undo {
            mine,
            preview,
            id,
            force,
        } => undo(global, mine, preview, id, force),
        Command::Dj { action } if action.is_read() => dj_cmd::read(global, &action),
        Command::Policy { action } => policy_cmd::run(global, &action),
        Command::Play {
            zone,
            search: Some(query),
            pick,
            ..
        } => play_search(global, zone, query, pick, &Direct::survey(global)?),
        Command::Play {
            zone,
            favorite: Some(favorite),
            next: false,
            queue: false,
            ..
        } => {
            let req = PlayFavoriteRequest { zone, favorite };
            let outcome = Direct::survey(global)?.play_favorite(&req)?;
            emit(global.json, &outcome, outcome_text)
        }
        Command::Say(args) => announce_cmd::run_and_emit(global, &args.request()),
        Command::Chime(args) => announce_cmd::run_and_emit(global, &args.request()),
        Command::Announce(args) => announce_cmd::run_and_emit(global, &args.request()?),
        Command::Stream(args) => streaming::run(global, &args),
        control => run_control(global, &control),
    }
}

/// `fsonos zones`: every zone, or the one `room` plays in.
fn zones(global: &config::GlobalArgs, room: Option<&str>) -> anyhow::Result<()> {
    let house = Direct::survey(global)?;
    match room {
        None => emit(global.json, &house.zones()?, |z| direct::zones_text(z)),
        Some(room) => emit(global.json, &house.zone(room)?, |z| {
            direct::zones_text(std::slice::from_ref(z))
        }),
    }
}

/// `fsonos rooms`: list them, or change an alias or the default room.
fn rooms(global: &config::GlobalArgs, alias: Option<RoomsCommand>) -> anyhow::Result<()> {
    match alias {
        None => {
            let rooms = Direct::survey(global)?.rooms()?;
            emit(global.json, &rooms, |r| direct::rooms_text(r))
        }
        Some(RoomsCommand::Alias { action }) => alias_command(global, &action),
        Some(RoomsCommand::Default {
            room,
            client,
            clear,
        }) => default_room_command(global, room.as_deref(), client.as_deref(), clear),
    }
}

/// A control command, carried out directly under the house policy.
fn run_control(global: &config::GlobalArgs, control: &Command) -> anyhow::Result<()> {
    let direct = Direct::survey(global)?;
    if let Some(req) = queue_request(control) {
        return emit(
            global.json,
            &direct.queue_request(&req)?,
            QueueAddedDto::text,
        );
    }
    let tool = tool_name(control);
    let outcome = match control {
        Command::Volume {
            zone,
            level,
            group,
            fade: Some(secs),
        } => direct.fade_volume(&fade_volume_request(zone, level, *group, *secs)?)?,
        Command::Pause {
            zone,
            fade: Some(secs),
        } => direct.fade_out(&FadePauseRequest {
            zone: zone.clone(),
            fade_secs: *secs,
        })?,
        _ if takes_set(control) => {
            direct.run_set(tool, |households| plan_set_for(control, households))?
        }
        _ => direct.run(tool, |households| plan_for(control, households))?,
    };
    emit(global.json, &outcome, outcome_text)
}

/// A control's outcome as the CLI prints it, directly or through the daemon:
/// what was done, then a line per note (a clamped volume, a room skipped, a
/// command healed).
fn outcome_text(outcome: &OutcomeDto) -> String {
    let mut text = format!("{}\n", outcome.done);
    for note in &outcome.notes {
        let _ = writeln!(text, "note: {}", note.detail);
    }
    text
}

/// `fsonos play <room> <link> --next|--queue`: what to add to the queue.
fn queue_request(command: &Command) -> Option<QueueRequest> {
    match command {
        Command::Play {
            zone,
            source_uri,
            favorite,
            title,
            next,
            queue,
            ..
        } if (*next || *queue) && (source_uri.is_some() || favorite.is_some()) => {
            Some(QueueRequest {
                zone: zone.clone(),
                source_uri: source_uri.clone(),
                favorite: favorite.clone(),
                title: title.clone(),
                next: *next,
            })
        }
        _ => None,
    }
}

/// `fsonos undo`: preview the action an undo would reverse (`--preview`),
/// undo action `id` only if it is still the one to undo, or undo the latest.
fn undo(
    global: &config::GlobalArgs,
    mine: bool,
    preview: bool,
    id: Option<i64>,
    force: bool,
) -> anyhow::Result<()> {
    let house = Direct::survey(global)?;
    if preview {
        let next = house.preview_undo()?;
        return emit(global.json, &next, |a: &Option<fsonos_api::ActionDto>| {
            direct::undo_preview_text(a.as_ref())
        });
    }
    let undone = match id {
        Some(id) => house.undo_checked(id)?,
        None => house.undo(mine, force)?,
    };
    emit(global.json, &undone, |u: &fsonos_api::UndoDto| {
        format!("{}\n", u.summary)
    })
}

/// Where `fsonos play --search` searches and plays: directly ([`Direct`]),
/// or through a running daemon (`crate::remote`).
trait SearchAndPlay {
    fn search(&self, req: &SearchRequest) -> Result<Vec<HitDto>, Failure>;
    /// `search_and_play`, as HTTP and MCP play a search result.
    fn search_and_play(&self, req: &SearchPlayRequest) -> Result<OutcomeDto, Failure>;
}

impl SearchAndPlay for Direct {
    fn search(&self, req: &SearchRequest) -> Result<Vec<HitDto>, Failure> {
        Direct::search(self, req)
    }

    fn search_and_play(&self, req: &SearchPlayRequest) -> Result<OutcomeDto, Failure> {
        Direct::search_and_play(self, req)
    }
}

/// What `fsonos search` asks for.
fn search_request(query: &[String], zone: Option<&str>, limit: u32) -> SearchRequest {
    SearchRequest {
        query: query.join(" "),
        zone: zone.map(str::to_owned),
        limit: usize::try_from(limit).ok(),
    }
}

/// `fsonos search`: the best matches, numbered, best first.
fn search(
    global: &config::GlobalArgs,
    req: &SearchRequest,
    via: &dyn SearchAndPlay,
) -> anyhow::Result<()> {
    let hits = via.search(req)?;
    if hits.is_empty() {
        let favorites = req
            .zone
            .as_ref()
            .map_or_else(String::new, |zone| format!(" or {zone}'s favorites"));
        return Err(Failure::new(
            ErrorCode::NoMatch,
            format!("nothing in the library{favorites} matches {:?}", req.query),
        )
        .into());
    }
    emit(global.json, &hits, |hits: &Vec<HitDto>| hits_text(hits))
}

/// Search hits a line each, numbered: title, who, and what kind.
fn hits_text(hits: &[HitDto]) -> String {
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
}

/// `fsonos play <room> --search <query> [--pick [N]]`: play the best match
/// (or the Nth), or with a bare `--pick` list the matches.
fn play_search(
    global: &config::GlobalArgs,
    zone: String,
    query: String,
    pick: Option<usize>,
    via: &dyn SearchAndPlay,
) -> anyhow::Result<()> {
    if pick == Some(0) {
        let req = SearchRequest {
            query,
            zone: Some(zone.clone()),
            limit: Some(10),
        };
        let hits = via.search(&req)?;
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
        return emit(global.json, &hits, |hits: &Vec<HitDto>| hits_text(hits));
    }
    // The same search and pick HTTP and MCP make: a pick past the matches
    // is NO_MATCH there too.
    let outcome = via.search_and_play(&SearchPlayRequest { zone, query, pick })?;
    emit(global.json, &outcome, |o: &OutcomeDto| {
        format!("{}\n", o.done)
    })
}

/// `fsonos rooms alias add|rm`: edit aliases.toml in the data directory.
fn alias_command(global: &config::GlobalArgs, action: &AliasAction) -> anyhow::Result<()> {
    let path = daemon::data_dir(global)?.join(fsonos_api::surface::ALIASES_FILE);
    let done = match action {
        AliasAction::Add { alias, rooms } => {
            if fsonos_core::rooms::RESERVED
                .contains(&fsonos_core::rooms::normalize_room(alias).as_str())
            {
                return Err(Failure::invalid(format!(
                    "{alias:?} is reserved for every room or yours"
                ))
                .with_hint("Pick another name; all, everywhere and here have fixed meanings.")
                .into());
            }
            rooms_cmd::edit_file(&path, |text| Ok(rooms_cmd::set_alias(text, alias, rooms)))?;
            format!("{alias} names {}", rooms.join(", "))
        }
        AliasAction::Rm { alias } => {
            rooms_cmd::edit_file(&path, |text| {
                rooms_cmd::remove_alias(text, alias).ok_or_else(|| {
                    Failure::new(
                        ErrorCode::UnknownRoom,
                        format!("no alias {alias:?} in aliases.toml"),
                    )
                    .with_hint("List the aliases with fsonos rooms.")
                })
            })?;
            format!("removed the alias {alias}")
        }
    };
    emit(
        global.json,
        &serde_json::json!({ "done": done, "file": path }),
        |_| format!("{done}\n"),
    )
}

/// `fsonos rooms default`: set, clear or show the room `here` means.
fn default_room_command(
    global: &config::GlobalArgs,
    room: Option<&str>,
    client: Option<&str>,
    clear: bool,
) -> anyhow::Result<()> {
    let path = daemon::data_dir(global)?.join(fsonos_api::surface::ALIASES_FILE);
    let whose = client.unwrap_or("everyone");
    let done = match (room, clear) {
        (Some(room), _) => {
            if fsonos_core::rooms::RESERVED
                .contains(&fsonos_core::rooms::normalize_room(room).as_str())
            {
                return Err(Failure::invalid(format!("{room:?} names no one room"))
                    .with_hint("Name a room; all, everywhere and here can't be the default.")
                    .into());
            }
            rooms_cmd::edit_file(&path, |text| {
                Ok(rooms_cmd::set_default_room(text, client, room))
            })?;
            format!("here now means {room} for {whose}")
        }
        (None, true) => {
            rooms_cmd::edit_file(&path, |text| {
                rooms_cmd::clear_default_room(text, client).ok_or_else(|| {
                    Failure::new(
                        ErrorCode::UnknownRoom,
                        format!("no default room is set for {whose}"),
                    )
                    .with_hint("Set one with fsonos rooms default <room>.")
                })
            })?;
            format!("here has no default of its own for {whose} now")
        }
        (None, false) => {
            let aliases = fsonos_core::rooms::Aliases::load(&path)
                .map_err(|e| Failure::invalid(e.to_string()))?;
            aliases.default_room(client).map_or_else(
                || format!("here means no room for {whose} yet: fsonos rooms default <room>"),
                |room| format!("here means {room} for {whose}"),
            )
        }
    };
    emit(
        global.json,
        &serde_json::json!({ "done": done, "file": path }),
        |_| format!("{done}\n"),
    )
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

/// `fsonos party [target]`: a household label names the household;
/// anything else is the room that leads.
fn party_request(households: &[fsonos_core::HouseholdState], target: Option<&str>) -> PartyRequest {
    let labels = fsonos_core::rooms::household_labels(households);
    match target {
        Some(t) if labels.iter().any(|l| l.eq_ignore_ascii_case(t.trim())) => PartyRequest {
            zone: None,
            household: Some(t.to_owned()),
        },
        Some(t) => PartyRequest {
            zone: Some(t.to_owned()),
            household: None,
        },
        None => PartyRequest::default(),
    }
}

/// The shared request a control subcommand stands for, planned against
/// `households`.
fn plan_for<'a>(
    command: &Command,
    households: impl Into<fsonos_api::plan::Rooms<'a>>,
) -> Result<fsonos_api::Command, Failure> {
    let households = households.into();
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
        Command::Pause { .. }
        | Command::Resume { .. }
        | Command::Next { .. }
        | Command::Previous { .. }
        | Command::Volume { .. }
        | Command::Mute { .. }
        | Command::Ungroup { .. } => unreachable!("a set-accepting command: see plan_set_for"),
        Command::Group { zone, to } => plan::plan_group(
            households,
            &GroupRequest {
                zone: zone.clone(),
                to: to.clone(),
            },
        ),
        Command::Seek { zone, position } => {
            plan::plan_seek(households, &seek_request(zone, position)?)
        }
        Command::Move { zone, to, copy } => plan::plan_move(
            households,
            &MoveRequest {
                zone: zone.clone(),
                to: to.clone(),
                copy: *copy,
            },
        ),
        Command::Party { target } => plan::plan_party(
            households,
            &party_request(households.households, target.as_deref()),
        ),
        Command::Dj { action } => action.plan(&households),
        Command::Discover
        | Command::Zones { .. }
        | Command::Search { .. }
        | Command::Doctor(_)
        | Command::Status { .. }
        | Command::Favorites { .. }
        | Command::Queue { .. }
        | Command::QueuePlay(_)
        | Command::QueueSearch(_)
        | Command::QueueRemove(_)
        | Command::QueueMove(_)
        | Command::PlayMode(_)
        | Command::Eq(_)
        | Command::History { .. }
        | Command::Log { .. }
        | Command::Events(_)
        | Command::Undo { .. }
        | Command::Policy { .. }
        | Command::Serve { .. }
        | Command::Sleep(_)
        | Command::Schedule { .. }
        | Command::Tailscale(_)
        | Command::Service(_)
        | Command::Rooms { .. }
        | Command::Scene(_)
        | Command::Setup(_)
        | Command::Selftest(_)
        | Command::Say(_)
        | Command::Chime(_)
        | Command::Announce(_)
        | Command::Stream(_)
        | Command::Completions { .. }
        | Command::CompleteRooms
        | Command::Mcp { .. } => {
            unreachable!("not a control command")
        }
        #[cfg(feature = "sim")]
        Command::Sim(_) => unreachable!("not a control command"),
    }
}

/// Does `command` accept a set of rooms (`all`, `everywhere`, a multi-room
/// alias)? Those plan with [`plan_set_for`], the rest with [`plan_for`].
fn takes_set(command: &Command) -> bool {
    matches!(
        command,
        Command::Pause { .. }
            | Command::Resume { .. }
            | Command::Next { .. }
            | Command::Previous { .. }
            | Command::Volume { .. }
            | Command::Mute { .. }
            | Command::Ungroup { .. }
    )
}

/// The commands a set-accepting subcommand stands for, planned against
/// `households`: one per room (volume, mute) or group (transport) its zone
/// names, so `fsonos pause everywhere` or `fsonos mute downstairs` reach every
/// room.
fn plan_set_for<'a>(
    command: &Command,
    households: impl Into<fsonos_api::plan::Rooms<'a>>,
) -> Result<Vec<fsonos_api::Command>, Failure> {
    let households = households.into();
    let zone = |zone: &String| ZoneRequest { zone: zone.clone() };
    match command {
        Command::Pause { zone: z, .. } => {
            plan::plan_transport_set(households, &zone(z), TransportAction::Pause)
        }
        Command::Resume { zone: z } => {
            plan::plan_transport_set(households, &zone(z), TransportAction::Resume)
        }
        Command::Next { zone: z } => {
            plan::plan_transport_set(households, &zone(z), TransportAction::Next)
        }
        Command::Previous { zone: z } => {
            plan::plan_transport_set(households, &zone(z), TransportAction::Previous)
        }
        Command::Volume {
            zone, level, group, ..
        } => plan::plan_volume_set(households, &volume_request(zone, level, *group)?),
        Command::Mute { zone, state } => plan::plan_mute_set(
            households,
            &MuteRequest {
                zone: zone.clone(),
                mute: *state == Switch::On,
            },
        ),
        Command::Ungroup { zone: z } => plan::plan_ungroup_set(households, &zone(z)),
        _ => unreachable!("not a set-accepting command"),
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
        Command::Seek { .. } => "seek",
        Command::Volume { .. } => "set_volume",
        Command::Mute { .. } => "mute",
        Command::Group { .. } => "group",
        Command::Ungroup { .. } => "ungroup",
        Command::Move { .. } => "move_playback",
        Command::Party { .. } => "group_all",
        Command::Dj { action } => action.tool(),
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
/// A `--fade` length in seconds: `10`, `10s` or `1m`.
fn parse_fade(text: &str) -> Result<u32, String> {
    let lower = text.trim().to_ascii_lowercase();
    let (digits, scale) = match lower.strip_suffix('m') {
        Some(minutes) => (minutes, 60),
        None => (lower.strip_suffix('s').unwrap_or(&lower), 1),
    };
    digits
        .trim()
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{text:?} is not a fade length like 10, 10s or 1m"))
}

/// A `volume --fade`: a length as [`parse_fade`] reads it, or `auto` (0):
/// each speaker ramps itself at its own pace.
fn parse_ramp(text: &str) -> Result<u32, String> {
    if text.trim().eq_ignore_ascii_case("auto") {
        return Ok(0);
    }
    parse_fade(text)
}

/// The ramp `volume --fade` asks for: each room `zone` names to the absolute
/// `level` over `fade_secs`.
fn fade_volume_request(
    zone: &str,
    level: &str,
    group: bool,
    fade_secs: u32,
) -> Result<FadeVolumeRequest, Failure> {
    if group {
        return Err(
            Failure::invalid("--fade ramps each room's own volume, not --group's")
                .with_hint("Leave out --group: `all` or an alias ramps several rooms."),
        );
    }
    let volume = volume_request(zone, level, false)?.volume.ok_or_else(|| {
        Failure::invalid(format!(
            "--fade needs a level to ramp to (0-100), not {level:?}"
        ))
        .with_hint("e.g. fsonos volume Kitchen 20 --fade 10s")
    })?;
    Ok(FadeVolumeRequest {
        zone: zone.to_string(),
        volume,
        fade_secs,
    })
}

/// The seek `seek <zone> <to>` asks for: `1:30`, `1:02:03` or `90` from the
/// track's start, `+30` / `-15` (seconds; `s` optional) from where it is.
fn seek_request(zone: &str, to: &str) -> Result<fsonos_api::SeekRequest, Failure> {
    let to = to.trim();
    let bad = || {
        Failure::invalid(format!(
            "{to:?} is not a place in a track like 1:30, 90, +30 or -15"
        ))
        .with_hint("e.g. fsonos seek Kitchen 0 (starts it over), fsonos seek Kitchen +30")
    };
    // Digits only, up to h:mm:ss, minutes and seconds under 60.
    let secs = |text: &str| -> Option<u32> {
        let parts: Vec<&str> = text.strip_suffix('s').unwrap_or(text).split(':').collect();
        if parts.len() > 3
            || parts
                .iter()
                .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        let mut total: u32 = 0;
        for (i, part) in parts.iter().enumerate() {
            let n: u32 = part.parse().ok()?;
            if i > 0 && n >= 60 {
                return None;
            }
            total = total.checked_mul(60)?.checked_add(n)?;
        }
        Some(total)
    };
    let (to_secs, by_secs) = if let Some(ahead) = to.strip_prefix('+') {
        (None, Some(i64::from(secs(ahead).ok_or_else(bad)?)))
    } else if let Some(back) = to.strip_prefix('-') {
        (None, Some(-i64::from(secs(back).ok_or_else(bad)?)))
    } else {
        (Some(secs(to).ok_or_else(bad)?), None)
    };
    Ok(fsonos_api::SeekRequest {
        zone: zone.to_string(),
        to_secs,
        by_secs,
    })
}

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

/// The daemon as `fsonos mcp` sees it: looked up on each call (it may start
/// or stop while the stdio server runs) and called as the stdio client.
struct StdioDaemon(config::GlobalArgs);

impl fsonos_mcp::tools::Daemon for StdioDaemon {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Option<Result<serde_json::Value, Failure>> {
        let daemon = match remote::Daemon::find(&self.0, self.0.daemon) {
            Ok(Some(daemon)) => daemon.for_stdio_mcp(),
            Ok(None) => return None,
            Err(failure) => return Some(Err(failure)),
        };
        Some(daemon.forward(method, path, body))
    }
}

/// Serve the MCP tools over stdio, acting on this LAN's speakers as the
/// `mcp-stdio` client of the house policy. While `fsonos serve` runs, the
/// DJ, scene-apply, sleep-timer, move and fade tools act through it (see
/// [`StdioDaemon`]).
/// Its `doctor` runs what `fsonos doctor` runs, for the daemon settings in
/// the environment.
/// `fsonos mcp --profile`: see [`fsonos_mcp::Profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum McpProfile {
    Essentials,
    Full,
}

impl From<McpProfile> for fsonos_mcp::Profile {
    fn from(profile: McpProfile) -> Self {
        match profile {
            McpProfile::Essentials => Self::Essentials,
            McpProfile::Full => Self::Full,
        }
    }
}

fn run_mcp_stdio(global: &config::GlobalArgs, profile: fsonos_mcp::Profile) -> anyhow::Result<()> {
    let data_dir = daemon::data_dir(global)?;
    let serve =
        config::ServeArgs::from_env().context("read serve's settings from the environment")?;
    let surface = daemon::surface(global, daemon::policy(&data_dir)?)?;
    let surface = daemon::with_action_log(surface, &data_dir, "mcp")
        .with_doctor_checks(doctor::checks(global, serve)?);
    let surface = std::sync::Arc::new(surface);
    let backend =
        fsonos_mcp::tools::Backend::shared(surface, fsonos_core::policy::Client::McpStdio)
            .with_daemon(std::sync::Arc::new(StdioDaemon(global.clone())));
    if !fsonos_mcp::tools::install(backend) {
        anyhow::bail!("the MCP backend is already installed");
    }
    runtime()?.block_on(async {
        let cx = asupersync::Cx::current().context("runtime installs an ambient Cx")?;
        fsonos_mcp::server_with(Vec::new(), profile)
            .run_stdio_with_cx(&cx)
            .await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outcome_prints_what_was_done_then_its_notes() {
        let outcome = OutcomeDto {
            done: "Kitchen joined the party".into(),
            changed: true,
            volume: None,
            notes: vec![fsonos_api::Note {
                code: fsonos_api::NoteCode::Skipped,
                detail: "Office skipped: the speaker refused".into(),
            }],
        };
        assert_eq!(
            outcome_text(&outcome),
            "Kitchen joined the party\nnote: Office skipped: the speaker refused\n"
        );
    }
    use fsonos_api::plan::VolumeScope;
    use fsonos_api::{Command as Planned, VolumeChange};
    use fsonos_core::HouseholdState;
    use fsonos_core::Room;
    use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};

    /// `remove` is `rm`, as HTTP and MCP name it (8hhv.2).
    #[test]
    fn remove_is_rm() {
        for args in [
            &["schedule", "remove", "3"][..],
            &["scene", "remove", "Evening"],
            &["rooms", "alias", "remove", "upstairs"],
        ] {
            let parsed = Cli::try_parse_from(std::iter::once("fsonos").chain(args.iter().copied()));
            assert!(parsed.is_ok(), "{args:?}: {:?}", parsed.err());
        }
    }

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
    fn fades_parse_as_seconds_and_need_a_level() {
        assert_eq!(parse_fade("10"), Ok(10));
        assert_eq!(parse_fade("10s"), Ok(10));
        assert_eq!(parse_fade(" 1m "), Ok(60));
        for bad in ["", "0", "x", "10h", "-5"] {
            assert!(parse_fade(bad).is_err(), "{bad:?}");
        }
        let Command::Volume { fade, .. } = parse(&["volume", "Kitchen", "30", "--fade", "10s"])
        else {
            panic!("expected volume")
        };
        assert_eq!(fade, Some(10));
        // `auto`: each speaker ramps itself; a pause has no such fade.
        let Command::Volume { fade, .. } = parse(&["volume", "Kitchen", "30", "--fade", "auto"])
        else {
            panic!("expected volume")
        };
        assert_eq!(fade, Some(0));
        assert!(parse_fade("auto").is_err());
        let Command::Pause { fade, .. } = parse(&["pause", "Den", "--fade", "1m"]) else {
            panic!("expected pause")
        };
        assert_eq!(fade, Some(60));
        let ramp = fade_volume_request("all", "20", false, 5).unwrap();
        assert_eq!(
            (ramp.zone.as_str(), ramp.volume, ramp.fade_secs),
            ("all", 20, 5)
        );
        assert!(fade_volume_request("Kitchen", "+5", false, 5).is_err());
        assert!(fade_volume_request("Kitchen", "20", true, 5).is_err());
    }

    #[test]
    fn negative_volume_is_a_value_not_a_flag() {
        let Command::Volume { level, group, .. } = parse(&["volume", "Kitchen", "-5"]) else {
            panic!("expected volume")
        };
        assert_eq!((level.as_str(), group), ("-5", false));
    }

    #[test]
    fn seek_takes_a_time_or_a_signed_offset() {
        // A step back is a value, not a flag.
        let Command::Seek { zone, position } = parse(&["seek", "Kitchen", "-1:30"]) else {
            panic!("expected seek")
        };
        assert_eq!((zone.as_str(), position.as_str()), ("Kitchen", "-1:30"));
        let at = |to: &str| {
            seek_request("Kitchen", to)
                .map(|r| (r.to_secs, r.by_secs))
                .map_err(|f| f.detail)
        };
        assert_eq!(at("1:30"), Ok((Some(90), None)));
        assert_eq!(at("90"), Ok((Some(90), None)));
        assert_eq!(at("0"), Ok((Some(0), None)));
        assert_eq!(at("1:02:03"), Ok((Some(3723), None)));
        assert_eq!(at("+30"), Ok((None, Some(30))));
        assert_eq!(at("-15s"), Ok((None, Some(-15))));
        assert_eq!(at("-1:00"), Ok((None, Some(-60))));
        for bad in [
            "", "soon", "1:", "+", "1::2", "1:75", "++30", "1:+30", "1:2:3:4",
        ] {
            assert!(at(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn subcommands_plan_to_the_right_players() {
        let houses = house();
        let plan = |args: &[&str]| plan_for(&parse(args), &houses).unwrap();
        let plan_set = |args: &[&str]| plan_set_for(&parse(args), &houses).unwrap();
        assert_eq!(
            plan_set(&["pause", "kitchen"]),
            [Planned::Transport {
                coordinator: PlayerId("RINCON_DEN".into()),
                action: TransportAction::Pause
            }]
        );
        assert_eq!(
            plan_set(&["volume", "Kitchen", "-5"]),
            [Planned::Volume {
                target: PlayerId("RINCON_KIT".into()),
                scope: VolumeScope::Room,
                change: VolumeChange::Adjust(-5)
            }]
        );
        assert_eq!(
            plan_set(&["volume", "Kitchen", "40", "--group"]),
            [Planned::Volume {
                target: PlayerId("RINCON_DEN".into()),
                scope: VolumeScope::Group,
                change: VolumeChange::Set(40)
            }]
        );
        assert_eq!(
            plan_set(&["mute", "Kitchen"]),
            [Planned::Mute {
                target: PlayerId("RINCON_KIT".into()),
                mute: true
            }]
        );
        assert_eq!(
            plan_set(&["mute", "Kitchen", "off"]),
            [Planned::Mute {
                target: PlayerId("RINCON_KIT".into()),
                mute: false
            }]
        );
        let leave = |pid: &str| Planned::Leave {
            member: PlayerId(pid.into()),
        };
        assert_eq!(plan_set(&["ungroup", "Kitchen"]), [leave("RINCON_KIT")]);
        assert_eq!(plan_set(&["ungroup", "everywhere"]), [leave("RINCON_KIT")]);
        assert_eq!(plan_set(&["ungroup", "Den"]), [leave("RINCON_DEN")]);
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
    fn the_database_logs_only_errors_unless_asked() {
        assert_eq!(log_spec(None), "info,fsqlite=error");
        assert_eq!(log_spec(Some("debug".into())), "debug,fsqlite=error");
        assert_eq!(
            log_spec(Some("info,fsqlite=debug".into())),
            "info,fsqlite=debug"
        );
        for spec in [log_spec(None), log_spec(Some(String::new()))] {
            assert!(
                tracing_subscriber::EnvFilter::try_new(&spec).is_ok(),
                "{spec}"
            );
        }
    }

    #[test]
    fn search_takes_its_words_and_a_limit() {
        let cli = Cli::try_parse_from(["fsonos", "search", "bwv", "988", "--limit", "3"]).unwrap();
        let Command::Search { query, zone, limit } = cli.command else {
            panic!("not a search");
        };
        let req = search_request(&query, zone.as_deref(), limit);
        assert_eq!(
            (req.query.as_str(), req.zone, req.limit),
            ("bwv 988", None, Some(3))
        );
        assert!(
            Cli::try_parse_from(["fsonos", "search"]).is_err(),
            "words needed"
        );
        assert!(Cli::try_parse_from(["fsonos", "search", "x", "--limit", "51"]).is_err());
    }

    /// `--next`, `--queue` and `--pick` belong to one way of choosing what
    /// to play. Paired with another way they must be refused, not dropped
    /// (`play Den --favorite X --next` once played X at once, replacing what
    /// was playing); a favorite is now queued like a source URI.
    #[test]
    fn play_flags_never_ride_along_with_another_source() {
        let refused = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("fsonos").chain(args.iter().copied())).is_err()
        };
        for flag in ["--next", "--queue"] {
            let queued = queue_request(&parse(&["play", "Den", "--favorite", "Jazz", flag]));
            assert!(
                queued
                    .as_ref()
                    .is_some_and(|r| r.favorite.as_deref() == Some("Jazz")
                        && r.source_uri.is_none()
                        && r.next == (flag == "--next")),
                "{flag}: {queued:?}"
            );
            assert!(
                refused(&["play", "Den", "--search", "bwv 988", flag]),
                "{flag}"
            );
            assert!(refused(&["play", "Den", flag]), "{flag} alone");
            assert!(
                matches!(
                    parse(&["play", "Den", "spotify:track:abc", flag]),
                    Command::Play { next, queue, .. } if next || queue
                ),
                "{flag} with a source URI"
            );
        }
        assert!(refused(&[
            "play",
            "Den",
            "--favorite",
            "Jazz",
            "--pick",
            "2"
        ]));
        assert!(refused(&["play", "Den", "spotify:track:abc", "--pick"]));
        assert!(refused(&["play", "Den", "--pick", "2"]));
        assert!(matches!(
            parse(&["play", "Den", "--search", "bwv 988", "--pick", "2"]),
            Command::Play { pick: Some(2), .. }
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
    fn announce_requires_a_local_file_and_accepts_room_options() {
        assert!(Cli::try_parse_from(["fsonos", "announce"]).is_err());
        let command = parse(&[
            "announce",
            "--file",
            "clip.wav",
            "--rooms",
            "Kitchen,Office",
            "--volume",
            "25",
        ]);
        let Command::Announce(args) = command else {
            panic!("announce command")
        };
        assert_eq!(args.file, std::path::Path::new("clip.wav"));
        assert_eq!(args.at.rooms, ["Kitchen", "Office"]);
        assert_eq!(args.at.volume, Some(25));
    }

    #[test]
    fn unknown_rooms_fail_with_suggestions() {
        let err = plan_set_for(&parse(&["pause", "Kitchn"]), &house()).unwrap_err();
        assert_eq!(
            (err.code, err.exit_code()),
            (fsonos_api::ErrorCode::UnknownRoom, 3)
        );
        assert_eq!(err.suggestions, ["Kitchen@S2"]);
    }

    #[test]
    fn a_set_of_rooms_plans_one_command_per_room_or_group() {
        let houses = house();
        let plan_set = |args: &[&str]| plan_set_for(&parse(args), &houses).unwrap();
        // Den and Kitchen play in one group: pausing everywhere pauses it once.
        assert_eq!(
            plan_set(&["pause", "everywhere"]),
            [Planned::Transport {
                coordinator: PlayerId("RINCON_DEN".into()),
                action: TransportAction::Pause
            }]
        );
        // Room volume and mute act on every room.
        let muted = plan_set(&["mute", "all"]);
        assert_eq!(muted.len(), 2, "{muted:?}");
        for pid in ["RINCON_DEN", "RINCON_KIT"] {
            assert!(muted.contains(&Planned::Mute {
                target: PlayerId(pid.into()),
                mute: true
            }));
        }
        let quieter = plan_set(&["volume", "all", "-5"]);
        assert_eq!(quieter.len(), 2, "{quieter:?}");
        assert!(quieter.iter().all(|c| matches!(
            c,
            Planned::Volume {
                scope: VolumeScope::Room,
                change: VolumeChange::Adjust(-5),
                ..
            }
        )));
        // Group volume on everywhere is one command per group.
        assert_eq!(plan_set(&["volume", "all", "30", "--group"]).len(), 1);
    }
}
