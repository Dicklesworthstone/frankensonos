//! `fsonos setup`: a guided first run, from a fresh data directory to a house
//! the daemon can play to, built on the doctor's checks.
//!
//! The steps, in order, each judged by the doctor checks it rests on (the
//! worst of them; skipped checks count only when all skip):
//! 1. the data directory and store (`store.*`);
//! 2. discovery (`lan.ssdp`, `lan.seeds`, `lan.players`). When multicast
//!    fails but players answer directly, setup offers to write `seeds.toml`
//!    in the data directory with their addresses, so every later run finds
//!    them;
//! 3. the households: S1 and S2 and their rooms (`lan.households`,
//!    `lan.topology`);
//! 4. the Spotify sign-in: the PKCE login and the first library sync
//!    ([`crate::setup_spotify`]; `--skip-spotify` skips it);
//! 5. Spotify in each household: linked, a Spotify favorite, render
//!    parameters learned (`spotify.*`);
//! 6. Tailscale, when it is there (`tailscale.*`);
//! 7. what to run next.
//!
//! On a terminal, a step that fails prints its fix and waits: Enter checks
//! again, `s` skips it. With `--yes`, `--json` or no terminal it never waits;
//! it reports, writes `seeds.toml` when that is the fix, and goes on. `--json`
//! prints one JSON object per step. Re-running is safe: whatever is already
//! in place passes straight through. Setup never changes playback.
//!
//! The exit code is the doctor's: 0 when every step passed (or was
//! skipped), 6 when one only warned, 7 when one failed.

use fsonos_core::doctor::{EXIT_FAIL, EXIT_OK, EXIT_WARN, Report, Status};
use fsonos_core::policy::Client;
use serde::Serialize;
use std::io::{IsTerminal as _, Write as _};
use std::net::IpAddr;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use crate::config::{GlobalArgs, ServeArgs};
use crate::direct::Direct;
use crate::setup_spotify::{self, Prompt};

/// `fsonos setup`.
#[derive(Debug, Clone, clap::Args)]
pub struct SetupArgs {
    /// Never wait for Enter: report each step, write seeds.toml when that is
    /// the fix, and go on (for scripts and agents).
    #[arg(long)]
    pub yes: bool,
    /// Leave out the Spotify login.
    #[arg(long)]
    pub skip_spotify: bool,
    /// The daemon settings the next steps assume (the same flags and env as
    /// `serve`).
    #[command(flatten)]
    pub serve: ServeArgs,
}

/// One step of the walkthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    Data,
    Discovery,
    Households,
    SpotifyLogin,
    SpotifyHouseholds,
    Tailscale,
    NextSteps,
}

impl Step {
    pub const ALL: [Self; 7] = [
        Self::Data,
        Self::Discovery,
        Self::Households,
        Self::SpotifyLogin,
        Self::SpotifyHouseholds,
        Self::Tailscale,
        Self::NextSteps,
    ];

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Data => "Data directory and store",
            Self::Discovery => "Finding the players",
            Self::Households => "Households and rooms",
            Self::SpotifyLogin => "Spotify sign-in",
            Self::SpotifyHouseholds => "Spotify in each household",
            Self::Tailscale => "Tailscale",
            Self::NextSteps => "Next steps",
        }
    }

    /// The prefixes of the doctor check ids it rests on.
    fn checks(self) -> &'static [&'static str] {
        match self {
            Self::Data => &["store."],
            Self::Discovery => &["lan.ssdp", "lan.seeds", "lan.players"],
            Self::Households => &["lan.households", "lan.topology"],
            Self::SpotifyHouseholds => &["spotify."],
            Self::Tailscale => &["tailscale."],
            Self::SpotifyLogin | Self::NextSteps => &[],
        }
    }
}

/// How a step went.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepOutcome {
    pub step: Step,
    pub title: &'static str,
    pub status: Status,
    pub summary: String,
    /// What to do about it, one fix per failing or warning check.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remedies: Vec<String>,
}

