//! The DJ as `dj_status`, `dj_steer` and `dj_moods` show it: fsonos-spotify's
//! feed, steering and moods as the surfaces' DTOs ([`fsonos_api::dj`]).

use chrono::Datelike;
pub use fsonos_api::dj::describe_steer as describe;
use fsonos_api::dj::{
    DjFactorDto, DjMoodDto, DjMoodsDto, DjReasonDto, DjStatusDto, DjSteeringDto, DjWorkDto,
    SteerConstraints,
};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::Clock;
use fsonos_core::store::Store;
use fsonos_spotify::SpotifyError;
use fsonos_spotify::classical::composer_matches;
use fsonos_spotify::dj::{Factor, PickReason, WorkPool};
use fsonos_spotify::feed::{QueueFeed, QueuedWork};
use fsonos_spotify::library::split_artists;
use fsonos_spotify::steer::{DjConstraints, DjSession, Moods, steering};
use fsonos_types::PlayerId;

/// How many of the DJ's works after the one playing a status shows.
const NEXT: usize = 2;

/// A steering failure, coded: an unknown mood (with the moods that exist),
/// or bounds that clash (`hint` says whose).
pub fn steer_failure(err: SpotifyError, hint: &str) -> Failure {
    match err {
        SpotifyError::UnknownMood { name, known } => {
            Failure::new(ErrorCode::UnknownMood, format!("no mood {name:?}"))
                .with_suggestions(known)
        }
        SpotifyError::Config(why) => Failure::invalid(why).with_hint(hint),
        other => Failure::new(ErrorCode::Internal, format!("DJ steering: {other}")),
    }
}

pub fn store_failure(err: impl std::fmt::Display) -> Failure {
    Failure::new(ErrorCode::Internal, format!("the DJ's store: {err}"))
}

/// The surfaces' constraints as the DJ's. The fields are the same, so
/// serde carries them across; a period the DJ doesn't know is
/// INVALID_ARGUMENT, naming the ones it does.
pub fn to_dj(constraints: &SteerConstraints) -> Result<DjConstraints, Failure> {
    serde_json::to_value(constraints)
        .and_then(serde_json::from_value)
        .map_err(|e| Failure::invalid(format!("constraints: {e}")))
}

/// The DJ's constraints as the surfaces show them; the expiry is reported on
/// its own.
#[must_use]
pub fn from_dj(c: &DjConstraints) -> SteerConstraints {
    SteerConstraints {
        include_composers: c.include_composers.clone(),
        exclude_composers: c.exclude_composers.clone(),
        include_artists: c.include_artists.clone(),
        exclude_artists: c.exclude_artists.clone(),
        periods: c
            .periods
            .iter()
            .filter_map(|p| serde_json::to_value(p).ok()?.as_str().map(str::to_owned))
            .collect(),
        include_keywords: c.include_keywords.clone(),
        exclude_keywords: c.exclude_keywords.clone(),
        min_work_minutes: c.min_work_minutes,
        max_work_minutes: c.max_work_minutes,
        energy_bias: c.energy_bias,
        allow_long_works: c.allow_long_works,
    }
}

/// How the DJ's next pick in `coordinator`'s group is steered at the
/// clock's now: its session while that lasts, else the time-of-day program.
pub fn steering_of(
    store: &dyn Store,
    moods: &Moods,
    coordinator: &PlayerId,
    clock: &dyn Clock,
) -> Result<DjSteeringDto, Failure> {
    let local = clock.now();
    let now = local.timestamp();
    let session = store
        .dj_session(&coordinator.0)
        .map_err(store_failure)?
        .map(|row| DjSession::from_store(&row))
        .transpose()
        .map_err(store_failure)?
        .filter(|session| !session.expired(now));
    let steer = steering(
        store,
        moods,
        &coordinator.0,
        now,
        Some((local.weekday(), local.time())),
    )
    .map_err(|e| steer_failure(e, "Steer the zone again, or clear its steering."))?;
    let source = match (&session, &steer) {
        (Some(_), _) => "session",
        (None, Some(steer)) if steer.mood.is_some() => "program",
        _ => "none",
    };
    let expires_at = steer.as_ref().and_then(|s| s.constraints.expires_at);
    Ok(DjSteeringDto {
        source: source.to_owned(),
        constraints: steer
            .as_ref()
            .map(|s| from_dj(&s.constraints))
            .unwrap_or_default(),
        mood: steer.and_then(|s| s.mood),
        expires_at,
        expires_in_secs: expires_at.map(|at| at.saturating_sub(now).max(0).unsigned_abs()),
    })
}

/// The house's time-of-day program at the clock's now.
#[must_use]
pub fn program_now(moods: &Moods, clock: &dyn Clock) -> DjSteeringDto {
    let local = clock.now();
    let mood = moods.program_at(local.weekday(), local.time());
    DjSteeringDto {
        source: if mood.is_some() { "program" } else { "none" }.to_owned(),
        constraints: mood
            .and_then(|m| moods.get(m))
            .map(from_dj)
            .unwrap_or_default(),
        mood: mood.map(str::to_owned),
        expires_at: None,
        expires_in_secs: None,
    }
}

