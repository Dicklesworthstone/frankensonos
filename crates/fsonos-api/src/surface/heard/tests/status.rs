use super::*;

fn status(snapshot: &mut Snapshot, player: &PlayerId, value: &str) -> LiveEvent {
    let notify = Notify {
        sid: "uuid:heard-status-test".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!(
                "<Event><InstanceID val=\"0\"><TransportStatus val=\"{value}\"/>\
                 </InstanceID></Event>"
            ),
        )],
    };
    let changes = snapshot
        .playback
        .apply(player, EventSource::AvTransport, &notify, Instant::now())
        .unwrap();
    assert!(changes.transport.is_none());
    assert!(changes.track.is_none());
    LiveEvent::Playback {
        player: player.clone(),
        changes,
    }
}

#[test]
fn a_failed_renderer_creates_no_listening_evidence_until_status_only_recovery() {
    let surface = surface(MemStore::default());
    let player = id("RINCON_DEN");
    let mut snapshot = snapshot();
    set(
        &mut snapshot,
        &player,
        "x-sonos-spotify:spotify%3atrack%3afailedTrack?sid=1",
        Some(4),
        "PLAYING",
    );
    let failed = status(&mut snapshot, &player, " ERROR_OCCURRED ");
    for event in [
        None,
        Some(&failed),
        Some(&LiveEvent::Resync),
        Some(&LiveEvent::Health {
            player: player.clone(),
            health: Health::Healthy,
        }),
    ] {
        reconcile(&surface, &snapshot, event);
        assert!(plays(&surface).is_empty(), "failed playback is not a listen");
    }
    {
        let heard = surface.heard.lock().unwrap();
        assert!(!heard.now.contains_key(&player));
        assert!(!heard.recorded.contains_key(&player));
        assert!(heard.acquired.contains(&player));
    }

    let recovered = status(&mut snapshot, &player, "OK");
    reconcile(&surface, &snapshot, Some(&recovered));
    reconcile(&surface, &snapshot, None);
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    let recorded = plays(&surface);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].source_uri, "spotify:track:failedTrack");
    assert_eq!(recorded[0].zone, player.0);
    assert_eq!(recorded[0].played_at, surface.now());
}

#[test]
fn a_failed_duplicate_occurrence_does_not_consume_its_later_successful_play() {
    let surface = surface(MemStore::default());
    let player = id("RINCON_DEN");
    let mut snapshot = snapshot();
    set(&mut snapshot, &player, "test:repeated", Some(1), "PLAYING");
    reconcile(&surface, &snapshot, None);

    status(&mut snapshot, &player, "ERROR_OCCURRED");
    set(&mut snapshot, &player, "test:repeated", Some(2), "PLAYING");
    reconcile(&surface, &snapshot, Some(&LiveEvent::Resync));
    assert_eq!(plays(&surface).len(), 1);
    assert_eq!(
        surface.heard.lock().unwrap().now[&player]
            .as_ref()
            .unwrap()
            .position,
        Some(1),
        "a partial event omitting status cannot acknowledge the failed occurrence"
    );

    // Explicitly clearing status returns it to unknown, rather than OK.
    // Unknown status retains the existing observed-PLAYING contract.
    let recovered = status(&mut snapshot, &player, "");
    reconcile(&surface, &snapshot, Some(&recovered));
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 2, "both actual occurrences count");
}

#[test]
fn a_renderer_error_and_retry_preserve_the_already_recorded_occurrence() {
    let surface = surface(MemStore::default());
    let player = id("RINCON_DEN");
    let mut snapshot = snapshot();
    set(&mut snapshot, &player, "test:recovering", Some(1), "PLAYING");
    reconcile(&surface, &snapshot, None);
    let before = plays(&surface);

    let failed = status(&mut snapshot, &player, "ERROR_OCCURRED");
    reconcile(&surface, &snapshot, Some(&failed));
    set(&mut snapshot, &player, "test:recovering", Some(1), "STOPPED");
    reconcile(&surface, &snapshot, None);
    set(&mut snapshot, &player, "test:recovering", Some(1), "PLAYING");
    let recovered = status(&mut snapshot, &player, "OK");
    reconcile(&surface, &snapshot, Some(&recovered));
    assert_eq!(plays(&surface), before, "a retry is not an invented repeat");

    // A later ordinary stop still ends this play, as before.
    set(&mut snapshot, &player, "test:recovering", Some(1), "STOPPED");
    reconcile(&surface, &snapshot, None);
    set(&mut snapshot, &player, "test:recovering", Some(1), "PLAYING");
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 2);
}

#[test]
fn absent_and_non_error_statuses_keep_the_observed_playback_contract() {
    for wire in [
        None,
        Some(""),
        Some("OK"),
        Some("VENDOR_STATUS"),
        Some("error_occurred"),
    ] {
        let surface = surface(MemStore::default());
        let player = id("RINCON_DEN");
        let mut snapshot = snapshot();
        set(&mut snapshot, &player, "test:compatible", Some(1), "PLAYING");
        if let Some(wire) = wire {
            status(&mut snapshot, &player, wire);
        }
        reconcile(&surface, &snapshot, None);
        reconcile(&surface, &snapshot, None);
        assert_eq!(plays(&surface).len(), 1, "status {wire:?}");
    }
}

#[test]
fn a_failed_new_coordinator_keeps_the_handover_until_healthy_playback() {
    let surface = surface(MemStore::default());
    let (old, new) = (id("RINCON_DEN"), id("RINCON_KIT1"));
    let mut snapshot = snapshot();
    set(&mut snapshot, &old, "test:moving", Some(2), "PLAYING");
    reconcile(&surface, &snapshot, None);

    snapshot.households[0].groups[0].coordinator = new.clone();
    set(&mut snapshot, &new, "test:moving", Some(2), "PLAYING");
    status(&mut snapshot, &new, "ERROR_OCCURRED");
    reconcile(&surface, &snapshot, Some(&LiveEvent::Topology));
    {
        let heard = surface.heard.lock().unwrap();
        assert!(heard.acquired.contains(&new));
        assert!(!heard.now.contains_key(&new));
    }
    assert_eq!(plays(&surface).len(), 1);

    let recovered = status(&mut snapshot, &new, "OK");
    reconcile(&surface, &snapshot, Some(&recovered));
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 1, "the moved music is still one play");
    assert!(!surface.heard.lock().unwrap().acquired.contains(&new));

    set(&mut snapshot, &new, "test:next", Some(3), "PLAYING");
    reconcile(&surface, &snapshot, None);
    assert_eq!(plays(&surface).len(), 2);
}
