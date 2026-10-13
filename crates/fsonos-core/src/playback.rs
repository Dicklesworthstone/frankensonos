//! Live playback state, kept current from GENA events.
//!
//! Each player reports its own state: AVTransport (transport state, current
//! track, queue position; meaningful on a group coordinator), RenderingControl
//! (room volume and mute) and GroupRenderingControl (group volume and mute,
//! on coordinators). [`Playback::apply`] folds one NOTIFY into the state and
//! says what changed, which is what the DJ and the agent surfaces react to.
//! LastChange carries no play position, so the position comes from
//! `GetPositionInfo` ([`Playback::apply_position`]) and is interpolated while
//! playing. Nothing here does I/O.

use fsonos_proto::ProtoError;
use fsonos_proto::control::PositionInfo;
use fsonos_proto::didl::DidlObject;
use fsonos_proto::gena::{LastChange, Notify};
use fsonos_types::{PlayerId, TransportState};
use std::collections::HashMap;
use std::time::Instant;

/// Which service a NOTIFY came from (the subscription that delivered it
/// knows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    AvTransport,
    RenderingControl,
    GroupRenderingControl,
}

/// What is playing, as the player describes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NowPlaying {
    pub title: String,
    /// The artist credit (for classical recordings, usually the composer).
    pub creator: Option<String>,
    pub album: Option<String>,
    /// Album art, as the player serves it (often a path on the player).
    pub art_uri: Option<String>,
}

impl NowPlaying {
    fn from_didl(o: &DidlObject) -> Self {
        Self {
            title: o.title.clone(),
            creator: o.creator.clone(),
            album: o.album.clone(),
            art_uri: o.album_art_uri.clone(),
        }
    }
}

/// One player's state. Fields stay `None` until an event or read reports
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayerPlayback {
    pub transport: Option<TransportState>,
    pub track_uri: Option<String>,
    pub now_playing: Option<NowPlaying>,
    pub duration_secs: Option<u32>,
    /// Queue position of the current track (1-based).
    pub queue_position: Option<u32>,
    pub queue_length: Option<u32>,
    pub play_mode: Option<String>,
    pub volume: Option<u8>,
    pub mute: Option<bool>,
    pub group_volume: Option<u8>,
    pub group_mute: Option<bool>,
    /// The last known position and when it was observed.
    position: Option<(u32, Instant)>,
}

impl PlayerPlayback {
    /// The play position at `now`: the last observed position, advanced by
    /// the time since while playing, and never past the track's end.
    #[must_use]
    pub fn position_at(&self, now: Instant) -> Option<u32> {
        let (secs, at) = self.position?;
        let advanced = if self.transport == Some(TransportState::Playing) {
            let elapsed = now.saturating_duration_since(at).as_secs();
            secs.saturating_add(u32::try_from(elapsed).unwrap_or(u32::MAX))
        } else {
            secs
        };
        // A zero duration is "no fixed end" (a stream), not "clamp to zero",
        // matching the players' own convention.
        Some(
            self.duration_secs
                .filter(|&d| d > 0)
                .map_or(advanced, |d| advanced.min(d)),
        )
    }
}

/// What a [`Playback::apply`] changed, for whoever reacts to it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Changes {
    pub transport: Option<(Option<TransportState>, TransportState)>,
    /// The track URI moved on (a new track started, or the queue ended).
    pub track: Option<(Option<String>, Option<String>)>,
    pub volume: Option<u8>,
    pub mute: Option<bool>,
    pub group_volume: Option<u8>,
    /// The queue's length changed (tracks added or removed, perhaps from
    /// another app), even with the same track playing.
    pub queue_length: Option<u32>,
}

impl Changes {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Playback state for every player that has reported any.
#[derive(Debug, Clone, Default)]
pub struct Playback {
    players: HashMap<PlayerId, PlayerPlayback>,
}

impl Playback {
    #[must_use]
    pub fn of(&self, player: &PlayerId) -> Option<&PlayerPlayback> {
        self.players.get(player)
    }

