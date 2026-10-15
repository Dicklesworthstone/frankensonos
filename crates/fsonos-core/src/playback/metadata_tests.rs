//! Same-URI metadata is a real observation, not a transport or queue transition.

use super::*;
use fsonos_proto::control::{MediaInfo, TransportInfo};
use fsonos_proto::didl::{parse_didl, xml_escape as escape};
use std::time::Duration;

const URI: &str = "x-rincon-mp3radio://radio.example.invalid/live";

fn player() -> PlayerId {
    PlayerId("test-radio".into())
}

fn document(title: &str, artist: &str, album: &str, art: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"radio\" parentID=\"0\" restricted=\"1\">\
         <dc:title>{}</dc:title><dc:creator>{}</dc:creator>\
         <upnp:album>{}</upnp:album><upnp:albumArtURI>{}</upnp:albumArtURI>\
         <upnp:class>object.item.audioItem.audioBroadcast</upnp:class>\
         </item></DIDL-Lite>",
        escape(title),
        escape(artist),
        escape(album),
        escape(art)
    )
}

fn notify(body: &str) -> Notify {
    Notify {
        sid: "uuid:test-radio".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
        )],
    }
}

fn metadata(doc: &str) -> Notify {
    notify(&format!("<CurrentTrackMetaData val=\"{}\"/>", escape(doc)))
}

fn sample(doc: &str) -> PlaybackObservation {
    PlaybackObservation {
        transport: TransportInfo {
            state: TransportState::Playing,
            status: "OK".into(),
        },
        position: PositionInfo {
            track: 0,
            uri: URI.into(),
            duration_secs: Some(0),
            position_secs: Some(80),
            metadata: parse_didl(doc).unwrap().into_iter().next(),
        },
        media: MediaInfo {
            uri: URI.into(),
            tracks: 1,
            uri_metadata: String::new(),
        },
    }
}

fn playing(now: Instant) -> (Playback, String) {
    let doc = document("First song", "First artist", "First album", "/first-art");
    let mut playback = Playback::default();
    playback.apply_observation(&player(), &sample(&doc), now);
    let state = playback.players.get_mut(&player()).unwrap();
    state.volume = Some(23);
    state.mute = Some(true);
    (playback, doc)
}

fn metadata_only(changes: &Changes) {
    assert_eq!(
        *changes,
        Changes {
            metadata_changed: true,
            ..Changes::default()
        }
    );
    assert!(!changes.is_empty());
}

#[test]
fn each_same_uri_track_detail_is_published_without_resetting_playback() {
    let now = Instant::now();
    let at = now + Duration::from_secs(10);
    for doc in [
        document("Next <song> & 雨", "First artist", "First album", "/first-art"),
        document("First song", "Next artist", "First album", "/first-art"),
        document("First song", "First artist", "Next album", "/first-art"),
        document("First song", "First artist", "First album", "/next-art?x=1&y=2"),
    ] {
        let (mut playback, _) = playing(now);
        let changes = playback
            .apply(&player(), EventSource::AvTransport, &metadata(&doc), at)
            .unwrap();
        metadata_only(&changes);
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.track_uri.as_deref(), Some(URI));
        assert_eq!(state.queue_position, None);
        assert_eq!(state.position_at(at), Some(90));
        assert_eq!(state.transport, Some(TransportState::Playing));
        assert_eq!(state.expected_end(), None);
        assert_eq!(state.volume, Some(23));
        assert_eq!(state.mute, Some(true));
        assert_eq!(
            state.now_playing,
            sample(&doc).position.metadata.as_ref().map(NowPlaying::from_didl)
        );
    }
}

#[test]
fn a_repeated_or_equivalently_encoded_document_does_not_emit_an_event() {
    let now = Instant::now();
    let (mut playback, doc) = playing(now);
    for doc in [doc.clone(), doc.replace("><", ">\n<")] {
        let changes = playback
            .apply(&player(), EventSource::AvTransport, &metadata(&doc), now)
            .unwrap();
        assert!(changes.is_empty());
    }
}

