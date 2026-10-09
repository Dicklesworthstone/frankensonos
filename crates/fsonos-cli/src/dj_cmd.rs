//! `fsonos dj …`: start, skip, stop and steer the DJ, show what it plays,
//! why, and its moods, and like or dislike the work playing.
//!
//! The DJ runs in the daemon (`fsonos serve`): only that process holds the
//! feeds that keep a queue topped up. So the reads (`dj status`, `dj why`,
//! `dj moods`) ask the daemon when one answers (found the way every command
//! finds it, see `crate::remote`; `--daemon` requires it, `--direct` skips
//! it), through the same HTTP API an agent uses, and otherwise read directly,
//! which shows the steering (kept in the data directory) but no running DJ.
//! Steering is stored, so a steer from here reaches the daemon's next pick.

use clap::{ArgAction, Subcommand};
use fsonos_api::dj::{DjMoodsDto, DjStatusDto, SteerConstraints};
use fsonos_api::plan::{self, DjAction as PlanDj, Rooms};
use fsonos_api::surface::dj_feedback::DjFeedback;
use fsonos_api::{
    Command, DjStartRequest, DjSteerRequest, ErrorCode, Failure, ZoneDto, ZoneRequest,
};
use std::fmt::Write as _;

use crate::config::GlobalArgs;
use crate::direct::Direct;
use crate::remote::{Daemon, pct as encode};

#[derive(Subcommand)]
pub enum DjAction {
    /// Start the DJ in a room's group; with --mood, steered by it.
    Start {
        zone: String,
        /// A mood (fsonos dj moods lists them).
        #[arg(long)]
        mood: Option<String>,
        /// How long the mood lasts: 2h, 90m, 1h30m [default: until
        /// cleared].
        #[arg(long = "for", value_name = "SPAN", value_parser = parse_span, requires = "mood")]
        for_secs: Option<u64>,
    },
    /// Skip the DJ's current work.
    Skip { zone: String },
    /// Stop the DJ.
    Stop { zone: String },
    /// You like the work playing: the DJ favors it, its composer and its
    /// performer. Without a room, the group the DJ plays in.
    Like { zone: Option<String> },
    /// You dislike the work playing: the DJ plays it, its composer and its
    /// performer less (a second dislike keeps the work out for 180 days).
    Dislike { zone: Option<String> },
    /// Steer the DJ in a room's group from its next piece, running or not:
    /// a mood, constraints, how long; or --clear for the time-of-day program.
    Steer(Box<SteerArgs>),
    /// What the DJ plays (in every zone it runs in, without a room), why,
    /// what comes next, and the steering.
    Status { zone: Option<String> },
    /// Why the DJ chose the work playing: every factor and relaxation.
    Why { zone: String },
    /// The moods and time-of-day programs, and the steering in effect now.
    Moods {
        /// A room: its steering, rather than the house's program.
        zone: Option<String>,
    },
    /// The standing preferences, for every group's DJ: favor or avoid
    /// genres, artists, eras and moods; pin or ban artists, albums and
    /// tracks; a default energy; whether explicit tracks may play.
    Prefs {
        #[command(subcommand)]
        action: crate::dj::prefs::PrefsAction,
    },
}