/// Every mood and program, with the steering in effect `now`.
#[must_use]
pub fn moods_of(moods: &Moods, now: DjSteeringDto) -> DjMoodsDto {
    DjMoodsDto {
        moods: moods
            .names()
            .map(|name| DjMoodDto {
                name: name.to_owned(),
                constraints: moods.get(name).map(from_dj).unwrap_or_default(),
            })
            .collect(),
        // fsonos-spotify writes a program as moods.toml does: days, from,
        // to, mood.
        programs: moods
            .programs()
            .iter()
            .filter_map(|p| {
                serde_json::to_value(p)
                    .and_then(serde_json::from_value)
                    .ok()
            })
            .collect(),
        now,
    }
}

/// The DJ's status in `zone`, given its feed there (if it runs), the pool it
/// picks from, and the group's queue position.
#[must_use]
pub fn status(
    zone: String,
    feed: Option<&QueueFeed>,
    pool: &WorkPool,
    queue_position: Option<u32>,
    steering: DjSteeringDto,
) -> DjStatusDto {
    let Some(feed) = feed.filter(|f| f.is_active()) else {
        return DjStatusDto {
            zone,
            running: false,
            now: None,
            next: Vec::new(),
            steering,
        };
    };
    let queued = feed.queued();
    // The work playing: the feed's own idea, else where the queue is.
    let playing = feed
        .current()
        .and_then(|current| queued.iter().position(|w| std::ptr::eq(w, current)))
        .or_else(|| queue_position.and_then(|at| queued.iter().position(|w| w.contains(at))));
    DjStatusDto {
        zone,
        running: true,
        now: playing.map(|i| work(pool, &queued[i], queue_position)),
        next: queued
            .iter()
            .skip(playing.map_or(0, |i| i + 1))
            .take(NEXT)
            .map(|w| work(pool, w, None))
            .collect(),
        steering,
    }
}

