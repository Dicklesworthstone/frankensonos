//! Reconcile listening history from the live model, including after lost events.
//! A queued event is a wake-up to inspect current state, not a past play to replay.
//! Startup and lost-event recovery visit each current coordinator. Queue slots
//! distinguish repeated tracks; pause/resume and metadata updates keep one play.

use super::{JUST_RECORDED, Surface, played_uri};
use fsonos_core::live::{LiveEvent, Snapshot};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::reconcile::Health;
use fsonos_core::store::{PlayName, PlayRecord};
use fsonos_types::{PlayerId, TransportState};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, PoisonError, Weak};
use std::time::{Duration, Instant};

const MOVED_WITHIN: Duration = Duration::from_secs(120);
// Cached-state recovery only: no speaker reads. An unrecorded play can retry
// after a temporary store failure even if no further notification arrives.
const RETRY: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Occurrence {
    uri: String,
    position: Option<u32>,
    members: Vec<PlayerId>,
}

impl Occurrence {
    fn same(&self, other: &Self) -> bool {
        self.uri == other.uri
            && (self.position.is_none()
                || other.position.is_none()
                || self.position == other.position)
    }
}

/// The current observation and the history row already accounted for in it.
/// Keeping the latter across stops avoids mistaking an earlier repeat for a
/// command or DJ write of the new occurrence.
#[derive(Default)]
pub(super) struct Heard {
    now: HashMap<PlayerId, Option<Occurrence>>,
    recorded: HashMap<PlayerId, Option<PlayRecord>>,
    ended: Vec<(Occurrence, Instant)>,
    coordinators: HashSet<PlayerId>,
    acquired: HashSet<PlayerId>,
}

impl Heard {
    fn remember(&mut self, player: &PlayerId, next: Option<Occurrence>) {
        if let Some(Some(was)) = self.now.insert(player.clone(), next) {
            self.ended.push((was, Instant::now()));
        }
        self.ended.retain(|(.., at)| at.elapsed() < MOVED_WITHIN);
    }

    fn handed_over(&self, player: &PlayerId, next: &Occurrence) -> bool {
        if !self.acquired.contains(player) {
            return false;
        }
        let continues = |before: &Occurrence| {
            before.uri == next.uri && before.members.iter().any(|id| next.members.contains(id))
        };
        self.now
            .iter()
            .any(|(p, current)| p != player && current.as_ref().is_some_and(continues))
            || self
                .ended
                .iter()
                .any(|(before, at)| at.elapsed() < MOVED_WITHIN && continues(before))
    }
}

pub(super) fn follow(surface: &Arc<Surface>) {
    let (Some(live), true) = (surface.live(), surface.log.is_some()) else {
        return;
    };
    // Subscribe before the startup snapshot so a transition during startup
    // cannot fall between the initial read and event registration.
    let changes = live.subscribe();
    let surface = Arc::downgrade(surface);
    if let Err(error) = std::thread::Builder::new()
        .name("fsonos-heard".into())
        .spawn(move || drive(&changes, RETRY, |event| handle(&surface, event.as_ref())))
    {
        tracing::error!(%error, "could not start the history follower");
    }
}

fn drive<T>(events: &Receiver<T>, interval: Duration, mut handle: impl FnMut(Option<T>) -> bool) {
    if !handle(None) {
        return;
    }
    let mut due = Instant::now() + interval;
    loop {
        match events.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                if !handle(Some(event)) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if Instant::now() >= due {
            if !handle(None) {
                return;
            }
            due = Instant::now() + interval;
        }
    }
}

fn handle(surface: &Weak<Surface>, event: Option<&LiveEvent>) -> bool {
    let Some(surface) = surface.upgrade() else {
        return false;
    };
    let Some(live) = surface.live() else {
        return false;
    };
    reconcile(&surface, &live.snapshot(), event);
    true
}

fn reconcile(surface: &Surface, snapshot: &Snapshot, event: Option<&LiveEvent>) {
    let coordinators: HashSet<_> = snapshot
        .households
        .iter()
        .flat_map(|house| {
            house
                .groups
                .iter()
                .filter(|g| house.player(&g.coordinator).is_some())
                .map(|g| g.coordinator.clone())
        })
        .collect();
    let mut heard = surface.heard.lock().unwrap_or_else(PoisonError::into_inner);
    let acquired: Vec<_> = coordinators
        .difference(&heard.coordinators)
        .cloned()
        .collect();
    heard.acquired.retain(|id| coordinators.contains(id));
    heard.acquired.extend(acquired);
    heard.coordinators.clone_from(&coordinators);
    // A former coordinator may retain stale playback. End it before the new
    // coordinator is observed, retaining the short handover deduplication.
    let retired: Vec<_> = heard
        .now
        .iter()
        .filter(|(id, current)| current.is_some() && !coordinators.contains(*id))
        .map(|(id, _)| id.clone())
        .collect();
    for player in retired {
        heard.remember(&player, None);
    }
    let only = match event {
        Some(LiveEvent::Playback { player, .. })
        | Some(LiveEvent::Health {
            player,
            health: Health::Healthy,
        }) => Some(player),
        Some(LiveEvent::Health { .. }) => return,
        Some(LiveEvent::Topology | LiveEvent::Resync) | None => None,
    };
    // Keep the household/group order stable when multiple tracks are observed
    // at once. Neither a lagging delta nor an old group member is authoritative.
    for group in snapshot.households.iter().flat_map(|h| &h.groups) {
        let player = &group.coordinator;
        if !coordinators.contains(player)
            || only.is_some_and(|id| id != player)
            || snapshot
                .health
                .of(player)
                .is_some_and(|h| h.health == Health::Offline)
        {
            continue;
        }
        if let Some(playback) = snapshot.playback.of(player) {
            record(surface, &mut heard, player, playback, &group.members);
        }
    }
}

