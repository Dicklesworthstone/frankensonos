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
        Some(self.duration_secs.map_or(advanced, |d| advanced.min(d)))
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
    /// observed at `now`. Returns what changed.
    pub fn apply(
        &mut self,
        player: &PlayerId,
        source: EventSource,
        notify: &Notify,
        now: Instant,
    ) -> Result<Changes, ProtoError> {
        let state = self.players.entry(player.clone()).or_default();
        let mut changes = Changes::default();
        match source {
            EventSource::AvTransport => {
                if let Some(lc) = notify.last_change()? {
                    apply_av_transport(state, &lc, now, &mut changes)?;
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
        Ok(changes)
    }

    /// Record a `GetPositionInfo` read of `player` made at `now`.
    pub fn apply_position(&mut self, player: &PlayerId, info: &PositionInfo, now: Instant) {
        let state = self.players.entry(player.clone()).or_default();
        if let Some(secs) = info.position_secs {
            state.position = Some((secs, now));
        }
        if info.duration_secs.is_some() {
            state.duration_secs = info.duration_secs;
        }
        if info.track > 0 {
            state.queue_position = Some(info.track);
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
        state.track_uri = to;
        // A new track starts from the top; its duration comes with it.
        state.position = Some((0, now));
        state.duration_secs = None;
    }
    if let Some(d) = lc.current_track_duration_secs() {
        state.duration_secs = Some(d);
    }
    if let Some(n) = position {
        state.queue_position = Some(n);
    }
    if let Some(n) = lc.get("NumberOfTracks").and_then(|v| v.trim().parse().ok()) {
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
