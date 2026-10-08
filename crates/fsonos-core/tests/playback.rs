//! `playback` folds the live-captured NOTIFY fixtures into per-player state
//! and reports what changed.

use fsonos_core::playback::{EventSource, Playback};
use fsonos_proto::control::PositionInfo;
use fsonos_proto::gena::{Notify, parse_propertyset};
use fsonos_types::{PlayerId, TransportState};
use std::time::{Duration, Instant};

const AVT_INITIAL: &str =
    include_str!("../../fsonos-proto/tests/fixtures/gena_notify_avt_initial_s1.xml");
const AVT_PAUSE: &str =
    include_str!("../../fsonos-proto/tests/fixtures/gena_notify_avt_pause_s1.xml");
const RCS_INITIAL: &str =
    include_str!("../../fsonos-proto/tests/fixtures/gena_notify_rcs_initial_s1.xml");
const RCS_VOLUME: &str =
    include_str!("../../fsonos-proto/tests/fixtures/gena_notify_rcs_volume_s1.xml");

fn notify(body: &str, seq: u32) -> Notify {
    Notify {
        sid: "uuid:RINCON_000E58A0000401400_sub1".into(),
        seq,
        path: "/".into(),
        properties: parse_propertyset(body).unwrap(),
    }
}

fn plain(props: &[(&str, &str)]) -> Notify {
    Notify {
        sid: "uuid:x".into(),
        seq: 0,
        path: "/".into(),
        properties: props
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect(),
    }
}

fn player() -> PlayerId {
    PlayerId("RINCON_000E58A0000401400".into())
}

#[test]
fn the_initial_transport_event_fills_in_the_track() {
    let mut p = Playback::default();
    let now = Instant::now();
    let changes = p
        .apply(
            &player(),
            EventSource::AvTransport,
            &notify(AVT_INITIAL, 0),
            now,
        )
        .unwrap();
    assert_eq!(changes.transport, Some((None, TransportState::Stopped)));
    let st = p.of(&player()).unwrap();
    assert!(
        st.track_uri
            .as_deref()
            .unwrap()
            .starts_with("x-sonos-spotify:")
    );
    assert_eq!(st.duration_secs, Some(163));
    assert_eq!((st.queue_position, st.queue_length), (Some(1), Some(1)));
    assert_eq!(st.play_mode.as_deref(), Some("NORMAL"));
    assert!(st.now_playing.as_ref().is_some_and(|n| !n.title.is_empty()));
    assert!(
        changes.track.is_some(),
        "the first event introduces the track"
    );
}

#[test]
fn a_later_event_reports_only_what_changed() {
    let mut p = Playback::default();
    let now = Instant::now();
    p.apply(
        &player(),
        EventSource::AvTransport,
        &notify(AVT_INITIAL, 0),
        now,
    )
    .unwrap();
    let changes = p
        .apply(
            &player(),
            EventSource::AvTransport,
            &notify(AVT_PAUSE, 1),
            now,
        )
        .unwrap();
    assert_eq!(
        changes.transport,
        Some((Some(TransportState::Stopped), TransportState::Transitioning))
    );
    assert_eq!(changes.track, None, "same track");
}

#[test]
fn rendering_control_tracks_master_volume_and_mute() {
    let mut p = Playback::default();
    let now = Instant::now();
    let first = p
        .apply(
            &player(),
            EventSource::RenderingControl,
            &notify(RCS_INITIAL, 0),
            now,
        )
        .unwrap();
    assert_eq!((first.volume, first.mute), (Some(18), Some(false)));
    let second = p
        .apply(
            &player(),
            EventSource::RenderingControl,
            &notify(RCS_VOLUME, 1),
            now,
        )
        .unwrap();
    assert_eq!(second.volume, Some(19));
    assert_eq!(second.mute, None, "mute did not change");
    assert_eq!(p.of(&player()).unwrap().volume, Some(19));
}

#[test]
fn group_rendering_control_is_plain_properties() {
    let mut p = Playback::default();
    let changes = p
        .apply(
            &player(),
            EventSource::GroupRenderingControl,
            &plain(&[
                ("GroupVolume", "33"),
                ("GroupMute", "0"),
                ("GroupVolumeChangeable", "1"),
            ]),
            Instant::now(),
        )
        .unwrap();
    assert_eq!(changes.group_volume, Some(33));
    let st = p.of(&player()).unwrap();
    assert_eq!((st.group_volume, st.group_mute), (Some(33), Some(false)));
}

