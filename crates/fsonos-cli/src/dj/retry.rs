//! Bound automatic refill retries without delaying real playback changes.

use fsonos_core::playback::PlayerPlayback;
use fsonos_types::TransportState;
use std::time::{Duration, Instant};

const FIRST: Duration = Duration::from_secs(30);
const MAX: Duration = Duration::from_secs(300);

/// Volume, mute, artwork and the interpolated clock do not change what
/// must be queued. A new track, transport transition or queue edit does.
#[derive(Debug, PartialEq, Eq)]
struct PlaybackKey {
    transport: Option<TransportState>,
    uri: Option<String>,
    position: Option<u32>,
    length: Option<u32>,
}

impl From<&PlayerPlayback> for PlaybackKey {
    fn from(playback: &PlayerPlayback) -> Self {
        Self {
            transport: playback.transport,
            uri: playback.track_uri.clone(),
            position: playback.queue_position,
            length: playback.queue_length,
        }
    }
}

/// Transient per-coordinator state. A restart retries immediately rather
/// than persisting a monotonic deadline from a different process.
#[derive(Debug)]
pub(super) struct Retry {
    playback: PlaybackKey,
    delay: Duration,
    due: Instant,
}

impl Retry {
    pub(super) fn failed(previous: Option<&Self>, playback: &PlayerPlayback, now: Instant) -> Self {
        let playback = PlaybackKey::from(playback);
        let delay = previous
            .filter(|previous| previous.playback == playback)
            .map_or(FIRST, |previous| previous.delay.saturating_mul(2).min(MAX));
        Self {
            playback,
            delay,
            due: now + delay,
        }
    }

    pub(super) fn ready(&self, playback: &PlayerPlayback, now: Instant) -> bool {
        now >= self.due || self.playback != PlaybackKey::from(playback)
    }

    pub(super) fn delay(&self) -> Duration {
        self.delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dj::SpotifyDj;
    use fsonos_api::dj::{DjEngine, DjSpeakers, DjSteer};
    use fsonos_api::plan::DjAction;
    use fsonos_core::HouseholdState;
    use fsonos_core::clock::SystemClock;
    use fsonos_core::store::MemStore;
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_spotify::dj::DjConfig;
    use fsonos_spotify::feed::{FeedSnapshot, QueueFeed};
    use fsonos_types::PlayerId;

    fn playback() -> PlayerPlayback {
        let mut playback = PlayerPlayback::default();
        playback.transport = Some(TransportState::Playing);
        playback.track_uri = Some("test:track".into());
        playback.queue_position = Some(1);
        playback.queue_length = Some(2);
        playback
    }

    #[test]
    fn volume_and_mute_events_cannot_bypass_the_retry_deadline() {
        let now = Instant::now();
        let mut playback = playback();
        let retry = Retry::failed(None, &playback, now);
        for second in 0..30 {
            playback.volume = Some(u8::try_from(second).unwrap());
            playback.mute = Some(second % 2 == 0);
            playback.group_volume = playback.volume;
            assert!(!retry.ready(&playback, now + Duration::from_secs(second)));
        }
        assert!(retry.ready(&playback, now + FIRST));
    }

    #[test]
    fn repeated_failure_backs_off_to_a_bounded_five_minutes() {
        let mut now = Instant::now();
        let playback = playback();
        let mut retry = None;
        for seconds in [30, 60, 120, 240, 300, 300, 300] {
            let next = Retry::failed(retry.as_ref(), &playback, now);
            assert_eq!(next.delay(), Duration::from_secs(seconds));
            assert!(!next.ready(&playback, now));
            now += next.delay();
            assert!(next.ready(&playback, now));
            retry = Some(next);
        }
    }

    #[test]
    fn track_transport_and_queue_changes_are_processed_immediately() {
        let now = Instant::now();
        let original = playback();
        let retry = Retry::failed(None, &original, now);
        for change in 0..4 {
            let mut changed = original.clone();
            match change {
                0 => changed.track_uri = Some("test:next-track".into()),
                1 => changed.transport = Some(TransportState::Paused),
                2 => changed.queue_position = Some(2),
                _ => changed.queue_length = Some(3),
            }
            assert!(retry.ready(&changed, now));
            let failed = Retry::failed(Some(&retry), &changed, now);
            assert_eq!(
                failed.delay(),
                FIRST,
                "new state starts a new retry series"
            );
        }
    }

    fn household() -> (SimHandle, Vec<HouseholdState>, PlayerId, SpotifyDj) {
        let sim = SimHousehold::builder()
            .s2([SimPlayerSpec::new("Den", SimModel::One)])
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let player = sim.player("Den").unwrap();
        let id = PlayerId(player.uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, player.ip).unwrap());
        let feed = QueueFeed::restore(
            FeedSnapshot {
                zone: id.0.clone(),
                earlier: Vec::new(),
                lookahead: 1,
                queued: Vec::new(),
                playing: None,
                recorded: None,
                played: Vec::new(),
            },
            DjConfig::default(),
            1,
        );
        let dj = SpotifyDj::new(None);
        {
            let mut state = dj.state();
            state.feeds.insert(id.clone(), feed);
            state.planned = true;
        }
        (sim, vec![house], id, dj)
    }

