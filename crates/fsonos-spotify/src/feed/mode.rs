//! Queue-order control for whole-work starts and live mode changes.

use fsonos_core::{control, playback::PlayerPlayback};
use fsonos_proto::Transport;
use fsonos_proto::content::{QUEUE, browse_range_snapshot};
use fsonos_proto::control::{get_media_info, get_position_info, set_normal_play_mode};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;

use super::{FeedError, QueueFeed, QueuedWork, Speakers, host_of, placed};

/// Some players reject SetPlayMode while a stream, not the queue, is active.
/// The core starts the queue, sets NORMAL, seeks, then plays; any failed
/// prerequisite prevents the later stages. Ordinary queue starts are unchanged.
pub(super) fn start<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
    position: u32,
) -> Result<(), FeedError> {
    control::play_queue_from_in_order(at.transport, at.households, at.coordinator, position)?;
    Ok(())
}

impl QueueFeed {
    /// A retained work can be heard again. Preserve every movement of the
    /// current work and put its retained successors before newer picks.
    /// Call only after reconciling positions and confirming the observation.
    pub(super) fn restore_played_from(&mut self, position: u32) {
        if !self
            .played
            .iter()
            .any(|work| work.last().is_some_and(|last| last >= position))
        {
            return;
        }
        let (played, mut ahead): (Vec<QueuedWork>, Vec<QueuedWork>) =
            std::mem::take(&mut self.played)
                .into_iter()
                .partition(|work| work.last().is_none_or(|last| last < position));
        self.played = played;
        ahead.append(&mut self.queued);
        self.queued = ahead;
    }
}

/// Repair shuffle/repeat only on a still-owned active queue. A stale event
/// must not change a stream, a replaced queue, or another group's transport.
/// NORMAL and absent mode deltas add no reads to the ordinary feed path.
pub(super) fn maintain<T: Transport + ?Sized>(
    feed: &mut QueueFeed,
    at: &Speakers<'_, T>,
    playback: &PlayerPlayback,
) -> Result<bool, FeedError> {
    if !feed.is_active() {
        return Ok(false);
    }
    if !requires_order(playback.play_mode.as_deref()) {
        return Ok(true);
    }
    let host = host_of(at)?;
    let own_queue = format!("x-rincon-queue:{}#0", at.coordinator.0);
    if get_media_info(at.transport, host)?.uri != own_queue {
        // An announcement or other source can temporarily use this player.
        // Suspend this event, without stopping it or forgetting the DJ set.
        return Ok(false);
    }
    let before = browse_range_snapshot(at.transport, host, QUEUE, 0, 1)?;
    let had = !feed.queued.is_empty() || !feed.played.is_empty();
    // Do not publish a partial reconciliation if a later read fails or an
    // independent queue edit changes the evidence while these reads run.
    let mut reconciled = feed.clone();
    let queue = reconciled.resync(at)?;
    let current = get_position_info(at.transport, host)?;
    let topology = get_zone_group_state(at.transport, host)?;
    let after = browse_range_snapshot(at.transport, host, QUEUE, 0, 1)?;
    if before != after {
        feed.queue_len = None;
        return Ok(false);
    }
    if !topology
        .groups
        .iter()
        .any(|group| group.coordinator == *at.coordinator)
        || get_media_info(at.transport, host)?.uri != own_queue
    {
        feed.queue_len = None;
        return Ok(false);
    }
    let owned = reconciled.played.iter().chain(&reconciled.queued).any(|work| {
        work.movements
            .iter()
            .any(|movement| movement.added && movement.position.is_some())
    });
    if !owned {
        if had {
            feed.release();
        }
        return Ok(false);
    }
    if !event_matches(playback, current.track, &current.uri, &queue) {
        feed.queue_len = None;
        return Ok(false);
    }
    // A rewind can return to a retained work after every newer pick was
    // removed. Restore it before deciding that the DJ no longer owns a set.
    reconciled.restore_played_from(current.track);
    if reconciled.queued.is_empty() {
        feed.release();
        return Ok(false);
    }
    *feed = reconciled;
    // No source reset, seek, or Play is needed to restore order. A refused
    // write retains the feed and goes through its existing retry backoff.
    // These are read-before-write checks, not an atomic device transaction.
    let result = set_normal_play_mode(at.transport, host);
    if result.is_err() {
        feed.queue_len = None;
    }
    result?;
    Ok(true)
}

fn requires_order(mode: Option<&str>) -> bool {
    matches!(
        mode.map(str::trim),
        Some("SHUFFLE" | "SHUFFLE_NOREPEAT" | "SHUFFLE_REPEAT_ONE" | "REPEAT_ALL" | "REPEAT_ONE")
    )
}