impl StepOutcome {
    pub(crate) fn new(step: Step, status: Status, summary: impl Into<String>) -> Self {
        Self {
            step,
            title: step.title(),
            status,
            summary: summary.into(),
            remedies: Vec::new(),
        }
    }
}

/// Worse statuses rank higher; a skip ranks below a pass.
fn rank(status: Status) -> u8 {
    match status {
        Status::Skip => 0,
        Status::Pass => 1,
        Status::Warn => 2,
        Status::Fail => 3,
    }
}

/// A doctor-backed step's verdict: the worst of its checks, with their
/// summaries and fixes.
#[must_use]
pub fn judge(step: Step, report: &Report) -> StepOutcome {
    let entries: Vec<_> = report
        .entries
        .iter()
        .filter(|e| step.checks().iter().any(|p| e.id.0.starts_with(p)))
        .collect();
    let Some(worst) = entries
        .iter()
        .map(|e| e.result.status)
        .max_by_key(|&s| rank(s))
    else {
        return StepOutcome::new(step, Status::Skip, "nothing to check here");
    };
    let mut outcome = StepOutcome::new(
        step,
        worst,
        entries
            .iter()
            .filter(|e| e.result.status == worst)
            .map(|e| e.result.summary.as_str())
            .collect::<Vec<_>>()
            .join("; "),
    );
    outcome.remedies = entries
        .iter()
        .filter(|e| matches!(e.result.status, Status::Warn | Status::Fail))
        .filter_map(|e| e.result.remedy.clone())
        .collect();
    outcome
}

/// The `seeds.toml` to offer, when multicast found nothing but the players
/// answered directly and the data directory has none yet.
#[must_use]
pub fn seeds_offer(report: &Report, players: &[IpAddr], have_seeds_file: bool) -> Option<String> {
    let status = |prefix: &str| {
        report
            .entries
            .iter()
            .find(|e| e.id.0 == prefix)
            .map(|e| e.result.status)
    };
    let multicast_failed = matches!(status("lan.ssdp"), Some(Status::Warn | Status::Fail));
    if have_seeds_file || !multicast_failed || players.is_empty() {
        return None;
    }
    let quoted: Vec<String> = players.iter().map(|ip| format!("\"{ip}\"")).collect();
    Some(format!(
        "# Written by fsonos setup: these players answer directly, but multicast\n\
         # discovery (SSDP) does not reach them from this host.\n\
         players = [{}]\n",
        quoted.join(", ")
    ))
}

/// The exit code for a run's outcomes.
#[must_use]
pub fn exit_code(outcomes: &[StepOutcome]) -> u8 {
    match outcomes.iter().map(|o| rank(o.status)).max() {
        Some(3) => EXIT_FAIL,
        Some(2) => EXIT_WARN,
        _ => EXIT_OK,
    }
}

/// What to run once setup is through.
fn next_steps(serve: &ServeArgs) -> StepOutcome {
    StepOutcome::new(
        Step::NextSteps,
        Status::Pass,
        format!(
            "run the daemon (fsonos serve; its API answers on http://{}), keep it running \
             with the launchd plist in docs/DEPLOY.md, reach it from your tailnet with \
             fsonos tailscale setup, and give an agent the tools with: claude mcp add fsonos \
             -- fsonos mcp",
            serve.http_local()
        ),
    )
}

/// The Spotify sign-in step: on a terminal it can run the login; without
/// one it reports whether the owner is signed in.
fn spotify_login(global: &GlobalArgs, args: &SetupArgs, prompt: Option<&Prompt>) -> StepOutcome {
    if args.skip_spotify {
        return StepOutcome::new(Step::SpotifyLogin, Status::Skip, "skipped (--skip-spotify)");
    }
    setup_spotify::verdict(
        setup_spotify::sign_in(global, &args.serve, prompt),
        &args.serve.spotify_redirect_uri,
    )
}

