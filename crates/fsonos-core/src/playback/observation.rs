//! Typed read-back observations use the same state and change vocabulary as GENA.

use super::{Changes, Playback, PlayerPlayback};
use fsonos_proto::control::{MediaInfo, PositionInfo, TransportInfo};
use fsonos_types::{PlayerId, TransportState};
use std::time::{Duration, Instant};

mod controls;

/// One completed read-back of a renderer. Collect every response before
/// applying it: a failed read must not publish half of a new playback state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackObservation {
    pub transport: TransportInfo,
    pub position: PositionInfo,
    pub media: MediaInfo,
}

impl Playback {
    /// Claim the pending clock request before starting a background read.
    /// A later, even otherwise identical, AVTransport event re-arms the flag
    /// and therefore differs from this baseline. The older read must not
    /// consume a seek which happened while it was in flight.
    pub(crate) fn begin_position_sample(&mut self, player: &PlayerId) -> Option<PlayerPlayback> {
        let state = self.players.get_mut(player)?;
        state.position_requested = false;
        Some(state.clone())
    }

    /// Retry a claimed request without inventing a player or a track. This
    /// changes no playback value and never sends a command to the renderer.
    pub(crate) fn request_position_sample(&mut self, player: &PlayerId) {
        if let Some(state) = self.players.get_mut(player) {
            state.position_requested = state.track_uri.is_some();
        }
    }

    /// Reconcile transport, track identity, queue length and position from
    /// one completed read-back. Emit the same changes as a NOTIFY, including
    /// a queue-slot transition between two copies of the same track URI.
    /// Room levels and play mode are not part of this read and remain intact.
    pub fn apply_observation(
        &mut self,
        player: &PlayerId,
        observation: &PlaybackObservation,
        now: Instant,
    ) -> Changes {
        let before = self.players.get(player).cloned().unwrap_or_default();
        self.apply_position(player, &observation.position, now);
        let state = self.players.entry(player.clone()).or_default();
        let moved = before.track_uri != state.track_uri
            || before.queue_position.filter(|&n| n > 0) != state.queue_position;
        let transport = observation.transport.state;
        let changed_transport = before.transport != Some(transport);
        if changed_transport && !moved && observation.position.position_secs.is_none() {
            // Freeze/resume an existing clock only for this same identity.
            // Never manufacture timing for a new or absent track.
            state.position = before.position_at(now).map(|position| (position, now));
        }
        state.transport = Some(transport);
        let tracks = observation.media.tracks;
        state.queue_length = Some(tracks);
        let metadata_changed =
            before.now_playing != state.now_playing || before.duration_secs != state.duration_secs;
        Changes {
            metadata_changed,
            transport: changed_transport.then_some((before.transport, transport)),
            track: moved.then(|| (before.track_uri, state.track_uri.clone())),
            queue_length: (before.queue_length != Some(tracks)).then_some(tracks),
            ..Changes::default()
        }
    }
}