fn event_matches(
    event: &PlayerPlayback,
    current_position: u32,
    current_uri: &str,
    queue: &[Option<String>],
) -> bool {
    let Some(position) = event.queue_position.filter(|&p| p > 0) else {
        return false;
    };
    let Some(uri) = event.track_uri.as_deref().filter(|uri| !uri.is_empty()) else {
        return false;
    };
    let source = spotify_uri_from_renderer_uri(uri);
    let same = match (&source, spotify_uri_from_renderer_uri(current_uri)) {
        (Some(expected), Some(current)) => *expected == current,
        _ => uri == current_uri,
    };
    same && current_position > 0
        && placed(queue, position, source.as_deref()) == Some(current_position)
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use fsonos_proto::ProtoError;
    use fsonos_types::PlayerId;
    use std::net::IpAddr;

    fn event(position: Option<u32>, uri: Option<&str>) -> PlayerPlayback {
        let mut event = PlayerPlayback::default();
        event.queue_position = position;
        event.track_uri = uri.map(str::to_owned);
        event
    }

    fn queue(ids: &[&str]) -> Vec<Option<String>> {
        ids.iter()
            .map(|id| Some(format!("spotify:track:{id}")))
            .collect()
    }

    const A: &str = "x-sonos-spotify:spotify%3atrack%3aTestA?sid=1";
    const B: &str = "x-sonos-spotify:spotify%3atrack%3aTestB?sid=1";

    #[test]
    fn only_known_shuffle_and_repeat_modes_request_a_repair() {
        for mode in [
            "SHUFFLE",
            "SHUFFLE_NOREPEAT",
            "SHUFFLE_REPEAT_ONE",
            "REPEAT_ALL",
            "REPEAT_ONE",
        ] {
            assert!(requires_order(Some(mode)));
        }
        assert!(requires_order(Some("  REPEAT_ALL  ")));
        for mode in [
            None,
            Some("NORMAL"),
            Some(""),
            Some("NOT_IMPLEMENTED"),
            Some("future_mode"),
        ] {
            assert!(!requires_order(mode));
        }
    }

    #[test]
    fn renderer_encoding_variants_identify_the_same_queue_track() {
        let current = "x-sonos-spotify:spotify%3Atrack%3ATestA?sid=2";
        let sample = event(Some(1), Some(A));
        let tracks = queue(&["TestA"]);
        assert!(event_matches(&sample, 1, current, &tracks));
        assert!(!event_matches(&sample, 1, B, &tracks));
    }

    #[test]
    fn buffered_positions_can_move_up_after_a_queue_trim() {
        let tracks = queue(&["TestA", "TestB"]);
        assert!(event_matches(&event(Some(4), Some(B)), 2, B, &tracks));
        assert!(!event_matches(&event(Some(1), Some(B)), 2, B, &tracks));
    }

    #[test]
    fn a_different_copy_of_the_same_uri_is_not_the_observed_track() {
        let tracks = queue(&["TestA", "TestB", "TestA"]);
        assert!(event_matches(&event(Some(3), Some(A)), 3, A, &tracks));
        assert!(!event_matches(&event(Some(3), Some(A)), 1, A, &tracks));
        assert!(!event_matches(&event(Some(1), Some(A)), 3, A, &tracks));
    }

    #[test]
    fn absent_identity_and_unrelated_non_spotify_sources_do_not_match() {
        let tracks = queue(&["TestA"]);
        for sample in [
            event(None, Some(A)),
            event(Some(0), Some(A)),
            event(Some(1), None),
        ] {
            assert!(!event_matches(&sample, 1, A, &tracks));
        }
        let source = "x-file-cifs://example.invalid/owner.flac";
        let other = "x-file-cifs://example.invalid/other.flac";
        let sample = event(Some(1), Some(source));
        assert!(event_matches(&sample, 1, source, &[None]));
        assert!(!event_matches(&sample, 1, other, &[None]));
    }

    struct NoIo;

    impl Transport for NoIo {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("ordinary mode events must not issue SOAP reads or writes")
        }
    }

    #[test]
    fn normal_unknown_and_inactive_paths_perform_no_io() {
        let id = PlayerId("TestCoordinator".into());
        let at = Speakers {
            transport: &NoIo,
            households: &[],
            coordinator: &id,
        };
        let mut feed = QueueFeed::new(&id, crate::dj::DjConfig::default(), 1);
        let mut playback = PlayerPlayback::default();
        playback.play_mode = Some("SHUFFLE".into());
        assert!(!maintain(&mut feed, &at, &playback).unwrap());
        feed.active = true;
        for mode in [None, Some("NORMAL"), Some("NOT_IMPLEMENTED")] {
            playback.play_mode = mode.map(str::to_owned);
            assert!(maintain(&mut feed, &at, &playback).unwrap());
        }
    }
}