fn record(
    surface: &Surface,
    heard: &mut Heard,
    player: &PlayerId,
    playback: &PlayerPlayback,
    members: &[PlayerId],
) {
    let raw = playback.track_uri.as_deref().filter(|u| !u.is_empty());
    let no_music = |u: &str| u.starts_with("x-rincon:") || u.starts_with("x-sonos-htastream:");
    if playback.transport == Some(TransportState::Stopped) || raw.is_some_and(no_music) {
        let was_playing = heard.now.get(player).is_some_and(Option::is_some);
        heard.remember(player, None);
        if was_playing {
            heard.acquired.remove(player);
        }
        // A newly elected coordinator can still have the cached STOPPED or
        // x-rincon state of its former member role. Wait for its first music
        // observation before consuming the bounded handover evidence.
        return;
    }
    let Some(raw) = raw.filter(|_| playback.transport == Some(TransportState::Playing)) else {
        return;
    };
    // Connect names its session between tracks; it does not end the track.
    if raw.starts_with("x-sonos-vli:") && raw.contains(",spotify:") {
        return;
    }
    let occurrence = Occurrence {
        uri: played_uri(raw),
        position: playback.queue_position.filter(|n| *n > 0),
        members: members.to_vec(),
    };
    if let Some(Some(before)) = heard.now.get_mut(player)
        && before.same(&occurrence)
    {
        // Learning an initially unknown queue slot is not another listen.
        before.position = occurrence.position.or(before.position);
        if before.members != occurrence.members {
            // A split can publish the old coordinator's smaller group before
            // the new one's playback arrives. Keep the prior membership only
            // for the bounded handover window, never as a global URI match.
            heard.ended.push((before.clone(), Instant::now()));
            heard.ended.retain(|(.., at)| at.elapsed() < MOVED_WITHIN);
        }
        before.members.clone_from(&occurrence.members);
        return;
    }
    let first = !heard.now.contains_key(player);
    let moved = heard.handed_over(player, &occurrence);
    let name = playback
        .now_playing
        .as_ref()
        .filter(|n| !n.title.trim().is_empty() && n.title != raw && n.title != occurrence.uri)
        .map(|n| PlayName {
            source_uri: occurrence.uri.clone(),
            title: n.title.trim().to_string(),
            artist: n.creator.clone().filter(|c| !c.trim().is_empty()),
        });
    let at = surface.now();
    let recorded = surface.with_store(|store| {
        if let Some(name) = &name {
            store.name_play(name, at)?;
        }
        let last = store.recent_plays(Some(&player.0), 1)?.pop();
        let same = last
            .as_ref()
            .is_some_and(|p| played_uri(&p.source_uri) == occurrence.uri);
        let already = same
            && (first
                || (last.as_ref() != heard.recorded.get(player).and_then(Option::as_ref)
                    && last
                        .as_ref()
                        .is_some_and(|p| at.abs_diff(p.played_at) < JUST_RECORDED.unsigned_abs())));
        if moved || already {
            return Ok(last);
        }
        store.record_play(&player.0, &occurrence.uri, at)?;
        Ok(Some(PlayRecord {
            zone: player.0.clone(),
            source_uri: occurrence.uri.clone(),
            played_at: at,
        }))
    });
    match recorded {
        Ok(Some(last)) => {
            // `last` can be None after a handover to a coordinator with no
            // history of its own. The successful observation still counts.
            heard.remember(player, Some(occurrence));
            heard.recorded.insert(player.clone(), last);
            heard.acquired.remove(player);
        }
        Ok(None) => {}
        Err(failure) => {
            // Do not consume this occurrence on an error. A later event,
            // resync or cached-state retry can still persist it.
            tracing::warn!(uri = %occurrence.uri, detail = %failure.detail, "a play heard was not recorded");
        }
    }
}

#[cfg(test)]
mod tests;
