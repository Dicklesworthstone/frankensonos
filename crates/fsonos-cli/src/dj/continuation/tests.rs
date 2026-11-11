use super::*;
use fsonos_api::dj::{DjEngine, DjSpeakers};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Client;
use fsonos_core::store::MemStore;
use fsonos_proto::{ProtoError, Transport};
use fsonos_spotify::dj::{DjConfig, PickReason};
use fsonos_spotify::feed::{FeedSnapshot, Movement, QueueFeed, QueuedWork};
use fsonos_types::TransportState;
use std::net::IpAddr;

fn pending(id: &PlayerId) -> QueueFeed {
    QueueFeed::restore(
        FeedSnapshot {
            zone: id.0.clone(),
            earlier: Vec::new(),
            lookahead: 1,
            queued: vec![QueuedWork {
                title: "Retained work".into(),
                composer: "Synthetic composer".into(),
                work_key: "continuation-fixture".into(),
                movements: (1..=2)
                    .map(|n| Movement {
                        uri: format!("spotify:track:{n:022}"),
                        title: format!("Movement {n}"),
                        position: Some(n),
                        added: true,
                    })
                    .collect(),
                reason: PickReason {
                    factors: Vec::new(),
                    relaxed: Vec::new(),
                    summary: "fixture".into(),
                },
            }],
            playing: Some(2),
            recorded: Some((2, "spotify:track:0000000000000000000002".into())),
            played: Vec::new(),
            dry_end: Some(2),
            recovery: Default::default(),
        },
        DjConfig::default(),
        1,
    )
}

fn event(id: &PlayerId) -> PlayerPlayback {
    let mut event = PlayerPlayback::default();
    event.transport = Some(TransportState::Stopped);
    event.media_uri = Some(format!("x-rincon-queue:{}#0", id.0));
    event.track_uri = Some("spotify:track:0000000000000000000001".into());
    event.queue_position = Some(1);
    event.queue_length = Some(2);
    event
}

fn insert(dj: &SpotifyDj, id: &PlayerId) {
    let mut state = dj.state();
    state.feeds.insert(id.clone(), pending(id));
    state.starters.insert(id.clone(), vec![Client::Cli]);
}

#[test]
fn a_saved_continuation_is_consumed_only_for_the_attempted_coordinator() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(saved::FEEDS_FILE);
    let dj = SpotifyDj::new(None).with_saved_feeds(Some(path.clone()));
    let id = PlayerId("test-coordinator".into());
    let other = PlayerId("test-other-coordinator".into());
    insert(&dj, &id);
    insert(&dj, &other);
    let mut state = dj.state();
    dj.save_checked(&mut state).unwrap();
    let before = state.feeds[&id].snapshot().unwrap();
    assert!(prepare(&dj, &mut state, &id, &event(&id)));
    assert_eq!(state.feeds[&id].snapshot().unwrap(), before);
    let (restored, _, starters) = saved::load(&path, 2);
    let after = restored[&id].snapshot().unwrap();
    assert_eq!(after.dry_end, None);
    assert_eq!(after.playing, None);
    assert_eq!(after.queued, before.queued);
    assert_eq!(after.recorded, before.recorded);
    assert_eq!(starters[&id], [Client::Cli]);
    assert_eq!(restored[&other].snapshot().unwrap().dry_end, Some(2));
    assert_eq!(restored[&other].snapshot().unwrap().playing, Some(2));

    // A known refusal retains the in-memory cursor and can durably rearm it.
    dj.save_checked(&mut state).unwrap();
    let (restored, _, _) = saved::load(&path, 3);
    assert_eq!(restored[&id].snapshot().unwrap(), before);
}

#[test]
fn a_failed_post_attempt_save_cannot_restore_the_consumed_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(saved::FEEDS_FILE);
    let mut dj = SpotifyDj::new(None).with_saved_feeds(Some(path.clone()));
    let id = PlayerId("test-coordinator".into());
    insert(&dj, &id);
    {
        let mut state = dj.state();
        dj.save_checked(&mut state).unwrap();
        assert!(prepare(&dj, &mut state, &id, &event(&id)));
        // An unconfirmed start retires the live feed. Its later save fails.
        state.feeds.remove(&id);
    }
    dj.feeds_file = Some(dir.path().to_path_buf());
    assert!(dj.save_checked(&mut dj.state()).is_err());
    let restarted = SpotifyDj::new(None).with_saved_feeds(Some(path));
    let state = restarted.state();
    let retained = state.feeds[&id].snapshot().unwrap();
    assert_eq!(retained.dry_end, None);
    assert_eq!(retained.playing, None);
    assert_eq!(state.starters[&id], [Client::Cli]);
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a failed durable checkpoint must prevent speaker I/O")
    }

    fn http_get(&self, _: &str) -> Result<String, ProtoError> {
        panic!("a failed durable checkpoint must prevent HTTP I/O")
    }
}

#[test]
fn failed_checkpoint_defers_the_daemon_before_any_speaker_calls() {
    let dir = tempfile::tempdir().unwrap();
    let mut dj = SpotifyDj::new(None);
    // A directory cannot be replaced by the saved-state file.
    dj.feeds_file = Some(dir.path().to_path_buf());
    let id = PlayerId("test-coordinator".into());
    insert(&dj, &id);
    let before = dj.state().feeds[&id].snapshot();
    for (transport, status) in [
        (TransportState::Stopped, None),
        (TransportState::Stopped, Some("ERROR_OCCURRED")),
        (TransportState::Paused, None),
        (TransportState::Playing, None),
    ] {
        let mut playback = event(&id);
        playback.transport = Some(transport);
        playback.transport_status = status.map(str::to_owned);
        dj.on_playback(
            DjSpeakers {
                transport: &NoIo,
                households: &[],
                coordinator: &id,
            },
            &mut MemStore::default(),
            &playback,
            &SystemClock,
        );
        let state = dj.state();
        assert_eq!(state.feeds[&id].snapshot(), before);
        assert!(state.refill_retries.contains_key(&id));
        assert!(state.problems[&id].contains("could not save its queue continuation"));
    }
}