#[cfg(test)]
mod ownership;
#[cfg(test)]
mod replay {
    //! Retained-work recovery uses the real S1/S2 simulator, not fabricated replies.

    use super::*;
    use crate::dj::{DjConfig, PickReason};
    use crate::feed::{FeedSnapshot, Movement};
    use fsonos_core::HouseholdState;
    use fsonos_proto::control as soap;
    use fsonos_proto::soap::{AV_TRANSPORT, args_xml, call};
    use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_types::PlayerId;
    use std::net::IpAddr;

    const ROOM: &str = "Replay Room";
    const TRACKS: [&str; 4] = [
        "spotify:track:ReplayTestA",
        "spotify:track:ReplayTestB",
        "spotify:track:ReplayTestC",
        "spotify:track:ReplayTestD",
    ];

    struct Rig {
        sim: SimHandle,
        houses: Vec<HouseholdState>,
        id: PlayerId,
        host: IpAddr,
    }

    impl Rig {
        fn new(generation: u8) -> Self {
            let builder = SimHousehold::builder();
            let builder = if generation == 1 {
                builder.s1([SimPlayerSpec::new(ROOM, SimModel::Play5Gen1)])
            } else {
                builder.s2([SimPlayerSpec::new(ROOM, SimModel::One)])
            };
            let sim = builder.spawn().unwrap();
            let player = sim.player(ROOM).unwrap();
            let id = PlayerId(player.uuid.clone());
            let host = player.ip;
            let lan = sim.lan();
            let mut house = HouseholdState::default();
            house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
            let houses = vec![house];
            let tracks: Vec<_> = TRACKS.iter().map(|uri| (*uri, "Test movement")).collect();
            control::queue_spotify_tracks(&lan, &houses, &id, &tracks).unwrap();
            control::play_queue_from(&lan, &houses, &id, 3).unwrap();
            Self {
                sim,
                houses,
                id,
                host,
            }
        }

        fn work(start: usize) -> QueuedWork {
            QueuedWork {
                title: format!("Test work {start}"),
                composer: "Test composer".into(),
                work_key: format!("replay-work-{start}"),
                movements: (start..start + 2)
                    .map(|index| Movement {
                        uri: TRACKS[index].into(),
                        title: format!("Test movement {index}"),
                        position: Some(u32::try_from(index + 1).unwrap()),
                        added: true,
                    })
                    .collect(),
                reason: PickReason {
                    factors: Vec::new(),
                    relaxed: Vec::new(),
                    summary: "Test selection".into(),
                },
            }
        }

        fn feed(&self) -> QueueFeed {
            QueueFeed::restore(
                FeedSnapshot {
                    zone: self.id.0.clone(),
                    earlier: Vec::new(),
                    lookahead: 1,
                    queued: vec![Self::work(2)],
                    played: vec![Self::work(0)],
                    playing: Some(3),
                    recorded: Some((3, TRACKS[2].into())),
                },
                DjConfig::default(),
                7,
            )
        }

        fn seek(&self, position: u32) {
            control::play_queue_from(&self.sim.lan(), &self.houses, &self.id, position).unwrap();
        }

        fn remove_newer(&self) {
            let lan = self.sim.lan();
            let revision = browse_range_snapshot(&lan, self.host, QUEUE, 0, 4)
                .unwrap()
                .update_id;
            soap::remove_track_range_from_queue(&lan, self.host, revision, 3, 2).unwrap();
        }

        fn mode(&self, mode: &str) {
            call(
                &self.sim.lan(),
                self.host,
                &AV_TRANSPORT,
                "SetPlayMode",
                &args_xml(&[("InstanceID", "0"), ("NewPlayMode", mode)]),
            )
            .unwrap();
        }

        fn observation(&self, mode: &str) -> PlayerPlayback {
            let position = soap::get_position_info(&self.sim.lan(), self.host).unwrap();
            let mut state = PlayerPlayback::default();
            state.track_uri = Some(position.uri);
            state.queue_position = Some(position.track);
            state.play_mode = Some(mode.into());
            state
        }

        fn repair(&self, feed: &mut QueueFeed, event: &PlayerPlayback) -> Result<bool, FeedError> {
            maintain(
                feed,
                &Speakers {
                    transport: &self.sim.lan(),
                    households: &self.houses,
                    coordinator: &self.id,
                },
                event,
            )
        }

