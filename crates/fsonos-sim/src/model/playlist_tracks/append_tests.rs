use super::*;
use crate::docs::{AV_TRANSPORT, CONTENT_DIRECTORY};
use crate::model::{SONOS_FAILURE, Source, TransportState};
use crate::{SimClock, SimModel};
use fsonos_proto::didl::{DidlObject, item_metadata};

fn state(generation: u8) -> State {
    let mut state = State::new(vec![crate::household(generation)], SimClock::default());
    state.add_player(
        "Synthetic room",
        if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        },
        0,
    );
    let transport = &mut state.players[0].transport;
    transport.queue = state.households[0].saved_playlists[0].tracks.clone();
    transport.source = Source::Queue;
    transport.state = TransportState::PausedPlayback;
    transport.track = 2;
    transport.position_ms = 23_000;
    transport.queue_update_id = 71;
    transport.play_mode = "SHUFFLE_REPEAT_ONE".into();
    transport.sleep_at = Some(900_000);
    state
}

fn get(out: &Out, name: &str) -> String {
    out.iter().find(|(key, _)| *key == name).unwrap().1.clone()
}

fn contents(state: &mut State, id: &str) -> (Vec<DidlObject>, u32) {
    let args: Vec<_> = [
        ("ObjectID", id),
        ("BrowseFlag", "BrowseDirectChildren"),
        ("StartingIndex", "0"),
        ("RequestedCount", "100"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    let out = state
        .invoke(0, &CONTENT_DIRECTORY, "Browse", &Args(&args))
        .unwrap();
    (
        parse_didl(&get(&out, "Result")).unwrap(),
        get(&out, "UpdateID").parse().unwrap(),
    )
}

fn append_args(id: &str, revision: u32, track: &DidlObject) -> Vec<(String, String)> {
    [
        ("InstanceID", "0".into()),
        ("ObjectID", id.into()),
        ("UpdateID", revision.to_string()),
        ("EnqueuedURI", track.res.as_ref().unwrap().uri.clone()),
        ("EnqueuedURIMetaData", item_metadata(track)),
        ("AddAtIndex", u32::MAX.to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect()
}

fn append(state: &mut State, args: &[(String, String)]) -> Result<Out, Fault> {
    state.invoke(0, &AV_TRANSPORT, "AddURIToSavedQueue", &Args(args))
}

#[test]
fn saved_append_preserves_duplicates_metadata_and_every_players_playback() {
    for generation in [1, 2] {
        for empty in [false, true] {
            let mut state = state(generation);
            let source = contents(&mut state, "SQ:1").0.remove(0);
            if empty {
                state.households[0].saved_playlists[1].tracks.clear();
            }
            let (before, mut revision) = contents(&mut state, "SQ:7");
            let players = format!("{:?}", state.players);
            let original = format!("{:?}", state.households[0].saved_playlists[0]);
            let favorites = format!("{:?}", state.households[0].favorites);
            for added in 1..=2 {
                let reply = append(&mut state, &append_args("SQ:7", revision, &source)).unwrap();
                assert_eq!(get(&reply, "NumTracksAdded"), "1");
                assert_eq!(
                    get(&reply, "NewQueueLength"),
                    (before.len() + added).to_string()
                );
                revision += 1;
                assert_eq!(get(&reply, "NewUpdateID"), revision.to_string());
                let saved = &state.households[0].saved_playlists[1].tracks;
                assert_eq!(saved.last().unwrap().metadata, item_metadata(&source));
                let (current, observed_revision) = contents(&mut state, "SQ:7");
                assert_eq!(observed_revision, revision);
                assert_eq!(&current[..before.len()], before.as_slice());
                for (index, actual) in current[before.len()..].iter().enumerate() {
                    let mut expected = source.clone();
                    expected.id = format!("SQ:7/{}", before.len() + index + 1);
                    expected.parent_id = "SQ:7".into();
                    assert_eq!(*actual, expected);
                }
                assert_eq!(format!("{:?}", state.players), players);
                assert_eq!(
                    format!("{:?}", state.households[0].saved_playlists[0]),
                    original
                );
                assert_eq!(format!("{:?}", state.households[0].favorites), favorites);
            }
        }
    }
}

#[test]
fn stale_saved_revision_and_invalid_native_arguments_leave_both_queues_unchanged() {
    for generation in [1, 2] {
        let mut state = state(generation);
        let track = contents(&mut state, "SQ:1").0.remove(0);
        for (key, value, fault) in [
            ("UpdateID", "2", STALE_UPDATE_ID),
            ("ObjectID", "SQ:404", TRANSITION_NOT_AVAILABLE),
            ("ObjectID", "Q:0", INVALID_ARGS),
            ("AddAtIndex", "0", INVALID_ARGS),
            ("InstanceID", "1", INVALID_ARGS),
            ("EnqueuedURI", "", INVALID_ARGS),
            (
                "EnqueuedURI",
                "https://example.test/another.mp3",
                INVALID_ARGS,
            ),
            ("EnqueuedURIMetaData", "<DIDL-Lite/>", INVALID_ARGS),
            ("EnqueuedURIMetaData", "<DIDL-Lite>", INVALID_ARGS),
        ] {
            let players = format!("{:?}", state.players);
            let household = format!("{:?}", state.households);
            let mut args = append_args("SQ:7", 1, &track);
            args.iter_mut().find(|(name, _)| name == key).unwrap().1 = value.into();
            assert_eq!(append(&mut state, &args), Err(fault));
            assert_eq!(format!("{:?}", state.players), players);
            assert_eq!(format!("{:?}", state.households), household);
        }
        let mut foreign = track;
        foreign.desc.as_mut().unwrap().value = "SYNTHETIC_OTHER_HOUSEHOLD".into();
        assert_eq!(
            append(&mut state, &append_args("SQ:7", 1, &foreign)),
            Err(SONOS_FAILURE)
        );
    }
}

#[test]
fn native_queue_metadata_can_be_copied_without_inventing_a_service_descriptor() {
    for generation in [1, 2] {
        let mut state = state(generation);
        let source = contents(&mut state, "Q:0").0.remove(0);
        assert!(
            source.desc.is_none(),
            "native queue reads omit the descriptor"
        );
        let players = format!("{:?}", state.players);
        let reply = append(&mut state, &append_args("SQ:7", 1, &source)).unwrap();
        assert_eq!(get(&reply, "NumTracksAdded"), "1");
        let copied = contents(&mut state, "SQ:7").0.pop().unwrap();
        let mut expected = source.clone();
        expected.id = "SQ:7/4".into();
        expected.parent_id = "SQ:7".into();
        assert_eq!(copied, expected);
        assert_eq!(format!("{:?}", state.players), players);

        let mut foreign = source;
        let params = &state.households[0].params;
        let resource = foreign.res.as_mut().unwrap();
        resource.uri = resource
            .uri
            .replace(&format!("sn={}", params.sn), "sn=4294967295");
        let original = format!("{:?}", state.households);
        assert_eq!(
            append(&mut state, &append_args("SQ:7", 2, &foreign)),
            Err(SONOS_FAILURE)
        );
        assert_eq!(format!("{:?}", state.households), original);
    }
}

#[test]
fn native_revision_wildcard_and_wrap_remain_distinct_from_controller_preflight() {
    let mut state = state(2);
    let track = contents(&mut state, "SQ:1").0.remove(0);
    state.households[0].saved_playlists_update_id = u32::MAX;
    let reply = append(&mut state, &append_args("SQ:7", u32::MAX, &track)).unwrap();
    assert_eq!(get(&reply, "NewUpdateID"), "0");
    let reply = append(&mut state, &append_args("SQ:7", 0, &track)).unwrap();
    assert_eq!(get(&reply, "NewUpdateID"), "1");
}
