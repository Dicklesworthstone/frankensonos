//! What a surface reads from the daemon's live model
//! ([`fsonos_core::live::Live`]) instead of asking the speakers: the
//! households, a group's transport and track, a room's volume, and the
//! model's own health for the doctor.

use fsonos_core::HouseholdState;
use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult};
use fsonos_core::live::{Live, Snapshot};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::reconcile::Health;
use serde_json::json;
use std::time::{Duration, Instant};

use crate::failure::{ErrorCode, Failure};
use crate::reads::TrackDto;

/// The households as the live model has them, or `NOT_READY` until its
/// first survey found a room.
pub fn households(live: &Live) -> Result<Vec<HouseholdState>, Failure> {
    let households = live.households();
    if households.iter().any(|h| !h.rooms.is_empty()) {
        return Ok(households);
    }
    let snapshot = live.snapshot();
    let detail = match &snapshot.last_error {
        Some(why) => format!("no Sonos rooms found yet; the last survey failed: {why}"),
        None => "no Sonos rooms found yet; the daemon is still surveying".to_string(),
    };
    Err(Failure::new(ErrorCode::NotReady, detail).with_hint(
        "Retry in a few seconds. If it persists, run `fsonos doctor` (discovery, seeds).",
    ))
}

/// Ask the model to survey now and wait up to `timeout` for that survey;
/// the households it found, or `None` when it did not finish in time. A
/// survey already reading when the request came in does not count: it
/// reports the house from before whatever made the caller ask (a player
/// that moved), so the follow-up the request queues is the answer.
#[must_use]
pub fn resurvey(live: &Live, timeout: Duration) -> Option<Vec<HouseholdState>> {
    let asked = Instant::now();
    live.refresh_soon();
    let deadline = asked + timeout;
    loop {
        let snapshot = live.snapshot();
        if snapshot.surveyed_at.is_some_and(|started| started >= asked) {
            return Some(snapshot.households);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `failure` as it is, or `PLAYER_UNREACHABLE` when it names an unknown
/// room the households list as vanished: a speaker that is off or off the
/// network, not a typo.
#[must_use]
pub fn vanished(failure: Failure, snapshot: &Snapshot) -> Failure {
    if failure.code != ErrorCode::UnknownRoom {
        return failure;
    }
    let Some(device) = failure
        .room
        .as_deref()
        .and_then(|room| snapshot.vanished_room(room))
    else {
        return failure;
    };
    let seen = device
        .last_known_ip
        .map_or_else(String::new, |ip| format!("; last seen at {ip}"));
    Failure::new(
        ErrorCode::PlayerUnreachable,
        format!(
            "{} is powered off or disconnected (its household lists it as vanished{seen})",
            device.zone_name
        ),
    )
    .with_hint("Check the speaker's power and network; it reappears on its own once it is back.")
}

/// The track a player's events describe, or `None` when it is on nothing.
#[must_use]
pub fn track(playback: &PlayerPlayback, now: Instant) -> Option<TrackDto> {
    let uri = playback.track_uri.clone().filter(|u| !u.is_empty())?;
    let meta = playback.now_playing.as_ref();
    Some(TrackDto {
        title: meta.map(|m| m.title.clone()).filter(|t| !t.is_empty()),
        creator: meta.and_then(|m| m.creator.clone()),
        album: meta.and_then(|m| m.album.clone()),
        uri,
        duration_secs: playback.duration_secs,
        position_secs: playback.position_at(now),
        queue_position: playback.queue_position.filter(|p| *p > 0),
    })
}

/// The live model as `GET /health` reports it: counts and a verdict. Blind
/// means no player has been found, which is how a daemon the operating
/// system keeps off the LAN (macOS Local Network privacy) or one on the
/// wrong network looks; starting, its first survey has not ended yet;
/// degraded, players offline or no event subscriptions.
#[must_use]
pub fn health(s: &Snapshot) -> crate::LiveHealth {
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let players: usize = s.households.iter().map(|h| h.players.len()).sum();
    let offline = s
        .health
        .iter()
        .filter(|(_, h)| h.health == Health::Offline)
        .count();
    let verdict = if players == 0 && s.surveyed_at.is_none() && s.last_error.is_none() {
        "starting"
    } else if players == 0 {
        "blind"
    } else if offline > 0 || s.subscriptions == 0 {
        "degraded"
    } else {
        "ok"
    };
    crate::LiveHealth {
        verdict: verdict.to_owned(),
        households: count(s.households.len()),
        players: count(players),
        offline: count(offline),
        subscriptions: count(s.subscriptions),
        surveyed_secs_ago: s.surveyed_at.map(|at| at.elapsed().as_secs()),
    }
}

const LIVE: CheckId = CheckId("daemon.live");

/// The live model's health: surveys, event subscriptions, offline players.
pub struct LiveCheck {
    pub snapshot: Snapshot,
}

impl Check for LiveCheck {
    fn id(&self) -> CheckId {
        LIVE
    }

    fn title(&self) -> &'static str {
        "Live model"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        let s = &self.snapshot;
        let players: usize = s.households.iter().map(|h| h.players.len()).sum();
        let offline: Vec<String> = s
            .health
            .iter()
            .filter(|(_, h)| h.health == Health::Offline)
            .map(|(id, _)| id.0.clone())
            .collect();
        let evidence = json!({
            "players": players,
            "subscriptions": s.subscriptions,
            "callback": s.callback,
            "surveyed_secs_ago": s.surveyed_at.map(|at| at.elapsed().as_secs()),
            "offline": offline,
            "last_error": s.last_error,
        });
        let result = if s.surveyed_at.is_none() {
            CheckResult::warn(
                "no survey has succeeded yet",
                "Check discovery with `fsonos doctor` on the daemon host, or set FSONOS_SEEDS.",
            )
            .with_detail(s.last_error.clone().unwrap_or_default())
        } else if s.subscriptions == 0 && players > 0 {
            CheckResult::warn(
                "no event subscriptions: state changes are seen only on the next survey",
                "Allow inbound TCP on the events port (FSONOS_EVENTS_PORT; docs/DEPLOY.md).",
            )
            .with_detail(s.last_error.clone().unwrap_or_default())
        } else if !offline.is_empty() {
            CheckResult::warn(
                format!(
                    "{} player(s) offline: {}",
                    offline.len(),
                    offline.join(", ")
                ),
                "Power and network for those players; the daemon resubscribes when they return.",
            )
        } else {
            CheckResult::pass(format!(
                "{players} players, {} event subscriptions, events at {}",
                s.subscriptions,
                s.callback.as_deref().unwrap_or("(not listening)")
            ))
        };
        result.with_evidence(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::{Runner, Status};

    fn run(snapshot: Snapshot) -> CheckResult {
        let mut runner = Runner::new();
        runner.register(LiveCheck { snapshot });
        runner.run().unwrap().get(LIVE).cloned().unwrap()
    }

    #[test]
    fn health_is_blind_without_players_and_degraded_without_subscriptions() {
        // Before its first survey ends, a daemon is starting, not blind.
        let starting = health(&Snapshot::default());
        assert_eq!(
            (starting.verdict.as_str(), starting.surveyed_secs_ago),
            ("starting", None)
        );
        let blind = health(&Snapshot {
            last_error: Some("SSDP: no replies; no seeds answered".into()),
            ..Snapshot::default()
        });
        assert_eq!((blind.verdict.as_str(), blind.players), ("blind", 0));
        let mut house = HouseholdState::default();
        house.players.push(fsonos_types::Player {
            id: fsonos_types::PlayerId("RINCON_KIT".into()),
            room_name: "Kitchen".into(),
            ip: "192.0.2.10".parse().unwrap(),
            model: String::new(),
            generation: fsonos_types::Generation::S2,
        });
        let unsubscribed = Snapshot {
            households: vec![house],
            surveyed_at: Some(Instant::now()),
            ..Snapshot::default()
        };
        let seen = health(&unsubscribed);
        assert_eq!(
            (seen.verdict.as_str(), seen.households, seen.players),
            ("degraded", 1, 1)
        );
        assert_eq!(seen.surveyed_secs_ago, Some(0));
        let subscribed = Snapshot {
            subscriptions: 3,
            ..unsubscribed
        };
        assert_eq!(health(&subscribed).verdict, "ok");
        // Counts only: nothing a caller could locate the house by.
        let text = serde_json::to_string(&health(&subscribed)).unwrap();
        assert!(
            !text.contains("192.0.2") && !text.contains("Kitchen"),
            "{text}"
        );
    }

    #[test]
    fn the_check_warns_until_a_survey_works() {
        let waiting = run(Snapshot {
            last_error: Some("SSDP: no replies".into()),
            ..Snapshot::default()
        });
        assert_eq!(waiting.status, Status::Warn);
        assert!(waiting.remedy.unwrap().contains("FSONOS_SEEDS"));
        let found_nothing = run(Snapshot {
            surveyed_at: Some(Instant::now()),
            callback: Some("http://192.0.2.1:8097".into()),
            ..Snapshot::default()
        });
        assert_eq!(found_nothing.status, Status::Pass);
    }

    #[test]
    fn an_unknown_room_that_vanished_is_unreachable() {
        let snapshot = Snapshot {
            vanished: vec![fsonos_proto::topology::VanishedDevice {
                uuid: fsonos_types::PlayerId("RINCON_BED".into()),
                zone_name: "Bedroom".into(),
                reason: Some("powered off".into()),
                model: None,
                last_known_ip: Some("192.0.2.14".parse().unwrap()),
            }],
            ..Snapshot::default()
        };
        let unknown = |room: &str| {
            Failure::from(fsonos_core::CoreError::UnknownRoom {
                name: room.into(),
                known: vec!["Kitchen@S1".into()],
            })
        };
        let off = vanished(unknown("bedroom"), &snapshot);
        assert_eq!(off.code, ErrorCode::PlayerUnreachable);
        assert!(
            off.detail.contains("Bedroom is powered off"),
            "{}",
            off.detail
        );
        assert!(off.detail.contains("192.0.2.14"), "{}", off.detail);
        assert!(off.hint.contains("power and network"));
        // A typo stays a typo, with its suggestions.
        let typo = vanished(unknown("Kitchn"), &snapshot);
        assert_eq!(typo.code, ErrorCode::UnknownRoom);
        assert_eq!(typo.suggestions, ["Kitchen@S1"]);
        // Other failures pass through.
        let other = Failure::new(ErrorCode::NotReady, "x");
        assert_eq!(vanished(other.clone(), &snapshot), other);
    }

    #[test]
    fn a_player_with_nothing_reported_has_no_track() {
        assert_eq!(track(&PlayerPlayback::default(), Instant::now()), None);
    }
}
