//! `fsonos sleep` and `fsonos schedule`: sleep timers and schedules from the
//! terminal, through the same surface as the HTTP API and the MCP tools (as
//! the house policy's `cli` client). Also the scheduler options and clock of
//! `fsonos serve`, which runs both.
//!
//! On its own the CLI sets the speaker's own sleep timer, which pauses
//! without a fade; `fsonos serve` fades the group out (`POST /sleep`, the
//! `set_sleep_timer` tool). Schedules go into the store in the data
//! directory, where a running `fsonos serve` picks them up within a second.

use chrono::{DateTime, FixedOffset};
use clap::{Args, Subcommand};
use fsonos_api::Failure;
use fsonos_api::surface::Surface;
use fsonos_api::surface::schedules::{
    ScheduleDto, ScheduleRequest, Sleep, SleepRequest, SleepTimerDto, parse_duration,
};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::policy::Client;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::GlobalArgs;

/// `fsonos sleep <room> [duration] [--extend <duration>] [--cancel]`.
#[derive(Args)]
pub struct SleepArgs {
    /// Room name: its group pauses (`Room@S1` / `Room@S2` picks a household).
    pub zone: String,
    /// How long until it pauses: 45m, 1h30m, 90s, or minutes (45). Without
    /// one (and without --extend or --cancel), show the timer.
    pub duration: Option<String>,
    /// Push the running timer this much later.
    #[arg(long, value_name = "DURATION", conflicts_with = "duration")]
    pub extend: Option<String>,
    /// Cancel the timer.
    #[arg(long, conflicts_with_all = ["duration", "extend"])]
    pub cancel: bool,
}

/// `fsonos schedule ...`.
#[derive(Subcommand)]
pub enum ScheduleCommand {
    /// Add a schedule, run by `fsonos serve`:
    /// `fsonos schedule add "weekdays 07:30" dj start Kitchen`,
    /// `... "in 45m" pause Bedroom --fade 2m`, `... "daily 22:00" volume
    /// Kitchen 15`, `... "sat 09:00" scene brunch`.
    Add {
        /// When: "in 45m", an RFC 3339 time, "daily 22:30", "weekdays 07:30",
        /// "weekends 09:00", "sat,sun 09:00", "mon-fri 07:00" (local time).
        when: String,
        /// What: dj start <room> | pause <room> [--fade <duration>] |
        /// volume <room> <0-100> | scene <name>.
        #[arg(
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        action: Vec<String>,
    },
    /// List the schedules, with their next runs.
    List,
    /// Remove a schedule by its id.
    Rm { id: i64 },
    /// Pause a schedule until resumed.
    Pause { id: i64 },
    /// Resume a paused schedule from its next time.
    Resume { id: i64 },
}

/// The CLI's surface: the LAN, with the data directory's policy and store.
fn surface(global: &GlobalArgs) -> Result<Surface, Failure> {
    let dir = crate::daemon::data_dir(global)?;
    let surface = crate::daemon::surface(global, crate::daemon::policy(&dir)?)?;
    Ok(crate::daemon::with_action_log(surface, &dir, "cli"))
}

/// `fsonos sleep`.
pub fn sleep(global: &GlobalArgs, args: &SleepArgs) -> anyhow::Result<()> {
    let surface = surface(global)?;
    let (duration, extend) = match (&args.duration, &args.extend) {
        (_, Some(by)) => (Some(by.clone()), true),
        (Some(after), None) => (Some(after.clone()), false),
        (None, None) if !args.cancel => {
            let timers = surface.sleep_timers(&Client::Cli, Some(&args.zone))?;
            return crate::emit(
                global.json,
                &timers,
                |timers: &Vec<SleepTimerDto>| match timers.first() {
                    Some(timer) => format!("{}\n", timer.done),
                    None => format!("no sleep timer runs in {}'s group\n", args.zone),
                },
            );
        }
        (None, None) => (None, false),
    };
    let req = SleepRequest {
        zone: args.zone.clone(),
        duration,
        extend,
        cancel: args.cancel,
    };
    let timer = surface.set_sleep_timer(&Client::Cli, &req)?;
    crate::emit(global.json, &timer, |t: &SleepTimerDto| {
        format!("{}\n", t.done)
    })
}