/// Survey and run every doctor check once.
fn check(global: &GlobalArgs, serve: &ServeArgs) -> anyhow::Result<(Direct, Report)> {
    let serve = serve.clone();
    let lan = crate::doctor::lan_checks(global)?;
    let direct = Direct::open(
        global,
        Some(Box::new(move |runner| {
            lan(runner);
            crate::doctor::register(runner, &serve);
        })),
    )?;
    let report = direct.doctor(&Client::Cli)?;
    Ok((direct, report))
}

/// The households and their rooms, as one line.
fn households_line(direct: &Direct) -> Option<String> {
    let rooms = direct.rooms().ok()?;
    let mut by_household: Vec<(String, Vec<String>)> = Vec::new();
    for room in rooms {
        match by_household.iter_mut().find(|(h, _)| *h == room.household) {
            Some((_, names)) => names.push(room.name),
            None => by_household.push((room.household, vec![room.name])),
        }
    }
    Some(
        by_household
            .iter()
            .map(|(h, names)| format!("{h}: {}", names.join(", ")))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// One step on stdout: a JSON line, or a marked line with its fixes.
fn print(json: bool, outcome: &StepOutcome) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string(outcome)?);
        return Ok(());
    }
    let mark = match outcome.status {
        Status::Pass => "ok  ",
        Status::Warn => "warn",
        Status::Fail => "FAIL",
        Status::Skip => "skip",
    };
    println!("[{mark}] {}: {}", outcome.title, outcome.summary);
    for remedy in &outcome.remedies {
        println!("       fix: {remedy}");
    }
    Ok(())
}

/// Wait for Enter (check again) or `s` (skip); `true` to check again.
fn again(prompt: &Prompt) -> bool {
    print!("       Fix it, then press Enter to check again (s to skip): ");
    let _ = std::io::stdout().flush();
    prompt
        .line(Duration::MAX)
        .is_some_and(|line| !line.eq_ignore_ascii_case("s"))
}

/// Write `seeds.toml` when discovery needs it (asking first on a terminal);
/// whether it was written.
fn offer_seeds(
    data_dir: &Path,
    report: &Report,
    direct: &Direct,
    prompt: Option<&Prompt>,
) -> anyhow::Result<bool> {
    let path = data_dir.join("seeds.toml");
    let players: Vec<IpAddr> = direct
        .discover()
        .players
        .iter()
        .filter_map(|p| p.ip.parse().ok())
        .collect();
    let Some(seeds) = seeds_offer(report, &players, path.exists()) else {
        return Ok(false);
    };
    if let Some(prompt) = prompt
        && !prompt.ask(&format!(
            "Write {} so every run finds these players?",
            path.display()
        ))
    {
        return Ok(false);
    }
    std::fs::write(&path, seeds)?;
    tracing::info!("wrote {}", path.display());
    Ok(true)
}