    /// Fold one NOTIFY from `player`'s `source` subscription into the state,
    /// observed at `now`. Returns what changed. A malformed event leaves
    /// the previous state intact, including whether the player was known.
    pub fn apply(
        &mut self,
        player: &PlayerId,
        source: EventSource,
        notify: &Notify,
        now: Instant,
    ) -> Result<Changes, ProtoError> {
        // Parse and fold into a candidate so a late DIDL error cannot publish
        // half an event without the corresponding Changes.
        let mut state = self.players.get(player).cloned().unwrap_or_default();
        let mut changes = Changes::default();
        match source {
            EventSource::AvTransport => {
                if let Some(lc) = notify.last_change()? {
                    apply_av_transport(&mut state, &lc, now, &mut changes)?;
                }
            }
            EventSource::RenderingControl => {
                if let Some(lc) = notify.last_change()? {
                    if let Some(v) = lc.volume().filter(|v| state.volume != Some(*v)) {
                        state.volume = Some(v);
                        changes.volume = Some(v);
                    }
                    if let Some(m) = lc.mute().filter(|m| state.mute != Some(*m)) {
                        state.mute = Some(m);
                        changes.mute = Some(m);
                    }
                }
            }
            EventSource::GroupRenderingControl => {
                if let Some(v) = notify
                    .property("GroupVolume")
                    .and_then(|v| v.trim().parse::<u8>().ok())
                    .map(|v| v.min(100))
                    .filter(|v| state.group_volume != Some(*v))
                {
                    state.group_volume = Some(v);
                    changes.group_volume = Some(v);
                }
                if let Some(m) = notify.property("GroupMute") {
                    state.group_mute = Some(m.trim() == "1");
                }
            }
        }
        self.players.insert(player.clone(), state);
        Ok(changes)
    }

    /// Record a `GetPositionInfo` read of `player` made at `now`. Identity
    /// and timing belong to the same sample: a new URI or queue slot cannot
    /// inherit the old track's clock, duration or metadata. Same-track reads
    /// retain optional details the player omitted (GENA may know more).
    pub fn apply_position(&mut self, player: &PlayerId, info: &PositionInfo, now: Instant) {
        let state = self.players.entry(player.clone()).or_default();
        let uri = match info.uri.trim() {
            "" | "NOT_IMPLEMENTED" => None,
            uri => Some(uri.to_string()),
        };
        let slot = (info.track > 0).then_some(info.track);
        let previous_slot = state.queue_position.filter(|&n| n > 0);
        let changed = state.track_uri != uri || (previous_slot.is_some() && previous_slot != slot);
        if changed || uri.is_none() {
            state.position = None;
            state.duration_secs = None;
            state.now_playing = None;
        }
        state.track_uri = uri;
        state.queue_position = slot;
        if state.track_uri.is_none() {
            return;
        }
        if let Some(secs) = info.position_secs {
            state.position = Some((secs, now));
        }
        if info.duration_secs.is_some() {
            state.duration_secs = info.duration_secs;
        }
        if let Some(md) = &info.metadata {
            state.now_playing = Some(NowPlaying::from_didl(md));
        }
    }

