//! The same-URI invalidation goes through the ordinary public event contract.

use super::*;
use fsonos_core::playback::{Changes, EventSource, Playback};
use fsonos_proto::gena::Notify;

#[test]
fn metadata_changes_publish_a_read_invalidation_not_a_fake_track_transition() {
    let bus = EventBus::new();
    let households = crate::zones::fixtures::households();
    let player = crate::zones::fixtures::id("RINCON_DEN");
    bus.publish_live(
        &LiveEvent::Playback {
            player: player.clone(),
            changes: Changes {
                metadata_changed: true,
                ..Changes::default()
            },
        },
        &households,
    );
    let answer = poll(&bus, 0, Duration::ZERO);
    assert_eq!(answer.reset, None);
    assert_eq!(answer.events.len(), 1);
    let event = &answer.events[0];
    assert_eq!(event.event, "zone.state");
    assert_eq!(event.data["player"], player.0);
    assert!(event.data["room"].is_string());
    assert_eq!(event.data["metadata_changed"], true);
    assert_eq!(event.data.as_object().unwrap().len(), 3);
    assert!(!event.data.to_string().contains("http"));
    assert_eq!(
        bus.after(0).unwrap()[0].frame(),
        frame(Some(1), "zone.state", &event.data.to_string())
    );
}

#[test]
fn ordinary_playback_deltas_do_not_claim_a_metadata_change() {
    let bus = EventBus::new();
    bus.publish_live(
        &LiveEvent::Playback {
            player: PlayerId("test-player".into()),
            changes: Changes {
                volume: Some(23),
                ..Changes::default()
            },
        },
        &[],
    );
    let answer = bus.after(0).unwrap();
    assert!(answer[0].data.get("metadata_changed").is_none());
}

#[test]
fn parsed_same_uri_metadata_survives_the_live_filter_and_reaches_json_consumers() {
    let bus = EventBus::new();
    let player = PlayerId("test-stream".into());
    let now = Instant::now();
    let mut playback = Playback::default();
    let event = |title: &str| Notify {
        sid: "uuid:metadata-test".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!(
                "<Event><InstanceID val=\"0\">\
                 <TransportState val=\"PLAYING\"/><CurrentTrackURI val=\"test:stream\"/>\
                 <CurrentTrackMetaData val=\"&lt;DIDL-Lite&gt;&lt;item id=&quot;1&quot;&gt;\
                 &lt;dc:title xmlns:dc=&quot;http://purl.org/dc/elements/1.1/&quot;&gt;{title}&lt;/dc:title&gt;\
                 &lt;/item&gt;&lt;/DIDL-Lite&gt;\"/></InstanceID></Event>"
            ),
        )],
    };
    for title in ["First", "Second", "Second"] {
        let changes = playback
            .apply(&player, EventSource::AvTransport, &event(title), now)
            .unwrap();
        // The production live worker only emits nonempty changes.
        if !changes.is_empty() {
            bus.publish_live(
                &LiveEvent::Playback {
                    player: player.clone(),
                    changes,
                },
                &[],
            );
        }
    }
    let answer = poll(&bus, 0, Duration::ZERO);
    assert_eq!(answer.events.len(), 2);
    assert_eq!(answer.events[1].data["metadata_changed"], true);
    assert!(answer.events[1].data.get("track").is_none());
    assert_eq!(
        playback.of(&player).unwrap().now_playing.as_ref().unwrap().title,
        "Second"
    );
}