/// `fsonos schedule ...`.
pub fn schedule(global: &GlobalArgs, command: &ScheduleCommand) -> anyhow::Result<()> {
    let surface = surface(global)?;
    let line = |d: &ScheduleDto| format!("{}\n", d.line());
    match command {
        ScheduleCommand::Add { when, action } => {
            let req = schedule_request(when, action)?;
            let added = surface.add_schedule(&Client::Cli, &req)?;
            crate::emit(global.json, &added, |d: &ScheduleDto| {
                format!("added {}\n", d.line())
            })
        }
        ScheduleCommand::List => {
            let schedules = surface.schedules(&Client::Cli)?;
            crate::emit(global.json, &schedules, |all: &Vec<ScheduleDto>| {
                if all.is_empty() {
                    "no schedules\n".to_string()
                } else {
                    all.iter().map(&line).collect()
                }
            })
        }
        ScheduleCommand::Rm { id } => {
            let removed = surface.remove_schedule(&Client::Cli, *id)?;
            crate::emit(global.json, &removed, |d: &ScheduleDto| {
                format!("removed {}\n", d.line())
            })
        }
        ScheduleCommand::Pause { id } | ScheduleCommand::Resume { id } => {
            let paused = matches!(command, ScheduleCommand::Pause { .. });
            let schedule = surface.pause_schedule(&Client::Cli, *id, paused)?;
            crate::emit(global.json, &schedule, line)
        }
    }
}

/// Take `--flag <value>` out of `words`.
fn take_option(words: &mut Vec<&str>, flag: &str) -> Result<Option<String>, Failure> {
    let Some(i) = words.iter().position(|w| *w == flag) else {
        return Ok(None);
    };
    let value = words
        .get(i + 1)
        .map(ToString::to_string)
        .ok_or_else(|| Failure::invalid(format!("{flag} needs a value")))?;
    words.drain(i..=i + 1);
    Ok(Some(value))
}

/// The request `fsonos schedule add <when> <action...>` stands for: `dj
/// start <room> [--mood <mood>]`, `pause <room> [--fade <duration>]`,
/// `volume <room> <0-100>`, or `scene [apply] <name>`. A room or scene
/// name may be several words.
pub fn schedule_request(when: &str, action: &[String]) -> Result<ScheduleRequest, Failure> {
    let mut words: Vec<&str> = action.iter().map(String::as_str).collect();
    let mood = take_option(&mut words, "--mood")?;
    let fade_secs = take_option(&mut words, "--fade")?
        .map(|raw| {
            parse_duration(&raw)
                .and_then(|d| u32::try_from(d.as_secs()).ok())
                .ok_or_else(|| {
                    Failure::invalid(format!("--fade {raw:?} is not a duration"))
                        .with_hint("Write it like 30s or 2m.")
                })
        })
        .transpose()?;
    let req = |action: &str, zone: Option<String>, scene: Option<String>| ScheduleRequest {
        when: when.to_string(),
        action: action.to_string(),
        zone,
        scene,
        mood: mood.clone(),
        volume: None,
        fade_secs,
    };
    let named = |words: &[&str]| Some(words.join(" "));
    match words.as_slice() {
        ["dj", "start", room @ ..] if !room.is_empty() => Ok(req("dj_start", named(room), None)),
        ["pause", room @ ..] if !room.is_empty() => Ok(req("pause", named(room), None)),
        ["volume", room @ .., level] if !room.is_empty() => {
            let volume = level.parse::<i64>().map_err(|_| {
                Failure::invalid(format!("volume {level:?} is not a level"))
                    .with_hint("End with the level, 0 to 100: volume Kitchen 15.")
            })?;
            Ok(ScheduleRequest {
                volume: Some(volume),
                ..req("volume", named(room), None)
            })
        }
        ["scene", "apply", name @ ..] | ["scene" | "apply-scene", name @ ..]
            if !name.is_empty() =>
        {
            Ok(req("apply_scene", None, named(name)))
        }
        _ => Err(Failure::invalid(format!(
            "{:?} is not something a schedule can do",
            action.join(" ")
        ))
        .with_hint(
            "Use: dj start <room> | pause <room> [--fade 2m] | volume <room> <0-100> | \
             scene <name>.",
        )),
    }
}