    /// Forget `player` (it left the household or went offline).
    pub fn remove(&mut self, player: &PlayerId) {
        self.players.remove(player);
    }
}

fn apply_av_transport(
    state: &mut PlayerPlayback,
    lc: &LastChange,
    now: Instant,
    changes: &mut Changes,
) -> Result<(), ProtoError> {
    if let Some(t) = lc.transport_state()
        && state.transport != Some(t)
    {
        // Freeze the interpolated position at the moment playing stops, and
        // restart the clock when it resumes.
        if let Some(pos) = state.position_at(now) {
            state.position = Some((pos, now));
        }
        changes.transport = Some((state.transport, t));
        state.transport = Some(t);
    }
    // The track moved on when its URI changed or, for the same URI queued
    // twice in a row, when its queue position changed.
    let uri = lc
        .get("CurrentTrackURI")
        .map(|_| lc.current_track_uri().map(str::to_string));
    let position: Option<u32> = lc.get("CurrentTrack").and_then(|v| v.trim().parse().ok());
    let uri_moved = uri.as_ref().is_some_and(|u| *u != state.track_uri);
    let position_moved =
        state.queue_position.is_some() && position.is_some_and(|n| state.queue_position != Some(n));
    if uri_moved || position_moved {
        let to = uri.clone().unwrap_or_else(|| state.track_uri.clone());
        changes.track = Some((state.track_uri.clone(), to.clone()));
        // The first time this model hears about the current track is not a
        // transition: a position already read from GetPositionInfo (e.g. a
        // survey's ground-truth sweep after a restart) stays authoritative.
        // Only a change between two observed tracks starts over from zero.
        let first_observation = state.track_uri.is_none();
        state.track_uri = to;
        if !first_observation {
            state.position = state.track_uri.as_ref().map(|_| (0, now));
            state.now_playing = None;
            // A URI change can leave the queue for a stream. A supplied
            // CurrentTrack below replaces this; otherwise the slot is unknown.
            if uri_moved {
                state.queue_position = None;
            }
        }
        state.duration_secs = None;
    }
    if let Some(d) = lc.current_track_duration_secs() {
        state.duration_secs = Some(d);
    }
    if let Some(n) = position {
        state.queue_position = Some(n);
    }
    if let Some(n) = lc.get("NumberOfTracks").and_then(|v| v.trim().parse().ok()) {
        if state.queue_length != Some(n) {
            changes.queue_length = Some(n);
        }
        state.queue_length = Some(n);
    }
    if let Some(mode) = lc.get("CurrentPlayMode") {
        state.play_mode = Some(mode.to_string());
    }
    if lc.get("CurrentTrackMetaData").is_some() {
        state.now_playing = lc
            .current_track_metadata()?
            .as_ref()
            .map(NowPlaying::from_didl);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn player() -> PlayerId {
        PlayerId("test-player".into())
    }

    fn notify(body: &str) -> Notify {
        Notify {
            sid: "uuid:test-playback".into(),
            seq: 0,
            path: "/AVTransport".into(),
            properties: vec![(
                "LastChange".into(),
                format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
            )],
        }
    }

    fn playing(now: Instant) -> Playback {
        let mut playback = Playback::default();
        playback.players.insert(
            player(),
            PlayerPlayback {
                transport: Some(TransportState::Playing),
                track_uri: Some("test:old-track".into()),
                now_playing: Some(NowPlaying {
                    title: "Old title".into(),
                    art_uri: Some("/old-art".into()),
                    ..NowPlaying::default()
                }),
                duration_secs: Some(200),
                queue_position: Some(1),
                queue_length: Some(10),
                play_mode: Some("NORMAL".into()),
                volume: Some(17),
                position: Some((30, now)),
                ..PlayerPlayback::default()
            },
        );
        playback
    }

    #[test]
    fn malformed_initial_event_does_not_introduce_a_player() {
        let mut playback = Playback::default();
        for body in [
            "<TransportState val=\"PLAYING\"/><CurrentTrackMetaData val=\"not-xml\"/>",
            "<TransportState",
        ] {
            assert!(
                playback
                    .apply(
                        &player(),
                        EventSource::AvTransport,
                        &notify(body),
                        Instant::now(),
                    )
                    .is_err()
            );
            assert!(playback.of(&player()).is_none());
        }
    }

    #[test]
    fn malformed_metadata_rolls_back_the_entire_event() {
        let now = Instant::now();
        let mut playback = playing(now);
        let before = playback.of(&player()).unwrap().clone();
        let body = "<TransportState val=\"PAUSED_PLAYBACK\"/>\
                    <CurrentTrackURI val=\"test:new-track\"/><CurrentTrack val=\"2\"/>\
                    <CurrentTrackDuration val=\"0:04:00\"/><NumberOfTracks val=\"22\"/>\
                    <CurrentPlayMode val=\"SHUFFLE\"/>\
                    <CurrentTrackMetaData val=\"not-xml\"/>";
        let at = now + Duration::from_secs(10);
        assert!(
            playback
                .apply(&player(), EventSource::AvTransport, &notify(body), at)
                .is_err()
        );
        assert_eq!(playback.of(&player()), Some(&before));
        // A valid replacement must still report every change: none was
        // silently applied by the rejected event.
        let valid = body.replace("not-xml", "");
        let changes = playback
            .apply(&player(), EventSource::AvTransport, &notify(&valid), at)
            .unwrap();
        assert_eq!(
            changes.transport,
            Some((Some(TransportState::Playing), TransportState::Paused))
        );
        assert_eq!(
            changes.track,
            Some((Some("test:old-track".into()), Some("test:new-track".into())))
        );
        assert_eq!(changes.queue_length, Some(22));
    }

    #[test]
    fn a_new_uri_without_metadata_cannot_keep_the_old_tracks_details() {
        let now = Instant::now();
        let mut playback = playing(now);
        let at = now + Duration::from_secs(10);
        let changes = playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify("<CurrentTrackURI val=\"test:new-track\"/>"),
                at,
            )
            .unwrap();
        assert!(changes.track.is_some());
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.track_uri.as_deref(), Some("test:new-track"));
        assert_eq!(state.now_playing, None);
        assert_eq!(state.duration_secs, None);
        assert_eq!(state.queue_position, None);
        assert_eq!(state.position_at(at + Duration::from_secs(2)), Some(2));
        assert_eq!(state.volume, Some(17));
        assert_eq!(state.queue_length, Some(10));
    }