/// `fsonos dj steer`: the fields of `dj_steer`.
#[derive(clap::Args)]
pub struct SteerArgs {
    /// The room; its group is steered.
    zone: String,
    /// A mood: focus, dinner, sunday-morning, bright, calm, or one in
    /// moods.toml.
    #[arg(long)]
    mood: Option<String>,
    /// Only this composer (repeat for more).
    #[arg(long = "composer", value_name = "NAME")]
    include_composers: Vec<String>,
    /// Never this composer.
    #[arg(long = "not-composer", value_name = "NAME")]
    exclude_composers: Vec<String>,
    /// Only works by this artist (repeat for more): any credited artist, a
    /// classical work's performers included.
    #[arg(long = "artist", value_name = "NAME")]
    include_artists: Vec<String>,
    /// Nothing by this artist.
    #[arg(long = "not-artist", value_name = "NAME")]
    exclude_artists: Vec<String>,
    /// Only this period: medieval, renaissance, baroque, classical,
    /// romantic, late_romantic, impressionist, modern, contemporary.
    #[arg(long = "period")]
    periods: Vec<String>,
    /// A keyword the work must have: piano, chamber, orchestral, choral,
    /// opera, song, vocal (each finds its forms), or any whole word.
    #[arg(long = "with", value_name = "KEYWORD")]
    include_keywords: Vec<String>,
    /// A keyword it must not have.
    #[arg(long = "without", value_name = "KEYWORD")]
    exclude_keywords: Vec<String>,
    /// Works at least this long.
    #[arg(long = "min-minutes", value_name = "N")]
    min_work_minutes: Option<u32>,
    /// Works at most this long.
    #[arg(long = "max-minutes", value_name = "N")]
    max_work_minutes: Option<u32>,
    /// Calmer; twice for much calmer.
    #[arg(long, action = ArgAction::Count, conflicts_with_all = ["brighter", "energy"])]
    calmer: u8,
    /// Brighter; twice for much brighter.
    #[arg(long, action = ArgAction::Count, conflicts_with = "energy")]
    brighter: u8,
    /// The energy shift: -2 (much calmer) to 2 (much brighter).
    #[arg(long, allow_hyphen_values = true, value_name = "-2..2")]
    energy: Option<i8>,
    /// Let long works (operas, Passions) in.
    #[arg(long)]
    allow_long: bool,
    /// How long: 2h, 90m, 1h30m [default: until cleared].
    #[arg(long = "for", value_name = "SPAN", value_parser = parse_span)]
    for_secs: Option<u64>,
    /// Clear the steering: back to the time-of-day program.
    #[arg(long)]
    clear: bool,
}

impl SteerArgs {
    fn request(&self) -> DjSteerRequest {
        let step = |n: u8| i8::try_from(n).unwrap_or(i8::MAX);
        DjSteerRequest {
            zone: self.zone.clone(),
            mood: self.mood.clone(),
            constraints: SteerConstraints {
                include_composers: self.include_composers.clone(),
                exclude_composers: self.exclude_composers.clone(),
                include_artists: self.include_artists.clone(),
                exclude_artists: self.exclude_artists.clone(),
                periods: self.periods.clone(),
                include_keywords: self.include_keywords.clone(),
                exclude_keywords: self.exclude_keywords.clone(),
                min_work_minutes: self.min_work_minutes,
                max_work_minutes: self.max_work_minutes,
                energy_bias: self
                    .energy
                    .unwrap_or_else(|| step(self.brighter).saturating_sub(step(self.calmer))),
                allow_long_works: self.allow_long,
            },
            for_secs: self.for_secs,
            clear: self.clear,
        }
    }
}

impl DjAction {
    /// Whether it runs on its own rather than as a planned control command:
    /// the reads (status, why, moods), and like and dislike.
    #[must_use]
    pub fn is_read(&self) -> bool {
        matches!(
            self,
            Self::Status { .. }
                | Self::Why { .. }
                | Self::Moods { .. }
                | Self::Like { .. }
                | Self::Dislike { .. }
                | Self::Prefs { .. }
        )
    }

    /// The tool it is authorized (and logged) as.
    #[must_use]
    pub fn tool(&self) -> &'static str {
        match self {
            Self::Start { .. } => "dj_start",
            Self::Skip { .. } => "dj_skip",
            Self::Stop { .. } => "dj_stop",
            Self::Steer(_) => "dj_steer",
            Self::Status { .. } | Self::Why { .. } => "dj_status",
            Self::Moods { .. } => "dj_moods",
            Self::Like { .. } | Self::Dislike { .. } => "dj_feedback",
            Self::Prefs {
                action: crate::dj::prefs::PrefsAction::Show,
            } => "dj_preferences",
            Self::Prefs { .. } => "dj_prefer",
        }
    }

    /// The shared request a control action stands for, planned against
    /// `rooms`.
    pub fn plan(&self, rooms: &Rooms<'_>) -> Result<Command, Failure> {
        let zone = |zone: &String| ZoneRequest { zone: zone.clone() };
        match self {
            Self::Start {
                zone,
                mood,
                for_secs,
            } => plan::plan_dj_start(
                rooms,
                &DjStartRequest {
                    zone: zone.clone(),
                    mood: mood.clone(),
                    for_secs: *for_secs,
                },
            ),
            Self::Skip { zone: z } => plan::plan_dj(rooms, &zone(z), PlanDj::Skip),
            Self::Stop { zone: z } => plan::plan_dj(rooms, &zone(z), PlanDj::Stop),
            Self::Steer(args) => plan::plan_dj_steer(rooms, &args.request()),
            Self::Status { .. }
            | Self::Why { .. }
            | Self::Moods { .. }
            | Self::Like { .. }
            | Self::Dislike { .. }
            | Self::Prefs { .. } => Err(Failure::new(
                ErrorCode::Internal,
                "dj status, why, moods, like and dislike run on their own",
            )),
        }
    }
}

