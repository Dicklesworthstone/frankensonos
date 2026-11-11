//! Native content updates invalidate observations without inventing playback.

use super::*;
use fsonos_core::events::ContentChanges;

#[test]
fn queue_edits_publish_even_without_a_playback_or_length_delta() {
    let bus = EventBus::new();
    let households = crate::zones::fixtures::households();
    let player = crate::zones::fixtures::id("RINCON_DEN");
    bus.publish_live(
        &LiveEvent::ContentChanged {
            player: player.clone(),
            changes: ContentChanges {
                queue: true,
                ..ContentChanges::default()
            },
        },
        &households,
    );
    let answer = poll(&bus, 0, Duration::ZERO);
    assert_eq!(answer.events.len(), 1);
    let event = &answer.events[0];
    assert_eq!(event.event, "zone.state");
    assert_eq!(event.data["player"], player.0);
    assert!(event.data["room"].is_string());
    assert_eq!(event.data["queue_changed"], true);
    assert_eq!(event.data.as_object().unwrap().len(), 3);
    for field in ["track", "transport", "queue_length", "revision"] {
        assert!(event.data.get(field).is_none(), "no fabricated {field}");
    }
}

#[test]
fn catalog_invalidations_name_the_same_household_as_public_zone_addresses() {
    let bus = EventBus::new();
    let households = crate::zones::fixtures::households();
    let labels = fsonos_core::rooms::household_labels(&households);
    for (household, label) in households.iter().zip(&labels) {
        let player = &household.groups[0].coordinator;
        let cursor = bus.last_id();
        bus.publish_live(
            &LiveEvent::ContentChanged {
                player: player.clone(),
                changes: ContentChanges {
                    favorites: true,
                    playlists: false,
                    ..ContentChanges::default()
                },
            },
            &households,
        );
        let events = bus.after(cursor).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "catalog.changed");
        assert_eq!(events[0].data["household"], *label);
        assert_eq!(events[0].data["favorites"], true);
        assert_eq!(events[0].data["playlists"], false);
        assert!(events[0].data.get("queue_changed").is_none());
    }
}

#[test]
fn unknown_players_still_invalidate_without_guessing_a_household() {
    let bus = EventBus::new();
    bus.publish_live(
        &LiveEvent::ContentChanged {
            player: PlayerId("test-unknown".into()),
            changes: ContentChanges {
                queue: true,
                favorites: true,
                playlists: true,
            },
        },
        &[],
    );
    let answer = poll(&bus, 0, Duration::ZERO);
    assert_eq!(answer.events.len(), 2);
    assert_eq!(answer.events[0].event, "zone.state");
    assert_eq!(answer.events[1].event, "catalog.changed");
    assert!(answer.events[1].data.get("household").is_none());
    assert_eq!(answer.events[1].data["favorites"], true);
    assert_eq!(answer.events[1].data["playlists"], true);
}

#[test]
fn empty_content_changes_do_not_claim_an_invalidation() {
    let bus = EventBus::new();
    bus.publish_live(
        &LiveEvent::ContentChanged {
            player: PlayerId("test-empty".into()),
            changes: ContentChanges::default(),
        },
        &[],
    );
    assert!(bus.after(0).unwrap().is_empty());
}