/// `fsonos setup`; see the module docs.
pub fn run(global: &GlobalArgs, args: &SetupArgs) -> anyhow::Result<ExitCode> {
    let data_dir = crate::daemon::data_dir(global)?;
    std::fs::create_dir_all(&data_dir)?;
    let interactive = !args.yes && !global.json && std::io::stdin().is_terminal();
    // One reader for every question setup asks.
    let prompt = interactive.then(Prompt::start);
    let prompt = prompt.as_ref();
    let (mut direct, mut report) = check(global, &args.serve)?;
    let mut outcomes = Vec::new();
    for step in Step::ALL {
        let outcome = loop {
            let mut outcome = match step {
                Step::SpotifyLogin => spotify_login(global, args, prompt),
                Step::NextSteps => next_steps(&args.serve),
                _ => judge(step, &report),
            };
            if step == Step::Discovery && offer_seeds(&data_dir, &report, &direct, prompt)? {
                (direct, report) = check(global, &args.serve)?;
                outcome = judge(step, &report);
                outcome.summary.push_str(" (seeds.toml written)");
            }
            if step == Step::Households
                && outcome.status == Status::Pass
                && let Some(line) = households_line(&direct)
            {
                outcome.summary = line;
            }
            let Some(prompt) = prompt.filter(|_| outcome.status == Status::Fail) else {
                break outcome;
            };
            print(false, &outcome)?;
            if !again(prompt) {
                break outcome;
            }
            (direct, report) = check(global, &args.serve)?;
        };
        print(global.json, &outcome)?;
        outcomes.push(outcome);
    }
    Ok(ExitCode::from(exit_code(&outcomes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::{CheckId, CheckResult, Entry};

    fn entry(id: &'static str, result: CheckResult) -> Entry {
        Entry {
            id: CheckId(id),
            title: id.to_owned(),
            result,
        }
    }

    fn report(entries: Vec<Entry>) -> Report {
        Report { entries }
    }

    #[test]
    fn a_step_is_its_worst_check() {
        let r = report(vec![
            entry(
                "lan.ssdp",
                CheckResult::warn("no SSDP answers", "Check multicast."),
            ),
            entry("lan.seeds", CheckResult::pass("2 seeds answer")),
            entry("lan.players", CheckResult::pass("4 players read")),
            entry("lan.households", CheckResult::pass("2 households")),
            entry("spotify.linked.s1", CheckResult::skip("no S1 household")),
        ]);
        let discovery = judge(Step::Discovery, &r);
        assert_eq!(discovery.status, Status::Warn);
        assert_eq!(discovery.summary, "no SSDP answers");
        assert_eq!(discovery.remedies, ["Check multicast."]);
        assert_eq!(judge(Step::Households, &r).status, Status::Pass);
        // Only skips: the step is skipped too.
        assert_eq!(judge(Step::SpotifyHouseholds, &r).status, Status::Skip);
        // Nothing at all to check.
        let tailscale = judge(Step::Tailscale, &r);
        assert_eq!(
            (tailscale.status, tailscale.summary.as_str()),
            (Status::Skip, "nothing to check here")
        );
    }

    #[test]
    fn seeds_are_offered_only_when_multicast_fails_and_players_answer() {
        let players: Vec<IpAddr> =
            vec!["192.0.2.10".parse().unwrap(), "192.0.2.11".parse().unwrap()];
        let failing = report(vec![entry(
            "lan.ssdp",
            CheckResult::fail("no SSDP answers", "Check multicast."),
        )]);
        let seeds = seeds_offer(&failing, &players, false).expect("an offer");
        assert!(
            seeds.ends_with("players = [\"192.0.2.10\", \"192.0.2.11\"]\n"),
            "{seeds}"
        );
        assert_eq!(
            crate::config::ip_addresses(&seeds),
            players,
            "the file reads back as these seeds"
        );
        assert_eq!(
            seeds_offer(&failing, &players, true),
            None,
            "a file is there"
        );
        assert_eq!(
            seeds_offer(&failing, &[], false),
            None,
            "no player answered"
        );
        let passing = report(vec![entry("lan.ssdp", CheckResult::pass("4 answer"))]);
        assert_eq!(
            seeds_offer(&passing, &players, false),
            None,
            "multicast works"
        );
    }

    #[test]
    fn the_exit_code_is_the_worst_step() {
        let at = |status| StepOutcome::new(Step::Data, status, "");
        assert_eq!(exit_code(&[at(Status::Pass), at(Status::Skip)]), EXIT_OK);
        assert_eq!(exit_code(&[at(Status::Pass), at(Status::Warn)]), EXIT_WARN);
        assert_eq!(exit_code(&[at(Status::Warn), at(Status::Fail)]), EXIT_FAIL);
        assert_eq!(exit_code(&[]), EXIT_OK);
    }

    #[test]
    fn steps_run_in_order_and_serialize_by_name() {
        assert_eq!(Step::ALL.first(), Some(&Step::Data));
        assert_eq!(Step::ALL.last(), Some(&Step::NextSteps));
        let line =
            serde_json::to_value(StepOutcome::new(Step::SpotifyLogin, Status::Skip, "x")).unwrap();
        assert_eq!(line["step"], "spotify-login");
        assert_eq!(line["status"], "skip");
        assert!(line.get("remedies").is_none());
    }
}