/// One of the DJ's works; with the queue position, the movement playing.
fn work(pool: &WorkPool, queued: &QueuedWork, queue_position: Option<u32>) -> DjWorkDto {
    let known = queued.movements.first().and_then(|m| pool.work_of(&m.uri));
    let track = known.and_then(|w| w.movements.first()).map(|m| &m.track);
    let performers = track
        .and_then(|t| t.artist.as_deref())
        .map(split_artists)
        .unwrap_or_default()
        .into_iter()
        .filter(|artist| !composer_matches(artist, &queued.composer))
        .collect();
    let playing =
        queue_position.and_then(|at| queued.movements.iter().find(|m| m.position == Some(at)));
    let index = playing.and_then(|m| queued.movements.iter().position(|q| q.uri == m.uri));
    DjWorkDto {
        composer: queued.composer.clone(),
        title: queued.title.clone(),
        performers,
        album: track.and_then(|t| t.album.clone()),
        movements: count(queued.movements.len()),
        movement: index.map(|i| count(i + 1)),
        // The movement part of its title ("II. Andante"), else the title.
        movement_title: playing.map(|m| {
            known
                .and_then(|w| w.movements.iter().find(|k| k.track.source_uri == m.uri))
                .and_then(|k| k.movement.clone())
                .unwrap_or_else(|| m.title.clone())
        }),
        minutes: known
            .map(|w| w.total_secs)
            .filter(|&secs| secs > 0)
            .map(|secs| secs.div_ceil(60)),
        reason: reason(&queued.reason),
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn reason(r: &PickReason) -> DjReasonDto {
    DjReasonDto {
        summary: r.summary.clone(),
        factors: r
            .factors
            .iter()
            .map(|&(factor, weight)| DjFactorDto {
                factor: factor_name(factor),
                weight,
            })
            .collect(),
        relaxed: r.relaxed.iter().map(|x| x.label().to_owned()).collect(),
    }
}

/// A factor's serde name (`ComposerSpacing`) in snake case
/// (`composer_spacing`), so a factor fsonos-spotify adds needs nothing here.
fn factor_name(factor: Factor) -> String {
    let name = serde_json::to_value(factor)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut snake = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                snake.push('_');
            }
            snake.push(c.to_ascii_lowercase());
        } else {
            snake.push(c);
        }
    }
    snake
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use fsonos_core::clock::FakeClock;
    use fsonos_core::store::MemStore;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|&w| w.to_owned()).collect()
    }

    #[test]
    fn constraints_cross_to_the_dj_and_back() {
        let ours = SteerConstraints {
            include_composers: words(&["Bach"]),
            include_artists: words(&["Yo-Yo Ma"]),
            exclude_artists: words(&["The Beatles"]),
            periods: words(&["late_romantic", "baroque"]),
            exclude_keywords: words(&["vocal"]),
            max_work_minutes: Some(30),
            energy_bias: -1,
            ..SteerConstraints::default()
        };
        let theirs = to_dj(&ours).unwrap();
        assert_eq!(theirs.energy_bias, -1);
        assert_eq!(theirs.periods.len(), 2);
        assert_eq!(from_dj(&theirs), ours);
        let rococo = SteerConstraints {
            periods: words(&["rococo"]),
            ..SteerConstraints::default()
        };
        let err = to_dj(&rococo).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            err.detail.contains("rococo") && err.detail.contains("baroque"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn a_steer_reads_as_words() {
        let c = SteerConstraints {
            include_composers: words(&["Bach", "Handel"]),
            periods: words(&["late_romantic"]),
            exclude_keywords: words(&["vocal"]),
            max_work_minutes: Some(30),
            energy_bias: -1,
            ..SteerConstraints::default()
        };
        assert_eq!(
            describe(Some("focus"), &c, Some(7200)),
            "focus mood, Bach or Handel only, late-romantic works, without vocal, \
             works up to 30 minutes, calmer, for 2 hours"
        );
        assert_eq!(
            describe(None, &SteerConstraints::default(), None),
            "no steering"
        );
        let artists = SteerConstraints {
            include_artists: words(&["Miles Davis"]),
            exclude_artists: words(&["The Beatles"]),
            ..SteerConstraints::default()
        };
        assert_eq!(
            describe(None, &artists, None),
            "by Miles Davis, nothing by The Beatles"
        );
    }

    #[test]
    fn factors_are_named_in_snake_case() {
        assert_eq!(factor_name(Factor::ComposerSpacing), "composer_spacing");
        assert_eq!(factor_name(Factor::EnergyFit), "energy_fit");
        assert_eq!(factor_name(Factor::Liked), "liked");
    }

    #[test]
    fn steering_is_the_session_while_it_lasts_then_the_program() {
        // A Wednesday at 08:00: the built-in weekday program plays bright.
        let clock =
            FakeClock::new(DateTime::parse_from_rfc3339("2026-10-07T08:00:00-04:00").unwrap());
        let moods = Moods::builtin();
        let den = PlayerId("RINCON_DEN".into());
        let mut store = MemStore::default();
        let shown = steering_of(&store, &moods, &den, &clock).unwrap();
        assert_eq!(
            (shown.source.as_str(), shown.mood.as_deref()),
            ("program", Some("bright"))
        );
        assert_eq!(shown.constraints.energy_bias, 2);
        assert_eq!(program_now(&moods, &clock), shown);

        let now = clock.now().timestamp();
        let session = DjSession {
            zone: den.0.clone(),
            mood: Some("focus".into()),
            constraints: to_dj(&SteerConstraints {
                include_composers: words(&["Bach"]),
                ..SteerConstraints::default()
            })
            .unwrap(),
            expires_at: Some(now + 3600),
        };
        store.save_dj_session(&session.to_store().unwrap()).unwrap();
        let shown = steering_of(&store, &moods, &den, &clock).unwrap();
        assert_eq!(
            (shown.source.as_str(), shown.mood.as_deref()),
            ("session", Some("focus"))
        );
        assert_eq!(shown.constraints.include_composers, ["Bach"]);
        assert_eq!(shown.constraints.energy_bias, -1, "focus's own");
        assert_eq!(shown.expires_at, Some(now + 3600));
        assert_eq!(shown.expires_in_secs, Some(3600));
        assert_eq!(
            shown.summary(),
            "steered: focus mood, Bach only, without vocal, calmer, for another 1 hour"
        );

        // Four hours on, the session has lapsed, and at 12:00 no program plays.
        clock.advance(chrono::TimeDelta::hours(4));
        let shown = steering_of(&store, &moods, &den, &clock).unwrap();
        assert_eq!((shown.source.as_str(), shown.mood), ("none", None));
    }

    #[test]
    fn moods_list_every_mood_and_program() {
        let clock =
            FakeClock::new(DateTime::parse_from_rfc3339("2026-10-10T07:00:00-04:00").unwrap());
        let moods = Moods::builtin();
        let listed = moods_of(&moods, program_now(&moods, &clock));
        let names: Vec<&str> = listed.moods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["bright", "calm", "dinner", "focus", "sunday-morning"]
        );
        assert_eq!(listed.programs.len(), 4);
        let weekend = &listed.programs[0];
        assert_eq!(
            (
                weekend.mood.as_str(),
                weekend.from.as_str(),
                weekend.to.as_str()
            ),
            ("sunday-morning", "06:00", "11:00")
        );
        assert_eq!(weekend.days, ["sat", "sun"]);
        // A Saturday at 07:00.
        assert_eq!(listed.now.mood.as_deref(), Some("sunday-morning"));
    }

    #[test]
    fn a_dj_not_running_shows_only_its_steering() {
        let steering = DjSteeringDto {
            source: "none".into(),
            mood: None,
            constraints: SteerConstraints::default(),
            expires_at: None,
            expires_in_secs: None,
        };
        let shown = status("Den".into(), None, &WorkPool::default(), Some(3), steering);
        assert_eq!(
            shown.summary(),
            "The DJ isn't running in Den's group. Steering: none."
        );
        assert!(!shown.running && shown.now.is_none() && shown.next.is_empty());
        assert_eq!(
            serde_json::to_value(&shown).unwrap(),
            serde_json::json!({ "zone": "Den", "running": false, "steering": { "source": "none" } })
        );
    }
}