/// `fsonos dj status|why|moods`.
pub fn read(global: &GlobalArgs, action: &DjAction) -> anyhow::Result<()> {
    match action {
        DjAction::Status { zone } => {
            let from = Source::find(global)?;
            if let Some(zone) = zone {
                let status = from.status(zone)?;
                crate::emit(global.json, &status, |s| format!("{}\n", s.summary()))
            } else {
                let statuses = from.statuses()?;
                crate::emit(global.json, &statuses, |all| statuses_text(all))
            }
        }
        DjAction::Why { zone } => {
            let status = Source::find(global)?.status(zone)?;
            let text = why_text(&status)?;
            crate::emit(global.json, &status, |_| text.clone())
        }
        DjAction::Moods { zone } => {
            let moods = Source::find(global)?.moods(zone.as_deref())?;
            crate::emit(global.json, &moods, moods_text)
        }
        DjAction::Like { zone } => {
            crate::dj::feedback::run(global, zone.as_deref(), DjFeedback::Like)
        }
        DjAction::Dislike { zone } => {
            crate::dj::feedback::run(global, zone.as_deref(), DjFeedback::Dislike)
        }
        DjAction::Prefs { action } => crate::dj::prefs::run(global, action),
        _ => Err(Failure::new(ErrorCode::Internal, "not a DJ read").into()),
    }
}

/// Where the DJ's state is read from.
enum Source {
    Daemon(Daemon),
    Direct(Box<Direct>),
}

impl Source {
    /// The daemon when one answers, else the speakers and store directly.
    fn find(global: &GlobalArgs) -> Result<Self, Failure> {
        if let Some(daemon) = Daemon::find(global, global.daemon)? {
            return Ok(Self::Daemon(daemon));
        }
        tracing::info!(
            "no daemon answers: the DJ runs in fsonos serve, so only its steering shows here"
        );
        Ok(Self::Direct(Box::new(Direct::survey(global)?)))
    }

    fn status(&self, zone: &str) -> Result<DjStatusDto, Failure> {
        match self {
            Self::Daemon(daemon) => daemon.get(&format!("/zones/{}/dj", encode(zone))),
            Self::Direct(direct) => direct.dj_status(zone),
        }
    }

    /// Every zone's.
    fn statuses(&self) -> Result<Vec<DjStatusDto>, Failure> {
        let zones: Vec<ZoneDto> = match self {
            Self::Daemon(daemon) => daemon.get("/zones")?,
            Self::Direct(direct) => direct.zones()?,
        };
        zones
            .iter()
            .map(|z| self.status(&format!("{}@{}", z.coordinator_room, z.household)))
            .collect()
    }

    fn moods(&self, zone: Option<&str>) -> Result<DjMoodsDto, Failure> {
        match self {
            Self::Daemon(daemon) => {
                let query = zone.map_or_else(String::new, |z| format!("?zone={}", encode(z)));
                daemon.get(&format!("/dj/moods{query}"))
            }
            Self::Direct(direct) => direct.dj_moods(zone),
        }
    }
}