impl PlayerPlayback {
    /// When a finite, currently playing track is expected to end according
    /// to its last position sample. A paused player, radio stream or unknown
    /// clock has no predicted end. This is a read-back deadline, never
    /// permission to start, skip or otherwise change playback.
    #[must_use]
    pub fn expected_end(&self) -> Option<Instant> {
        if self.transport != Some(TransportState::Playing) || self.track_uri.is_none() {
            return None;
        }
        let duration = self.duration_secs.filter(|&d| d > 0)?;
        let (position, observed) = self.position?;
        let remaining = u64::from(duration.saturating_sub(position));
        observed.checked_add(Duration::from_secs(remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(state: TransportState, track: u32, position: Option<u32>) -> PlaybackObservation {
        PlaybackObservation {
            transport: TransportInfo {
                state,
                status: "OK".into(),
            },
            position: PositionInfo {
                track,
                duration_secs: Some(180),
                position_secs: position,
                uri: "test:same-recording".into(),
                metadata: None,
            },
            media: MediaInfo {
                tracks: 4,
                uri: "test:queue".into(),
                uri_metadata: String::new(),
            },
        }
    }

    #[test]
    fn claiming_a_clock_preserves_display_and_an_identical_event_rearms_it() {
        use crate::playback::EventSource;
        use fsonos_proto::gena::Notify;

        let player = PlayerId("test-player".into());
        let now = Instant::now();
        let mut playback = Playback::default();
        playback.apply_observation(&player, &read(TransportState::Paused, 1, Some(23)), now);
        playback.request_position_sample(&player);
        let baseline = playback.begin_position_sample(&player).unwrap();
        assert!(!baseline.needs_position_sample());
        assert_eq!(baseline.position_at(now), Some(23));
        let notify = Notify {
            sid: "uuid:test".into(),
            seq: 1,
            path: "/AVTransport".into(),
            properties: vec![(
                "LastChange".into(),
                "<Event><InstanceID val=\"0\"><TransportState val=\"PAUSED_PLAYBACK\"/>\
                 </InstanceID></Event>"
                    .into(),
            )],
        };
        let changes = playback
            .apply(&player, EventSource::AvTransport, &notify, now)
            .unwrap();
        assert!(changes.is_empty());
        let after = playback.of(&player).unwrap();
        assert!(after.needs_position_sample());
        assert_ne!(after, &baseline);
        assert_eq!(after.position_at(now), Some(23));
    }

    #[test]
    fn retrying_a_sample_does_not_resurrect_a_removed_player() {
        let player = PlayerId("test-player".into());
        let mut playback = Playback::default();
        playback.request_position_sample(&player);
        assert!(playback.begin_position_sample(&player).is_none());
        assert!(playback.of(&player).is_none());
    }

    #[test]
    fn read_back_recovers_a_missed_pause_resume_and_stop() {
        let player = PlayerId("test-player".into());
        let start = Instant::now();
        let mut playback = Playback::default();
        let playing = read(TransportState::Playing, 1, Some(40));
        playback.apply_observation(&player, &playing, start);
        let at = start + Duration::from_secs(10);
        let paused = read(TransportState::Paused, 1, Some(43));
        let changed = playback.apply_observation(&player, &paused, at);
        assert_eq!(
            changed.transport,
            Some((Some(TransportState::Playing), TransportState::Paused))
        );
        let state = playback.of(&player).unwrap();
        assert_eq!(state.position_at(at + Duration::from_secs(50)), Some(43));
        let resumed = at + Duration::from_secs(50);
        let playing = read(TransportState::Playing, 1, None);
        playback.apply_observation(&player, &playing, resumed);
        let state = playback.of(&player).unwrap();
        assert_eq!(
            state.position_at(resumed + Duration::from_secs(2)),
            Some(45)
        );
        let stopped = read(TransportState::Stopped, 1, Some(46));
        let changed = playback.apply_observation(&player, &stopped, resumed);
        assert_eq!(
            changed.transport,
            Some((Some(TransportState::Playing), TransportState::Stopped))
        );
        assert_eq!(playback.of(&player).unwrap().expected_end(), None);
    }

    #[test]
    fn a_missed_duplicate_track_transition_is_published_once() {
        let player = PlayerId("test-player".into());
        let now = Instant::now();
        let mut playback = Playback::default();
        let first = read(TransportState::Playing, 1, Some(179));
        playback.apply_observation(&player, &first, now);
        let second = read(TransportState::Playing, 2, Some(3));
        let changed = playback.apply_observation(&player, &second, now);
        let uri = Some(second.position.uri.clone());
        assert_eq!(changed.track, Some((uri.clone(), uri)));
        assert_eq!(playback.of(&player).unwrap().queue_position, Some(2));
        assert_eq!(playback.of(&player).unwrap().position_at(now), Some(3));
        assert!(playback.apply_observation(&player, &second, now).is_empty());
    }

    #[test]
    fn a_new_identity_cannot_inherit_an_old_clock_or_metadata() {
        let player = PlayerId("test-player".into());
        let now = Instant::now();
        let mut playback = Playback::default();
        let first = read(TransportState::Playing, 1, Some(120));
        playback.apply_observation(&player, &first, now);
        let previous = playback.players.get_mut(&player).unwrap();
        previous.now_playing = Some(super::super::NowPlaying {
            title: "Old title".into(),
            ..Default::default()
        });
        previous.volume = Some(23);
        previous.mute = Some(true);
        previous.play_mode = Some("NORMAL".into());
        let mut next = read(TransportState::Paused, 2, None);
        next.position.duration_secs = None;
        next.media.tracks = 8;
        let changed = playback.apply_observation(&player, &next, now);
        let state = playback.of(&player).unwrap();
        assert_eq!(state.position_at(now), None);
        assert_eq!(state.duration_secs, None);
        assert_eq!(state.now_playing, None);
        assert_eq!(state.volume, Some(23));
        assert_eq!(state.mute, Some(true));
        assert_eq!(state.play_mode.as_deref(), Some("NORMAL"));
        assert_eq!(changed.queue_length, Some(8));
    }

    #[test]
    fn an_empty_renderer_read_clears_the_old_identity() {
        let player = PlayerId("test-player".into());
        let now = Instant::now();
        let mut playback = Playback::default();
        let first = read(TransportState::Playing, 1, Some(170));
        playback.apply_observation(&player, &first, now);
        let mut empty = read(TransportState::Stopped, 0, None);
        empty.position.uri.clear();
        empty.media.tracks = 0;
        let changed = playback.apply_observation(&player, &empty, now);
        assert_eq!(
            changed.track,
            Some((Some("test:same-recording".into()), None))
        );
        let state = playback.of(&player).unwrap();
        assert_eq!(state.position_at(now), None);
        assert_eq!(state.expected_end(), None);
        assert_eq!(state.queue_length, Some(0));
    }

    #[test]
    fn end_prediction_uses_the_sample_not_the_clamped_display_clock() {
        let player = PlayerId("test-player".into());
        let now = Instant::now();
        let mut playback = Playback::default();
        let playing = read(TransportState::Playing, 1, Some(175));
        playback.apply_observation(&player, &playing, now);
        let state = playback.of(&player).unwrap();
        assert_eq!(state.expected_end(), Some(now + Duration::from_secs(5)));
        assert_eq!(state.position_at(now + Duration::from_secs(100)), Some(180));
        assert_eq!(state.expected_end(), Some(now + Duration::from_secs(5)));
        for duration in [None, Some(0)] {
            let mut stream = read(TransportState::Playing, 0, Some(1));
            stream.position.duration_secs = duration;
            // A different identity must not retain the previous duration.
            stream.position.uri = "test:radio".into();
            let mut unknown = Playback::default();
            unknown.apply_observation(&player, &stream, now);
            assert_eq!(unknown.of(&player).unwrap().expected_end(), None);
        }
    }
}
