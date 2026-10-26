use super::*;
use crate::zones::fixtures::{households, id};
use fsonos_core::clock::FakeClock;
use fsonos_core::playback::{Changes, EventSource};
use fsonos_core::policy::Policy;
use fsonos_core::store::{MemStore, Store};
use fsonos_proto::gena::Notify;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;
use std::sync::mpsc;

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("history reconciliation must not poll the speakers");
    }
}

fn surface(store: MemStore) -> Surface {
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-25T18:00:00Z").unwrap();
    Surface::new(
        Box::new(NoIo),
        Box::new(|_| panic!("history reconciliation must not survey the house")),
        Policy::default(),
        Box::new(FakeClock::new(at)),
    )
    .with_action_log(Box::new(store), "test")
}

fn snapshot() -> Snapshot {
    Snapshot {
        households: households(),
        ..Snapshot::default()
    }
}

fn set(snapshot: &mut Snapshot, player: &PlayerId, uri: &str, position: Option<u32>, state: &str) {
    let position = position.map_or_else(String::new, |n| format!("<CurrentTrack val=\"{n}\"/>"));
    let notify = Notify {
        sid: "uuid:heard-test".into(),
        seq: 0,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!(
                "<Event><InstanceID val=\"0\"><TransportState val=\"{state}\"/>\
                 <CurrentTrackURI val=\"{uri}\"/>{position}</InstanceID></Event>"
            ),
        )],
    };
    snapshot
        .playback
        .apply(player, EventSource::AvTransport, &notify, Instant::now())
        .unwrap();
}

fn plays(surface: &Surface) -> Vec<PlayRecord> {
    surface
        .with_store(|store| store.recent_plays(None, 100))
        .unwrap()
        .unwrap()
}

#[test]
fn starts_from_the_snapshot_and_resync_catches_up_without_replaying_stale_deltas() {
    let surface = surface(MemStore::default());
    let player = id("RINCON_DEN");
    let mut snapshot = snapshot();
    set(
        &mut snapshot,
        &player,
        "test:already-playing",
        Some(1),
        "PLAYING",
    );
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 1);

    // The retained delta refers to a different player; after a gap the new
    // current snapshot, not that prefix, supplies the track to record.
    set(&mut snapshot, &player, "test:after-gap", Some(3), "PLAYING");
    reconcile(
        &surface,
        &snapshot,
        Some(&LiveEvent::Playback {
            player: id("RINCON_PATIO"),
            changes: Changes::default(),
        }),
    );
    assert_eq!(plays(&surface).len(), 1);
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    reconcile(&surface, &snapshot, None);
    assert_eq!(
        plays(&surface)
            .iter()
            .map(|p| p.source_uri.as_str())
            .collect::<Vec<_>>(),
        ["test:already-playing", "test:after-gap"]
    );
}

#[test]
fn consecutive_occurrences_and_returns_to_a_track_are_not_recent_write_duplicates() {
    let surface = surface(MemStore::default());
    let player = id("RINCON_DEN");
    let mut snapshot = snapshot();
    for (uri, slot) in [
        ("test:repeated", 1),
        ("test:repeated", 2),
        ("test:other", 3),
        ("test:repeated", 4),
    ] {
        set(&mut snapshot, &player, uri, Some(slot), "PLAYING");
        reconcile(&surface, &snapshot, None);
    }
    assert_eq!(
        plays(&surface).len(),
        4,
        "all four played within the same second"
    );
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    assert_eq!(plays(&surface).len(), 4);
}

#[test]
fn pause_resume_and_learning_the_initial_slot_do_not_make_another_play() {
    let surface = surface(MemStore::default());
    let player = id("RINCON_DEN");
    let mut snapshot = snapshot();
    for (slot, state) in [
        (None, "PLAYING"),
        (Some(8), "PLAYING"),
        (Some(8), "PAUSED_PLAYBACK"),
        (Some(8), "PLAYING"),
    ] {
        set(&mut snapshot, &player, "test:one-listen", slot, state);
        reconcile(&surface, &snapshot, None);
    }
    assert_eq!(plays(&surface).len(), 1);
    set(
        &mut snapshot,
        &player,
        "test:one-listen",
        Some(8),
        "STOPPED",
    );
    reconcile(&surface, &snapshot, None);
    set(
        &mut snapshot,
        &player,
        "test:one-listen",
        Some(8),
        "PLAYING",
    );
    reconcile(&surface, &snapshot, None);
    assert_eq!(
        plays(&surface).len(),
        2,
        "a confirmed stop ended the earlier play"
    );
}

#[test]
fn a_new_command_or_dj_record_and_a_restart_are_not_recorded_twice() {
    let player = id("RINCON_DEN");
    let mut store = MemStore::default();
    store
        .record_play(&player.0, "test:left-playing", 1)
        .unwrap();
    let surface = surface(store);
    let mut snapshot = snapshot();
    set(
        &mut snapshot,
        &player,
        "test:left-playing",
        Some(1),
        "PLAYING",
    );
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 1);
    let at = surface.now();
    surface
        .with_store(|store| store.record_play(&player.0, "test:new-command", at))
        .unwrap();
    set(
        &mut snapshot,
        &player,
        "test:new-command",
        Some(1),
        "PLAYING",
    );
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 2);
}