    #[test]
    fn a_transport_only_delta_keeps_the_current_tracks_details() {
        let now = Instant::now();
        let mut playback = playing(now);
        let before = playback.of(&player()).unwrap().clone();
        let changes = playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify("<TransportState val=\"PAUSED_PLAYBACK\"/>"),
                now + Duration::from_secs(10),
            )
            .unwrap();
        let state = playback.of(&player()).unwrap();
        assert_eq!(changes.track, None);
        assert_eq!(state.now_playing, before.now_playing);
        assert_eq!(state.duration_secs, before.duration_secs);
        assert_eq!(state.queue_position, before.queue_position);
        assert_eq!(state.position_at(now + Duration::from_secs(50)), Some(40));
    }

    #[test]
    fn clearing_the_track_also_clears_its_clock_and_metadata() {
        let now = Instant::now();
        let mut playback = playing(now);
        let changes = playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify("<CurrentTrackURI val=\"\"/><TransportState val=\"STOPPED\"/>"),
                now + Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(changes.track, Some((Some("test:old-track".into()), None)));
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.now_playing, None);
        assert_eq!(state.position_at(now + Duration::from_secs(50)), None);
        assert_eq!(state.queue_position, None);
    }

    #[test]
    fn an_initial_event_preserves_the_restart_position_sample() {
        let now = Instant::now();
        let mut playback = Playback::default();
        playback.apply_position(
            &player(),
            &PositionInfo {
                track: 1,
                duration_secs: Some(200),
                position_secs: Some(90),
                uri: "test:old-track".into(),
                metadata: None,
            },
            now,
        );
        playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify(
                    "<TransportState val=\"PLAYING\"/><CurrentTrack val=\"1\"/>\
                     <CurrentTrackURI val=\"test:old-track\"/>\
                     <CurrentTrackDuration val=\"0:03:20\"/>",
                ),
                now,
            )
            .unwrap();
        assert_eq!(
            playback
                .of(&player())
                .unwrap()
                .position_at(now + Duration::from_secs(2)),
            Some(92)
        );
    }

    fn position(uri: &str, track: u32, secs: Option<u32>) -> PositionInfo {
        PositionInfo {
            track,
            duration_secs: None,
            position_secs: secs,
            uri: uri.into(),
            metadata: None,
        }
    }

    #[test]
    fn a_restart_read_supplies_identity_before_the_initial_event() {
        let now = Instant::now();
        let mut playback = Playback::default();
        let mut read = position("test:current", 4, Some(90));
        read.duration_secs = Some(240);
        playback.apply_position(&player(), &read, now);
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.track_uri.as_deref(), Some("test:current"));
        assert_eq!(state.queue_position, Some(4));
        let changes = playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify(
                    "<TransportState val=\"PLAYING\"/><CurrentTrack val=\"4\"/>\
                     <CurrentTrackURI val=\"test:current\"/>",
                ),
                now,
            )
            .unwrap();
        assert_eq!(
            changes.track, None,
            "the read already identified this track"
        );
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.duration_secs, Some(240));
        assert_eq!(state.position_at(now + Duration::from_secs(2)), Some(92));
    }

    #[test]
    fn a_position_read_replaces_the_old_identity_and_timing_together() {
        let now = Instant::now();
        let mut playback = playing(now);
        let at = now + Duration::from_secs(10);
        let mut read = position("test:new-track", 7, Some(12));
        read.duration_secs = Some(60);
        playback.apply_position(&player(), &read, at);
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.track_uri.as_deref(), Some("test:new-track"));
        assert_eq!(state.queue_position, Some(7));
        assert_eq!(state.duration_secs, Some(60));
        assert_eq!(state.position_at(at + Duration::from_secs(1)), Some(13));
        assert_eq!(state.now_playing, None);
        assert_eq!(state.volume, Some(17));
        assert_eq!(state.queue_length, Some(10));
        // A matching event is not a new transition and cannot reset the clock.
        let changes = playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify("<CurrentTrackURI val=\"test:new-track\"/><CurrentTrack val=\"7\"/>"),
                at + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(changes.track, None);
        assert_eq!(playback.of(&player()).unwrap().position_at(at), Some(12));
    }

    #[test]
    fn a_new_track_without_timing_does_not_inherit_the_previous_clock() {
        let now = Instant::now();
        for read in [
            position("test:new-track", 1, None),
            position("test:old-track", 2, None),
        ] {
            let mut playback = playing(now);
            playback.apply_position(&player(), &read, now + Duration::from_secs(10));
            let state = playback.of(&player()).unwrap();
            assert_eq!(state.track_uri.as_deref(), Some(read.uri.as_str()));
            assert_eq!(state.queue_position, Some(read.track));
            assert_eq!(state.position_at(now + Duration::from_secs(20)), None);
            assert_eq!(state.duration_secs, None);
            assert_eq!(state.now_playing, None);
        }
    }

    #[test]
    fn an_empty_position_read_clears_stale_media_even_with_zero_timing() {
        let now = Instant::now();
        for uri in ["", "   ", "NOT_IMPLEMENTED"] {
            let mut playback = playing(now);
            let mut read = position(uri, 0, Some(0));
            read.duration_secs = Some(0);
            playback.apply_position(&player(), &read, now);
            let state = playback.of(&player()).unwrap();
            assert_eq!(state.track_uri, None);
            assert_eq!(state.queue_position, None);
            assert_eq!(state.position_at(now + Duration::from_secs(20)), None);
            assert_eq!(state.duration_secs, None);
            assert_eq!(state.now_playing, None);
            assert_eq!(state.volume, Some(17));
        }
    }

    #[test]
    fn a_same_track_seek_preserves_optional_gena_details() {
        let now = Instant::now();
        let mut playback = playing(now);
        let metadata = playback.of(&player()).unwrap().now_playing.clone();
        let at = now + Duration::from_secs(10);
        playback.apply_position(&player(), &position("test:old-track", 1, Some(5)), at);
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.position_at(at + Duration::from_secs(2)), Some(7));
        assert_eq!(state.duration_secs, Some(200));
        assert_eq!(state.now_playing, metadata);
        // An omitted sample for the same track keeps interpolation running.
        playback.apply_position(&player(), &position("test:old-track", 1, None), at);
        assert_eq!(
            playback
                .of(&player())
                .unwrap()
                .position_at(at + Duration::from_secs(3)),
            Some(8)
        );
    }

    #[test]
    fn a_non_queue_read_clears_an_old_slot_and_retains_a_stream_clock() {
        let now = Instant::now();
        let mut playback = playing(now);
        let mut read = position("test:stream", 0, Some(20));
        read.duration_secs = Some(0);
        playback.apply_position(&player(), &read, now);
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.queue_position, None);
        assert_eq!(state.track_uri.as_deref(), Some("test:stream"));
        assert_eq!(state.now_playing, None);
        assert_eq!(state.position_at(now + Duration::from_secs(2)), Some(22));
        // GENA may report the explicit non-queue slot 0. A subsequent read
        // must not mistake that for another track transition.
        playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify("<CurrentTrack val=\"0\"/>"),
                now,
            )
            .unwrap();
        playback.apply_position(&player(), &position("test:stream", 0, None), now);
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.duration_secs, Some(0));
        assert_eq!(state.position_at(now + Duration::from_secs(2)), Some(22));
    }
}
