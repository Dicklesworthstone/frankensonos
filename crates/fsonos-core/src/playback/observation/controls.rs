//! Read-back of independent controls, using the same deltas as GENA.

use super::super::{Changes, Playback};
use fsonos_types::{PlayMode, PlayerId};

impl Playback {
    /// Apply a completed RenderingControl read. The caller collects both
    /// values before publishing; failed or cancelled reads leave both intact.
    pub(crate) fn apply_room_levels(
        &mut self,
        player: &PlayerId,
        volume: u8,
        mute: bool,
    ) -> Changes {
        let state = self.players.entry(player.clone()).or_default();
        let volume = volume.min(100);
        let changes = Changes {
            volume: (state.volume != Some(volume)).then_some(volume),
            mute: (state.mute != Some(mute)).then_some(mute),
            ..Changes::default()
        };
        state.volume = Some(volume);
        state.mute = Some(mute);
        changes
    }

    /// Only the current coordinator reports the whole group's volume.
    pub(crate) fn apply_group_volume(&mut self, player: &PlayerId, volume: u8) -> Changes {
        let state = self.players.entry(player.clone()).or_default();
        let volume = volume.min(100);
        let changes = Changes {
            group_volume: (state.group_volume != Some(volume)).then_some(volume),
            ..Changes::default()
        };
        state.group_volume = Some(volume);
        changes
    }

    /// A settings read never changes transport, track identity or its clock.
    pub(crate) fn apply_play_mode(&mut self, player: &PlayerId, mode: PlayMode) -> Changes {
        let state = self.players.entry(player.clone()).or_default();
        let mode = mode.as_str();
        let changed = state.play_mode.as_deref() != Some(mode);
        state.play_mode = Some(mode.to_string());
        Changes {
            play_mode: changed.then(|| mode.to_string()),
            ..Changes::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::EventSource;
    use fsonos_proto::gena::Notify;
    use std::time::{Duration, Instant};

    fn event(body: &str) -> Notify {
        Notify {
            sid: "uuid:test-controls".into(),
            seq: 0,
            path: "/AVTransport".into(),
            properties: vec![(
                "LastChange".into(),
                format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
            )],
        }
    }

    #[test]
    fn read_controls_and_gena_produce_the_same_state_and_changes() {
        let player = PlayerId("test-player".into());
        let now = Instant::now();
        let mut read = Playback::default();
        let mut pushed = Playback::default();
        for (volume, mute) in [(0, false), (37, true), (100, false), (100, false)] {
            let body = format!(
                "<Volume channel=\"Master\" val=\"{volume}\"/>\
                 <Mute channel=\"Master\" val=\"{}\"/>",
                u8::from(mute)
            );
            assert_eq!(
                read.apply_room_levels(&player, volume, mute),
                pushed
                    .apply(&player, EventSource::RenderingControl, &event(&body), now)
                    .unwrap()
            );
            assert_eq!(read.of(&player), pushed.of(&player));
        }
        for mode in PlayMode::ALL {
            let body = format!("<CurrentPlayMode val=\"{}\"/>", mode.as_str());
            assert_eq!(
                read.apply_play_mode(&player, mode),
                pushed
                    .apply(&player, EventSource::AvTransport, &event(&body), now)
                    .unwrap()
            );
            assert_eq!(read.of(&player), pushed.of(&player));
            assert!(read.apply_play_mode(&player, mode).is_empty());
        }
        for volume in [0, 43, 100, 100] {
            let mut notify = event("");
            notify.properties = vec![("GroupVolume".into(), volume.to_string())];
            assert_eq!(
                read.apply_group_volume(&player, volume),
                pushed
                    .apply(&player, EventSource::GroupRenderingControl, &notify, now)
                    .unwrap()
            );
            assert_eq!(read.of(&player), pushed.of(&player));
        }
    }

    #[test]
    fn repairing_controls_preserves_paused_identity_metadata_and_position() {
        use super::super::super::NowPlaying;
        use fsonos_proto::control::PositionInfo;
        use fsonos_types::TransportState;

        let player = PlayerId("test-player".into());
        let now = Instant::now();
        let mut playback = Playback::default();
        playback.apply_position(
            &player,
            &PositionInfo {
                track: 7,
                duration_secs: Some(300),
                position_secs: Some(123),
                uri: "test:recording".into(),
                metadata: None,
            },
            now,
        );
        let state = playback.players.get_mut(&player).unwrap();
        state.transport = Some(TransportState::Paused);
        state.queue_length = Some(12);
        state.now_playing = Some(NowPlaying {
            title: "Still this recording".into(),
            ..Default::default()
        });
        state.group_mute = Some(true);
        let before = state.clone();
        let room = playback.apply_room_levels(&player, 255, true);
        assert_eq!(room.volume, Some(100));
        assert_eq!(room.mute, Some(true));
        let group = playback.apply_group_volume(&player, 255);
        assert_eq!(group.group_volume, Some(100));
        playback.apply_play_mode(&player, PlayMode::from_wire("REPEAT_ALL").unwrap());
        let state = playback.of(&player).unwrap();
        let mut expected = before;
        expected.volume = Some(100);
        expected.mute = Some(true);
        expected.group_volume = Some(100);
        expected.play_mode = Some("REPEAT_ALL".into());
        assert_eq!(state, &expected);
        assert_eq!(state.position_at(now + Duration::from_secs(60)), Some(123));
    }
}