/// A span: `2h`, `90m`, `1h30m`, `1d`, `45s`, or bare minutes (`90`).
pub fn parse_span(text: &str) -> Result<u64, String> {
    let bad = || format!("{text:?} is not a span like 2h, 90m or 1h30m");
    let span = text.trim().to_ascii_lowercase();
    if let Ok(minutes) = span.parse::<u64>() {
        return minutes.checked_mul(60).ok_or_else(bad);
    }
    let (mut secs, mut digits, mut units) = (0_u64, String::new(), 0);
    for c in span.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            'd' => 86_400,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return Err(bad()),
        };
        let n: u64 = digits.parse().map_err(|_| bad())?;
        secs = n
            .checked_mul(unit)
            .and_then(|s| secs.checked_add(s))
            .ok_or_else(bad)?;
        digits.clear();
        units += 1;
    }
    if units == 0 || !digits.is_empty() {
        return Err(bad());
    }
    Ok(secs)
}

/// Every zone the DJ runs in, or that it runs nowhere.
fn statuses_text(all: &[DjStatusDto]) -> String {
    let running: Vec<String> = all
        .iter()
        .filter(|s| s.running)
        .map(|s| format!("{}\n", s.summary()))
        .collect();
    if running.is_empty() {
        "The DJ isn't running in any zone.\n".to_owned()
    } else {
        running.concat()
    }
}

/// `fsonos dj why`: the work playing, every factor that moved its odds and
/// every steering filter relaxed for it.
fn why_text(status: &DjStatusDto) -> Result<String, Failure> {
    let Some(work) = &status.now else {
        return Err(Failure::new(
            ErrorCode::NoDjSession,
            format!("none of the DJ's works plays in {}'s group", status.zone),
        )
        .with_hint(format!("Start it: fsonos dj start {:?}.", status.zone)));
    };
    let mut text = work.line();
    if let (Some(m), Some(title)) = (work.movement, &work.movement_title) {
        let _ = write!(text, ", movement {m} of {}: {title}", work.movements);
    }
    let _ = writeln!(text, "\nWhy: {}", work.reason.summary);
    let width = work
        .reason
        .factors
        .iter()
        .map(|f| f.factor.len())
        .max()
        .unwrap_or(0);
    for f in &work.reason.factors {
        let weight = f64::from(f.weight) / 1000.0;
        let _ = writeln!(text, "  {:width$}  ×{weight:.2}", f.factor);
    }
    if !work.reason.relaxed.is_empty() {
        let _ = writeln!(
            text,
            "Relaxed (too few works passed): {}",
            work.reason.relaxed.join(", ")
        );
    }
    let _ = writeln!(text, "Steering: {}", status.steering.summary());
    Ok(text)
}