#[test]
fn topology_recovery_retires_old_coordinators_and_preserves_a_handover() {
    let surface = surface(MemStore::default());
    let (old, new) = (id("RINCON_DEN"), id("RINCON_KIT1"));
    let mut snapshot = snapshot();
    set(&mut snapshot, &old, "test:moving", Some(2), "PLAYING");
    set(&mut snapshot, &new, "", None, "STOPPED");
    reconcile(&surface, &snapshot, None);
    snapshot.households[0].groups[0].coordinator = new.clone();
    // The former member's cached state can precede its new subscription's
    // initial event. That stale STOPPED must not consume the handover proof.
    reconcile(&surface, &snapshot, Some(&LiveEvent::Topology));
    set(&mut snapshot, &new, "test:moving", Some(2), "PLAYING");
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    assert_eq!(plays(&surface).len(), 1);
    // The new coordinator has no history row. Its current occurrence must
    // nevertheless be acknowledged, even after the handover grace expires.
    for (_, ended) in &mut surface.heard.lock().unwrap().ended {
        *ended = Instant::now() - MOVED_WITHIN;
    }
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    assert_eq!(plays(&surface).len(), 1);

    // A former coordinator's delayed notification cannot enter history.
    set(&mut snapshot, &old, "test:stale-member", Some(3), "PLAYING");
    reconcile(
        &surface,
        &snapshot,
        Some(&LiveEvent::Playback {
            player: old,
            changes: Changes::default(),
        }),
    );
    assert_eq!(plays(&surface).len(), 1);
    set(&mut snapshot, &new, "test:next", Some(3), "PLAYING");
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    assert_eq!(plays(&surface).len(), 2);
}

#[test]
fn independent_groups_and_households_playing_the_same_uri_each_record_a_listen() {
    let surface = surface(MemStore::default());
    let mut snapshot = snapshot();
    let players = [id("RINCON_DEN"), id("RINCON_KIT2"), id("RINCON_PATIO")];
    for player in &players {
        set(
            &mut snapshot,
            player,
            "test:shared-song",
            Some(1),
            "PLAYING",
        );
    }
    reconcile(&surface, &snapshot, None);
    let records = plays(&surface);
    assert_eq!(records.len(), 3);
    for player in &players {
        assert_eq!(records.iter().filter(|p| p.zone == player.0).count(), 1);
    }
    reconcile(&surface, &snapshot, Some(&LiveEvent::Topology));
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    assert_eq!(plays(&surface).len(), 3);

    // An unchanged group's next occurrence cannot reuse another group's
    // currently playing URI as evidence of a coordinator handover.
    set(
        &mut snapshot,
        &players[0],
        "test:shared-song",
        Some(2),
        "PLAYING",
    );
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 4);
}

#[test]
fn a_topology_first_split_keeps_bounded_lineage_until_new_coordinator_playback_arrives() {
    let surface = surface(MemStore::default());
    let (old, new) = (id("RINCON_DEN"), id("RINCON_KIT1"));
    let mut snapshot = snapshot();
    set(&mut snapshot, &old, "test:handover", Some(4), "PLAYING");
    set(&mut snapshot, &new, "", None, "STOPPED");
    reconcile(&surface, &snapshot, None);

    // Both become standalone coordinators. The old one's unchanged playback
    // is processed first, before B's new subscription reports the same music.
    snapshot.households[0].groups[0].members = vec![old];
    snapshot.households[0].groups.push(fsonos_types::ZoneGroup {
        coordinator: new.clone(),
        members: vec![new.clone()],
    });
    reconcile(&surface, &snapshot, Some(&LiveEvent::Topology));
    set(&mut snapshot, &new, "test:handover", Some(4), "PLAYING");
    reconcile(
        &surface,
        &snapshot,
        Some(&LiveEvent::Playback {
            player: new.clone(),
            changes: Changes::default(),
        }),
    );
    assert_eq!(plays(&surface).len(), 1);
    // The handover exception is consumed once. A later occurrence belongs
    // to the new coordinator even while the former group plays the same URI.
    set(&mut snapshot, &new, "test:handover", Some(5), "PLAYING");
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 2);
}

#[test]
fn driver_starts_immediately_and_cannot_starve_recovery_with_events() {
    let (send, events) = mpsc::channel();
    for n in 0..100 {
        send.send(n).unwrap();
    }
    let mut startup = 0;
    let mut delivered = 0;
    drive(&events, Duration::from_millis(5), |event| {
        if event.is_some() {
            delivered += 1;
            std::thread::sleep(Duration::from_millis(1));
        } else {
            startup += 1;
        }
        startup < 2
    });
    assert_eq!(startup, 2);
    assert!(delivered > 0 && delivered < 100);
}

#[test]
fn a_failed_store_access_does_not_consume_the_observation() {
    let surface = surface(MemStore::default());
    let player = id("RINCON_DEN");
    let mut snapshot = snapshot();
    set(&mut snapshot, &player, "test:retry", Some(1), "PLAYING");
    let store = &surface.log.as_ref().unwrap().store;
    // Make one store access fail, then restore access without changing any
    // player state. Recovery must still record the current occurrence.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = store.lock().unwrap();
        panic!("temporary store failure");
    }));
    reconcile(&surface, &snapshot, None);
    store.clear_poison();
    assert!(plays(&surface).is_empty());
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 1);
}
