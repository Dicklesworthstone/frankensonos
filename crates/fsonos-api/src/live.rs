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
use std::time::Instant;

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
    fn a_player_with_nothing_reported_has_no_track() {
        assert_eq!(track(&PlayerPlayback::default(), Instant::now()), None);
    }
}