/// `fsonos dj moods`: one mood per line, then the programs, then what is in
/// effect now (marked).
fn moods_text(moods: &DjMoodsDto) -> String {
    let width = moods.moods.iter().map(|m| m.name.len()).max().unwrap_or(0);
    let mut text = String::from("Moods:\n");
    for m in &moods.moods {
        let mark = if moods.now.mood.as_deref() == Some(m.name.as_str()) {
            '*'
        } else {
            ' '
        };
        let about = fsonos_api::dj::describe_steer(None, &m.constraints, None);
        let _ = writeln!(text, "{mark} {:width$}  {about}", m.name);
    }
    if !moods.programs.is_empty() {
        text.push_str("Programs:\n");
        for p in &moods.programs {
            let _ = writeln!(
                text,
                "  {:<27}  {}–{}  {}",
                p.days.join(","),
                p.from,
                p.to,
                p.mood
            );
        }
    }
    let _ = writeln!(text, "Now: {}", moods.now.summary());
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use fsonos_api::dj::{DjFactorDto, DjReasonDto, DjSteer, DjSteeringDto, DjWorkDto};

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        dj: DjAction,
    }

    fn parse(args: &[&str]) -> DjAction {
        Cli::try_parse_from(std::iter::once("dj").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
            .dj
    }

    fn steer_of(args: &[&str]) -> DjSteerRequest {
        match parse(args) {
            DjAction::Steer(steer) => steer.request(),
            _ => panic!("not a steer"),
        }
    }

    #[test]
    fn spans_read_as_people_write_them() {
        assert_eq!(parse_span("2h"), Ok(7200));
        assert_eq!(parse_span("90m"), Ok(5400));
        assert_eq!(parse_span("1h30m"), Ok(5400));
        assert_eq!(parse_span(" 1D "), Ok(86_400));
        assert_eq!(parse_span("45"), Ok(2700), "bare minutes");
        for bad in ["", "h", "2x", "1h30", "-2h"] {
            assert!(parse_span(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn steer_flags_become_the_shared_request() {
        let req = steer_of(&[
            "steer",
            "Kitchen",
            "--mood",
            "focus",
            "--composer",
            "Bach",
            "--composer",
            "Handel",
            "--without",
            "vocal",
            "--period",
            "baroque",
            "--max-minutes",
            "30",
            "--calmer",
            "--calmer",
            "--for",
            "2h",
        ]);
        assert_eq!(req.zone, "Kitchen");
        assert_eq!(req.mood.as_deref(), Some("focus"));
        assert_eq!(req.constraints.include_composers, ["Bach", "Handel"]);
        assert_eq!(req.constraints.exclude_keywords, ["vocal"]);
        assert_eq!(req.constraints.periods, ["baroque"]);
        assert_eq!(req.constraints.max_work_minutes, Some(30));
        assert_eq!(req.constraints.energy_bias, -2);
        assert_eq!(req.for_secs, Some(7200));
        let artists = steer_of(&[
            "steer",
            "Den",
            "--artist",
            "Miles Davis",
            "--not-artist",
            "The Beatles",
        ]);
        assert_eq!(artists.constraints.include_artists, ["Miles Davis"]);
        assert_eq!(artists.constraints.exclude_artists, ["The Beatles"]);
        assert_eq!(
            steer_of(&["steer", "Den", "--brighter"])
                .constraints
                .energy_bias,
            1
        );
        assert_eq!(
            steer_of(&["steer", "Den", "--energy", "-1"])
                .constraints
                .energy_bias,
            -1
        );
        assert_eq!(
            steer_of(&["steer", "Den", "--clear"]).steer().unwrap(),
            DjSteer::Clear
        );
        assert!(
            Cli::try_parse_from(["dj", "steer", "Den", "--calmer", "--brighter"]).is_err(),
            "calmer and brighter conflict"
        );
        assert!(
            Cli::try_parse_from(["dj", "start", "Den", "--for", "2h"]).is_err(),
            "--for needs --mood"
        );
    }

    fn steering() -> DjSteeringDto {
        DjSteeringDto {
            source: "session".into(),
            mood: Some("focus".into()),
            constraints: SteerConstraints {
                exclude_keywords: vec!["vocal".into()],
                energy_bias: -1,
                ..SteerConstraints::default()
            },
            expires_at: Some(1_790_007_200),
            expires_in_secs: Some(7200),
        }
    }

    #[test]
    fn why_names_every_factor() {
        let work = DjWorkDto {
            composer: "Johannes Brahms".into(),
            title: "Symphony No. 4 in E Minor, Op. 98".into(),
            performers: vec!["Sim Ensemble".into()],
            album: Some("Johannes Brahms: Works".into()),
            movements: 2,
            movement: Some(2),
            movement_title: Some("II. Andante".into()),
            minutes: Some(10),
            reason: DjReasonDto {
                summary: "Brahms not heard in 4 days".into(),
                factors: vec![
                    DjFactorDto {
                        factor: "composer_spacing".into(),
                        weight: 1400,
                    },
                    DjFactorDto {
                        factor: "energy_fit".into(),
                        weight: 850,
                    },
                ],
                relaxed: vec!["keyword".into()],
            },
        };
        let status = DjStatusDto {
            zone: "Den".into(),
            running: true,
            now: Some(work),
            next: Vec::new(),
            steering: steering(),
        };
        assert_eq!(
            why_text(&status).unwrap(),
            "Johannes Brahms: Symphony No. 4 in E Minor, Op. 98 (Sim Ensemble; 10 min), \
             movement 2 of 2: II. Andante\n\
             Why: Brahms not heard in 4 days\n  \
             composer_spacing  ×1.40\n  \
             energy_fit        ×0.85\n\
             Relaxed (too few works passed): keyword\n\
             Steering: steered: focus mood, without vocal, calmer, for another 2 hours\n"
        );
        let idle = DjStatusDto {
            now: None,
            running: false,
            ..status
        };
        assert_eq!(why_text(&idle).unwrap_err().code, ErrorCode::NoDjSession);
        assert_eq!(
            statuses_text(&[idle]),
            "The DJ isn't running in any zone.\n"
        );
    }
}