    #[test]
    fn failed_stop_keeps_the_feed_for_a_successful_retry() {
        let (sim, households, id, dj) = household();
        let lan = sim.lan();
        let at = DjSpeakers {
            transport: &lan,
            households: &households,
            coordinator: &id,
        };
        let mut store = MemStore::default();
        let before = dj.state().feeds.get(&id).unwrap().snapshot();
        sim.upnp_fault("Den", "Stop", 701).unwrap();
        let failed = dj
            .act(at, &mut store, DjAction::Stop, &SystemClock)
            .unwrap_err();
        assert_eq!(failed.upnp_code, Some(701));
        assert!(dj.feeds(&id));
        let after = dj.state().feeds.get(&id).unwrap().snapshot();
        assert_eq!(after, before);
        sim.clear_faults("Den").unwrap();
        let stopped = dj
            .act(at, &mut store, DjAction::Stop, &SystemClock)
            .unwrap();
        assert!(stopped.changed);
        assert!(!dj.feeds(&id));
        assert!(!dj.state().feeds.contains_key(&id));
    }

    #[test]
    fn daemon_refill_failure_retries_on_its_deadline_not_on_volume_churn() {
        let (sim, households, id, dj) = household();
        let lan = sim.lan();
        let at = DjSpeakers {
            transport: &lan,
            households: &households,
            coordinator: &id,
        };
        let mut store = MemStore::default();
        let mut playback = PlayerPlayback::default();
        dj.on_playback(at, &mut store, &playback, &SystemClock);
        let (due, calls) = {
            let state = dj.state();
            let retry = state
                .refill_retries
                .get(&id)
                .expect("empty pool needs a retry");
            assert_eq!(retry.delay(), FIRST);
            (retry.due, sim.soap_log().len())
        };
        for volume in 0..20 {
            playback.volume = Some(volume);
            dj.on_playback(at, &mut store, &playback, &SystemClock);
            assert_eq!(dj.state().refill_retries.get(&id).unwrap().due, due);
        }
        assert_eq!(
            sim.soap_log().len(),
            calls,
            "churn must not repeat network I/O"
        );
        // Advance only the retry deadline, not the wall clock or a speaker.
        {
            let mut state = dj.state();
            state.refill_retries.get_mut(&id).unwrap().due = Instant::now();
        }
        dj.on_playback(at, &mut store, &playback, &SystemClock);
        let delay = dj.state().refill_retries.get(&id).unwrap().delay();
        assert_eq!(delay, FIRST * 2);
        // An explicit steering request never waits behind automatic backoff.
        dj.steer(at, &mut store, &DjSteer::Clear, &SystemClock)
            .unwrap();
        assert!(!dj.state().refill_retries.contains_key(&id));
    }
}