#[test]
fn position_advances_while_playing_and_freezes_when_it_stops() {
    let mut p = Playback::default();
    let t0 = Instant::now();
    let playing = plain(&[(
        "LastChange",
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">\
         <TransportState val=\"PLAYING\"/><CurrentTrackURI val=\"x-file:a\"/>\
         <CurrentTrackDuration val=\"0:03:00\"/></InstanceID></Event>",
    )]);
    p.apply(&player(), EventSource::AvTransport, &playing, t0)
        .unwrap();
    p.apply_position(
        &player(),
        &PositionInfo {
            track: 1,
            duration_secs: Some(180),
            position_secs: Some(30),
            uri: "x-file:a".into(),
            metadata: None,
        },
        t0,
    );
    let st = p.of(&player()).unwrap();
    assert_eq!(st.position_at(t0 + Duration::from_secs(10)), Some(40));
    assert_eq!(
        st.position_at(t0 + Duration::from_secs(600)),
        Some(180),
        "capped at the end"
    );

    let paused = plain(&[(
        "LastChange",
        "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">\
         <TransportState val=\"PAUSED_PLAYBACK\"/></InstanceID></Event>",
    )]);
    p.apply(
        &player(),
        EventSource::AvTransport,
        &paused,
        t0 + Duration::from_secs(20),
    )
    .unwrap();
    let st = p.of(&player()).unwrap();
    assert_eq!(
        st.position_at(t0 + Duration::from_secs(100)),
        Some(50),
        "frozen at the pause"
    );
}

#[test]
fn a_new_track_restarts_the_position_and_is_reported() {
    let mut p = Playback::default();
    let t0 = Instant::now();
    let track = |uri: &str| {
        let lc = format!(
            "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">\
             <TransportState val=\"PLAYING\"/><CurrentTrackURI val=\"{uri}\"/></InstanceID></Event>"
        );
        plain(&[("LastChange", lc.as_str())])
    };
    p.apply(
        &player(),
        EventSource::AvTransport,
        &track("x-file:one"),
        t0,
    )
    .unwrap();
    let changes = p
        .apply(
            &player(),
            EventSource::AvTransport,
            &track("x-file:two"),
            t0 + Duration::from_secs(90),
        )
        .unwrap();
    assert_eq!(
        changes.track,
        Some((Some("x-file:one".into()), Some("x-file:two".into())))
    );
    let st = p.of(&player()).unwrap();
    assert_eq!(st.position_at(t0 + Duration::from_secs(95)), Some(5));
}

#[test]
fn the_same_uri_at_a_new_queue_position_is_a_new_track() {
    let mut p = Playback::default();
    let t0 = Instant::now();
    let at = |n: u32| {
        let lc = format!(
            "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">\
             <TransportState val=\"PLAYING\"/><CurrentTrack val=\"{n}\"/>\
             <CurrentTrackURI val=\"x-file:same\"/></InstanceID></Event>"
        );
        plain(&[("LastChange", lc.as_str())])
    };
    p.apply(&player(), EventSource::AvTransport, &at(1), t0)
        .unwrap();
    let again = p
        .apply(&player(), EventSource::AvTransport, &at(1), t0)
        .unwrap();
    assert_eq!(again.track, None, "same position, same track");
    let next = p
        .apply(
            &player(),
            EventSource::AvTransport,
            &at(2),
            t0 + Duration::from_secs(30),
        )
        .unwrap();
    assert_eq!(
        next.track,
        Some((Some("x-file:same".into()), Some("x-file:same".into())))
    );
    assert_eq!(p.of(&player()).unwrap().queue_position, Some(2));
    assert_eq!(
        p.of(&player())
            .unwrap()
            .position_at(t0 + Duration::from_secs(31)),
        Some(1)
    );
}

#[test]
fn a_queue_that_grows_under_the_same_track_is_reported() {
    let mut p = Playback::default();
    let now = Instant::now();
    let avt = |body: &str, seq| notify(body, seq);
    let first = p
        .apply(
            &player(),
            EventSource::AvTransport,
            &avt(AVT_INITIAL, 0),
            now,
        )
        .unwrap();
    assert_eq!(first.queue_length, Some(1));
    // Another app adds two tracks; the same track keeps playing.
    let grown = AVT_INITIAL.replace(
        "NumberOfTracks val=&quot;1&quot;",
        "NumberOfTracks val=&quot;3&quot;",
    );
    assert_ne!(grown, AVT_INITIAL, "the fixture names its queue length");
    let changes = p
        .apply(&player(), EventSource::AvTransport, &avt(&grown, 1), now)
        .unwrap();
    assert_eq!(changes.queue_length, Some(3));
    assert_eq!(changes.track, None, "the same track is playing");
    assert!(!changes.is_empty());
    assert_eq!(p.of(&player()).unwrap().queue_length, Some(3));
    // Nothing new: nothing reported.
    let again = p
        .apply(&player(), EventSource::AvTransport, &avt(&grown, 2), now)
        .unwrap();
    assert_eq!(again.queue_length, None);
}