#[test]
fn clearing_metadata_is_visible_once_without_clearing_the_stream() {
    let now = Instant::now();
    for empty in ["", "NOT_IMPLEMENTED"] {
        let (mut playback, _) = playing(now);
        let event = metadata(empty);
        let changes = playback
            .apply(&player(), EventSource::AvTransport, &event, now)
            .unwrap();
        metadata_only(&changes);
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.now_playing, None);
        assert_eq!(state.track_uri.as_deref(), Some(URI));
        assert_eq!(state.position_at(now), Some(80));
        assert!(
            playback
                .apply(&player(), EventSource::AvTransport, &event, now)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn omitted_metadata_and_room_volume_updates_keep_the_last_details() {
    let now = Instant::now();
    let (mut playback, _) = playing(now);
    let before = playback.of(&player()).unwrap().now_playing.clone();
    let same = notify("<TransportState val=\"PLAYING\"/>");
    assert!(
        playback
            .apply(&player(), EventSource::AvTransport, &same, now)
            .unwrap()
            .is_empty()
    );
    let volume = notify("<Volume channel=\"Master\" val=\"24\"/>");
    let changes = playback
        .apply(&player(), EventSource::RenderingControl, &volume, now)
        .unwrap();
    assert_eq!(changes.volume, Some(24));
    assert!(!changes.metadata_changed);
    assert_eq!(playback.of(&player()).unwrap().now_playing, before);
}

#[test]
fn malformed_metadata_cannot_partially_publish_a_new_duration_or_transport() {
    let now = Instant::now();
    let (mut playback, _) = playing(now);
    let before = playback.of(&player()).unwrap().clone();
    let bad = notify(
        "<TransportState val=\"PAUSED_PLAYBACK\"/><CurrentTrackDuration val=\"0:04:00\"/>\
         <CurrentTrackMetaData val=\"not-xml\"/>",
    );
    assert!(
        playback
            .apply(&player(), EventSource::AvTransport, &bad, now)
            .is_err()
    );
    assert_eq!(playback.of(&player()), Some(&before));
    let good = metadata(&document("Next song", "Artist", "Album", "/new-art"));
    metadata_only(
        &playback
            .apply(&player(), EventSource::AvTransport, &good, now)
            .unwrap(),
    );
}

#[test]
fn a_duration_correction_is_visible_without_restarting_the_sample_clock() {
    let now = Instant::now();
    let at = now + Duration::from_secs(10);
    let (mut playback, _) = playing(now);
    let event = notify("<CurrentTrackDuration val=\"0:02:00\"/>");
    metadata_only(
        &playback
            .apply(&player(), EventSource::AvTransport, &event, at)
            .unwrap(),
    );
    let state = playback.of(&player()).unwrap();
    assert_eq!(state.duration_secs, Some(120));
    assert_eq!(state.position_at(at), Some(90));
    assert_eq!(state.expected_end(), Some(now + Duration::from_secs(40)));
    assert!(
        playback
            .apply(&player(), EventSource::AvTransport, &event, at)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn same_uri_readbacks_report_new_details_and_preserve_missing_optional_values() {
    let now = Instant::now();
    let at = now + Duration::from_secs(10);
    let (mut playback, _) = playing(now);
    let mut fresh = sample(&document("Read-back song", "Artist", "Album", "/fresh-art"));
    fresh.position.position_secs = Some(88);
    metadata_only(&playback.apply_observation(&player(), &fresh, at));
    let state = playback.of(&player()).unwrap();
    let details = state.now_playing.clone();
    assert_eq!(state.position_at(at), Some(88));
    assert_eq!(state.track_uri.as_deref(), Some(URI));
    fresh.position.metadata = None;
    fresh.position.duration_secs = None;
    fresh.position.position_secs = None;
    assert!(playback.apply_observation(&player(), &fresh, at).is_empty());
    assert_eq!(playback.of(&player()).unwrap().now_playing, details);
    assert_eq!(playback.of(&player()).unwrap().duration_secs, Some(0));
}

#[test]
fn late_initial_metadata_on_a_paused_track_does_not_resume_it() {
    let now = Instant::now();
    let mut playback = Playback::default();
    let doc = document("Late title", "Artist", "Album", "/late-art");
    let mut initial = sample(&doc);
    initial.position.metadata = None;
    initial.transport.state = TransportState::Paused;
    playback.apply_observation(&player(), &initial, now);
    metadata_only(
        &playback
            .apply(&player(), EventSource::AvTransport, &metadata(&doc), now)
            .unwrap(),
    );
    let state = playback.of(&player()).unwrap();
    assert_eq!(state.transport, Some(TransportState::Paused));
    assert_eq!(state.position_at(now + Duration::from_secs(30)), Some(80));
    assert_eq!(state.expected_end(), None);
}

#[test]
fn replacing_or_clearing_a_source_reports_removed_details_too() {
    let now = Instant::now();
    for uri in ["", "test:new-source"] {
        let (mut playback, _) = playing(now);
        let event = notify(&format!("<CurrentTrackURI val=\"{uri}\"/>"));
        let changes = playback
            .apply(&player(), EventSource::AvTransport, &event, now)
            .unwrap();
        assert!(changes.track.is_some());
        assert!(changes.metadata_changed);
        assert_eq!(playback.of(&player()).unwrap().now_playing, None);
    }
}