        fn writes_since(&self, start: usize) -> Vec<String> {
            self.sim
                .soap_log()
                .into_iter()
                .skip(start)
                .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
                .map(|entry| entry.action)
                .collect()
        }
    }

    #[test]
    fn retained_work_recovers_order_after_all_newer_picks_were_removed() {
        for generation in [1, 2] {
            let rig = Rig::new(generation);
            rig.seek(2);
            rig.remove_newer();
            for mode in ["SHUFFLE", "REPEAT_ONE", "REPEAT_ALL"] {
                let mut feed = rig.feed();
                rig.mode(mode);
                let event = rig.observation(mode);
                let before = browse_range_snapshot(&rig.sim.lan(), rig.host, QUEUE, 0, 4).unwrap();
                let marker = rig.sim.soap_log().len();
                assert!(rig.repair(&mut feed, &event).unwrap());
                assert!(feed.is_active());
                assert_eq!(feed.queued(), &[Rig::work(0)]);
                assert!(feed.played.is_empty());
                assert_eq!(rig.writes_since(marker), ["SetPlayMode"]);
                assert_eq!(
                    soap::get_position_info(&rig.sim.lan(), rig.host)
                        .unwrap()
                        .track,
                    2
                );
                assert_eq!(
                    browse_range_snapshot(&rig.sim.lan(), rig.host, QUEUE, 0, 4).unwrap(),
                    before
                );
            }
        }
    }

    #[test]
    fn replayed_work_precedes_still_waiting_picks_without_duplicate_movements() {
        for generation in [1, 2] {
            let rig = Rig::new(generation);
            let mut feed = rig.feed();
            rig.seek(2);
            rig.mode("SHUFFLE");
            let event = rig.observation("SHUFFLE");
            assert!(rig.repair(&mut feed, &event).unwrap());
            assert_eq!(feed.queued(), &[Rig::work(0), Rig::work(2)]);
            assert!(feed.played.is_empty());
            assert!(rig.repair(&mut feed, &event).unwrap());
            assert_eq!(feed.queued(), &[Rig::work(0), Rig::work(2)]);
        }
    }

    #[test]
    fn a_stale_event_cannot_restore_or_release_a_retained_set() {
        for generation in [1, 2] {
            let rig = Rig::new(generation);
            let mut feed = rig.feed();
            rig.seek(1);
            rig.mode("SHUFFLE");
            let stale = rig.observation("SHUFFLE");
            rig.seek(2);
            rig.remove_newer();
            let saved = feed.snapshot();
            let marker = rig.sim.soap_log().len();
            assert!(!rig.repair(&mut feed, &stale).unwrap());
            assert_eq!(feed.snapshot(), saved);
            assert!(rig.writes_since(marker).is_empty());
            let current = rig.observation("SHUFFLE");
            assert!(rig.repair(&mut feed, &current).unwrap());
            assert_eq!(feed.queued(), &[Rig::work(0)]);
        }
    }

    #[test]
    fn a_failed_read_keeps_retained_ownership_and_recovery_can_retry() {
        for generation in [1, 2] {
            let rig = Rig::new(generation);
            let mut feed = rig.feed();
            rig.seek(2);
            rig.remove_newer();
            rig.mode("REPEAT_ONE");
            let event = rig.observation("REPEAT_ONE");
            let saved = feed.snapshot();
            rig.sim.upnp_fault(ROOM, "GetPositionInfo", 701).unwrap();
            let marker = rig.sim.soap_log().len();
            assert!(rig.repair(&mut feed, &event).is_err());
            assert_eq!(feed.snapshot(), saved);
            assert!(rig.writes_since(marker).is_empty());
            rig.sim.clear_faults(ROOM).unwrap();
            assert!(rig.repair(&mut feed, &event).unwrap());
            assert_eq!(feed.queued(), &[Rig::work(0)]);
        }
    }

    #[test]
    fn old_remnants_do_not_authorize_changing_the_owners_current_source() {
        for generation in [1, 2] {
            let rig = Rig::new(generation);
            let mut feed = rig.feed();
            rig.seek(2);
            rig.remove_newer();
            soap::add_uri_to_queue(
                &rig.sim.lan(),
                rig.host,
                "http://example.invalid/owners-track.wav",
                "",
                false,
            )
            .unwrap();
            rig.seek(3);
            rig.mode("SHUFFLE");
            let event = rig.observation("SHUFFLE");
            let marker = rig.sim.soap_log().len();
            assert!(!rig.repair(&mut feed, &event).unwrap());
            assert!(!feed.is_active());
            assert!(rig.writes_since(marker).is_empty());
        }
    }
}
