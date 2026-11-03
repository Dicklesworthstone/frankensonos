//! Source transitions must reach consumers even when the track is unchanged.

use super::*;
use fsonos_core::playback::{Changes, EventSource, Playback};
use fsonos_proto::gena::Notify;

#[test]
fn source_changes_publish_an_invalidation_without_source_addresses() {
    let bus = EventBus::new();
    let households = crate::zones::fixtures::households();
    let player = crate::zones::fixtures::id("RINCON_DEN");
    for next in [Some("http://192.0.2.250:3400/media/temporary.wav"), None] {
        bus.publish_live(
            &LiveEvent::Playback {
                player: player.clone(),
                changes: Changes {
                    media_uri: Some((
                        Some("x-rincon-queue:test-player#0".into()),
                        next.map(str::to_owned),
                    )),
                    ..Changes::default()
                },
            },
            &households,
        );
    }
    let answer = poll(&bus, 0, Duration::ZERO);
    assert_eq!(answer.reset, None);
    assert_eq!(answer.events.len(), 2);
    for event in &answer.events {
        assert_eq!(event.event, "zone.state");
        assert_eq!(event.data["player"], player.0);
        assert!(event.data["room"].is_string());
        assert_eq!(event.data["source_changed"], true);
        assert_eq!(event.data.as_object().unwrap().len(), 3);
        assert!(!event.data.to_string().contains("http"));
        assert!(!event.data.to_string().contains("x-rincon"));
        assert_eq!(
            bus.after(event.id - 1).unwrap()[0].frame(),
            frame(Some(event.id), "zone.state", &event.data.to_string())
        );
    }
}

#[test]
fn ordinary_volume_deltas_do_not_claim_a_source_change() {
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
    assert!(answer[0].data.get("source_changed").is_none());
}

#[test]
fn a_source_only_gena_transition_reaches_consumers_once_without_advancing_the_track() {
    let bus = EventBus::new();
    let player = PlayerId("test-player".into());
    let now = Instant::now();
    let mut playback = Playback::default();
    let notification = |source: &str| Notify {
        sid: "uuid:source-test".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!(
                "<Event><InstanceID val=\"0\">\
                 <TransportState val=\"PLAYING\"/>\
                 <AVTransportURI val=\"{source}\"/>\
                 <CurrentTrackURI val=\"test:retained-track\"/>\
                 </InstanceID></Event>"
            ),
        )],
    };
    for source in [
        "x-rincon-queue:test-player#0",
        "test:temporary",
        "test:temporary",
        "x-rincon-queue:test-player#0",
    ] {
        let changes = playback
            .apply(
                &player,
                EventSource::AvTransport,
                &notification(source),
                now,
            )
            .unwrap();
        // The live worker only publishes accepted, nonempty changes.
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
    assert_eq!(answer.events.len(), 3);
    for event in &answer.events[1..] {
        assert_eq!(event.data["source_changed"], true);
        assert!(event.data.get("track").is_none());
        assert!(event.data.get("transport").is_none());
    }
    let state = playback.of(&player).unwrap();
    assert_eq!(state.track_uri.as_deref(), Some("test:retained-track"));
    assert!(state.is_queue_of(&player));
}
