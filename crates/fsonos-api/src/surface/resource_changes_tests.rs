use super::Surface;
use super::testing::surface;
use crate::events::{EventBus, RING, ResourceChanges};
use crate::failure::ErrorCode;
use fsonos_core::live::LiveEvent;
use fsonos_core::policy::{Client, Policy};
use serde_json::json;
use std::sync::{Arc, mpsc};
use std::time::Duration;

fn journal() -> (Surface, Arc<EventBus>) {
    let (mut surface, _) = surface("", Vec::new());
    let bus = Arc::new(EventBus::new());
    surface.events = Some(Arc::clone(&bus));
    (surface, bus)
}

#[test]
fn internal_resource_changes_do_not_inherit_client_denials() {
    let (surface, bus) = journal();
    let policy = Policy::from_toml(
        r#"
[clients.unknown]
allow = []
[clients."loopback-http"]
allow = []
[clients."reader@example.com"]
allow = ["events", "list_zones"]
"#,
    )
    .unwrap();
    *surface.policy.write().unwrap() = Arc::new(policy);
    bus.publish(
        "action.logged",
        json!({"client": "synthetic-private-caller", "intent": "synthetic-private-action"}),
    );

    // Exhaustively name the returned fields: no event payload is exposed.
    let ResourceChanges { last_id, reset } = surface
        .resource_changes_after(Some(0), Duration::ZERO)
        .unwrap();
    assert_eq!((last_id, reset), (1, None));
    for client in [Client::Unknown, Client::LoopbackHttp] {
        assert_eq!(
            surface
                .events_after(&client, Some(0), Duration::ZERO)
                .unwrap_err()
                .code,
            ErrorCode::PolicyDenied,
        );
    }
    let allowed = surface
        .events_after(
            &Client::Tailnet("reader@example.com".into()),
            Some(0),
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(allowed.events[0].data["intent"], "synthetic-private-action");
}

#[test]
fn resource_changes_require_the_daemons_journal() {
    let (surface, _) = surface("", Vec::new());
    assert_eq!(
        surface
            .resource_changes_after(None, Duration::ZERO)
            .unwrap_err()
            .code,
        ErrorCode::NotImplemented,
    );
}

#[test]
fn resource_changes_start_at_the_tail_and_report_a_quiet_timeout() {
    let (surface, bus) = journal();
    bus.publish("zone.state", json!({"volume": 31}));
    let tail = ResourceChanges {
        last_id: 1,
        reset: None,
    };
    assert_eq!(
        surface.resource_changes_after(None, Duration::ZERO),
        Ok(tail)
    );
    assert_eq!(
        surface.resource_changes_after(Some(1), Duration::from_millis(5)),
        Ok(tail)
    );
}

#[test]
fn resource_changes_report_retention_loss_and_a_previous_daemon_cursor() {
    let (surface, bus) = journal();
    for _ in 0..RING + 3 {
        bus.publish("zone.state", json!({"volume": 31}));
    }
    let last_id = u64::try_from(RING + 3).unwrap();
    assert_eq!(
        surface.resource_changes_after(Some(0), Duration::ZERO),
        Ok(ResourceChanges {
            last_id,
            reset: Some(4),
        })
    );
    assert_eq!(
        surface.resource_changes_after(Some(3), Duration::ZERO),
        Ok(ResourceChanges {
            last_id,
            reset: None,
        })
    );
    assert_eq!(
        surface.resource_changes_after(Some(last_id + 1), Duration::ZERO),
        Ok(ResourceChanges {
            last_id,
            reset: Some(4),
        })
    );
}

fn waiting_change(publish: impl FnOnce(&EventBus)) -> ResourceChanges {
    let (surface, bus) = journal();
    let (send, receive) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let answer = surface.resource_changes_after(Some(0), Duration::from_secs(10));
        let _ = send.send(answer);
    });
    std::thread::sleep(Duration::from_millis(20));
    publish(&bus);
    let answer = receive
        .recv_timeout(Duration::from_secs(5))
        .expect("journal activity wakes the resource publisher")
        .unwrap();
    waiter.join().unwrap();
    answer
}

#[test]
fn resource_changes_wake_on_new_events() {
    assert_eq!(
        waiting_change(|bus| {
            bus.publish("zone.state", json!({"volume": 31}));
        }),
        ResourceChanges {
            last_id: 1,
            reset: None,
        }
    );
}

#[test]
fn resource_changes_wake_on_a_lost_final_event_without_another_delta() {
    assert_eq!(
        waiting_change(|bus| {
            bus.publish_live(&LiveEvent::Resync, &[]);
        }),
        ResourceChanges {
            last_id: 1,
            reset: Some(2),
        }
    );
}