/// `fsonos serve`'s scheduler settings; hidden, for the e2e tests.
#[derive(Debug, Clone, Default, Args)]
pub struct SchedulerArgs {
    /// Read the time from this file (an RFC 3339 instant) instead of the
    /// system clock, so a test can move the daemon's time.
    #[arg(long, hide = true, value_name = "FILE")]
    pub clock_file: Option<PathBuf>,
    /// How long a sleep timer fades out (default 2m).
    #[arg(long, hide = true, value_name = "DURATION", value_parser = duration_arg)]
    pub sleep_fade: Option<Duration>,
}

fn duration_arg(raw: &str) -> Result<Duration, String> {
    parse_duration(raw).ok_or_else(|| format!("{raw:?} is not a duration (2m, 90s)"))
}

impl SchedulerArgs {
    /// The daemon's clock.
    #[must_use]
    pub fn clock(&self) -> Box<dyn Clock> {
        match &self.clock_file {
            Some(path) => Box::new(FileClock(path.clone())),
            None => Box::new(SystemClock),
        }
    }

    /// The daemon's sleep timers.
    #[must_use]
    pub fn sleep(&self) -> Sleep {
        self.sleep_fade
            .map_or_else(Sleep::default, Sleep::with_fade)
    }
}

/// The time an RFC 3339 instant in a file says, read on every call; the
/// system clock while the file is missing or unreadable.
pub struct FileClock(pub PathBuf);

impl Clock for FileClock {
    fn now(&self) -> DateTime<FixedOffset> {
        std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|text| DateTime::parse_from_rfc3339(text.trim()).ok())
            .unwrap_or_else(|| SystemClock.now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_api::ErrorCode;

    fn add(when: &str, words: &str) -> Result<ScheduleRequest, Failure> {
        let words: Vec<String> = words.split(' ').map(String::from).collect();
        schedule_request(when, &words)
    }

    #[test]
    fn schedule_actions_read_as_people_say_them() {
        let dj = add("weekdays 07:30", "dj start Living Room --mood bright").unwrap();
        assert_eq!(
            (dj.action.as_str(), dj.zone.as_deref(), dj.mood.as_deref()),
            ("dj_start", Some("Living Room"), Some("bright"))
        );
        assert_eq!(dj.when, "weekdays 07:30");
        let pause = add("in 45m", "pause --fade 2m Bedroom").unwrap();
        assert_eq!(
            (
                pause.action.as_str(),
                pause.zone.as_deref(),
                pause.fade_secs
            ),
            ("pause", Some("Bedroom"), Some(120))
        );
        let volume = add("daily 22:00", "volume Living Room 15").unwrap();
        assert_eq!(
            (volume.zone.as_deref(), volume.volume),
            (Some("Living Room"), Some(15))
        );
        for words in [
            "scene dinner party",
            "scene apply dinner party",
            "apply-scene dinner party",
        ] {
            let scene = add("sat 09:00", words).unwrap();
            assert_eq!(
                (scene.action.as_str(), scene.scene.as_deref()),
                ("apply_scene", Some("dinner party"))
            );
        }
    }

    #[test]
    fn a_schedule_that_does_not_parse_says_so() {
        for words in [
            "reboot Kitchen",
            "dj start",
            "pause",
            "volume Kitchen loud",
            "volume 15",
            "pause Kitchen --fade",
            "pause Kitchen --fade soon",
            "scene",
        ] {
            assert_eq!(
                add("in 5m", words).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{words}"
            );
        }
    }

    #[test]
    fn a_file_clock_reads_its_file_and_falls_back_to_the_system() {
        let dir = std::env::temp_dir().join(format!("fsonos-file-clock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clock");
        let clock = FileClock(path.clone());
        let before = SystemClock.now();
        assert!(clock.now() >= before);
        std::fs::write(&path, "2026-10-08T21:00:00-04:00\n").unwrap();
        assert_eq!(clock.now().to_rfc3339(), "2026-10-08T21:00:00-04:00");
        std::fs::write(&path, "not a time").unwrap();
        assert!(clock.now() >= before);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
