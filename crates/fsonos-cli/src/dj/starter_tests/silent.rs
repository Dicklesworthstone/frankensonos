//! Silent preparation uses the daemon's durable feed and caller lifecycle.

use super::*;
use fsonos_api::Surface;
use fsonos_api::surface::scenes::SceneDefinition;
use fsonos_core::playback::{Playback, PlaybackObservation};
use fsonos_core::policy::Policy;
use fsonos_types::TransportState;

fn observation(rig: &Rig) -> PlayerPlayback {
    let lan = rig.sim.lan();
    let observed = PlaybackObservation {
        transport: soap::get_transport_info(&lan, rig.host).unwrap(),
        position: soap::get_position_info(&lan, rig.host).unwrap(),
        media: soap::get_media_info(&lan, rig.host).unwrap(),
    };
    let mut playback = Playback::default();
    playback.apply_observation(&rig.coordinator, &observed, Instant::now());
    playback.of(&rig.coordinator).unwrap().clone()
}

#[test]
fn silently_prepared_music_survives_reopen_and_resumes_without_duplicate_tracks() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let lan = rig.sim.lan();
        let mut store = library();
        let dj = rig.engine();
        let before = rig.sim.soap_log().len();
        dj.load_ready(rig.at(&lan), &store, None, &clock()).unwrap();
        assert!(!rig.file.exists());
        assert!(
            rig.sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
        );
        dj.load_as(rig.at(&lan), &mut store, &clock(), &Client::McpStdio)
            .unwrap();
        assert!(
            rig.sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action != "Play")
        );
        let quiet = observation(&rig);
        assert!(matches!(
            quiet.transport,
            Some(TransportState::Paused | TransportState::Stopped)
        ));
        assert!(dj.state().feeds[&rig.coordinator].is_suspended());
        assert_eq!(rig.saved()["feeds"][0]["suspended"], true);
        let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects;
        assert!(!queue.is_empty());
        drop(dj);

        let reopened = rig.engine();
        assert_eq!(
            reopened.starter(&rig.coordinator),
            DjStarter::Known(vec![Client::McpStdio])
        );
        assert!(reopened.state().feeds[&rig.coordinator].is_suspended());
        let before = rig.sim.soap_log().len();
        let mut failed = quiet.clone();
        failed.transport = Some(TransportState::Stopped);
        failed.transport_status = Some("ERROR_OCCURRED".into());
        reopened.on_playback(rig.at(&lan), &mut store, &failed, &clock());
        assert!(
            rig.sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
        );
        assert!(reopened.state().feeds[&rig.coordinator].is_suspended());

        reopened
            .act_as(
                rig.at(&lan),
                &mut store,
                DjAction::Start,
                &clock(),
                &Client::McpStdio,
            )
            .unwrap();
        assert_eq!(observation(&rig).transport, Some(TransportState::Playing));
        assert!(!reopened.state().feeds[&rig.coordinator].is_suspended());
        assert_eq!(
            browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects,
            queue
        );
        assert_eq!(rig.saved()["feeds"][0]["suspended"], false);
    }
}

struct DurableSilence<'a> {
    rig: &'a Rig,
    checked: Cell<bool>,
}

impl Transport for DurableSilence<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            assert_ne!(name, "Play", "silent preparation never starts playback");
            assert_eq!(self.rig.saved()["feeds"][0]["suspended"], true);
            assert_eq!(
                self.rig.saved()["starters"][&self.rig.coordinator.0],
                serde_json::json!(["cli", "mcp-stdio"])
            );
            self.checked.set(true);
        }
        self.rig.sim.lan().soap_post(host, path, action, body)
    }
}

#[test]
fn an_existing_feed_checkpoints_silence_and_both_callers_before_speaker_changes() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let lan = rig.sim.lan();
        let mut store = library();
        let dj = rig.engine();
        dj.act_as(
            rig.at(&lan),
            &mut store,
            DjAction::Start,
            &clock(),
            &Client::Cli,
        )
        .unwrap();
        let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects;
        let selected = soap::get_position_info(&lan, rig.host).unwrap();
        let observed = DurableSilence {
            rig: &rig,
            checked: Cell::new(false),
        };
        dj.load_as(rig.at(&observed), &mut store, &clock(), &Client::McpStdio)
            .unwrap();
        assert!(observed.checked.get());
        assert_eq!(observation(&rig).transport, Some(TransportState::Paused));
        assert_eq!(
            browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects,
            queue
        );
        let after = soap::get_position_info(&lan, rig.host).unwrap();
        assert_eq!(after.track, selected.track);
        assert_eq!(after.uri, selected.uri);
        assert_eq!(after.position_secs, selected.position_secs);
        assert_eq!(
            dj.starter(&rig.coordinator),
            DjStarter::Known(vec![Client::McpStdio])
        );
        drop(dj);
        assert!(rig.engine().state().feeds[&rig.coordinator].is_suspended());
    }
}

#[test]
fn a_failed_silence_checkpoint_never_pauses_or_edits_the_live_feed() {
    let rig = Rig::new(2);
    let lan = rig.sim.lan();
    let mut store = library();
    let dj = rig.engine();
    dj.act_as(
        rig.at(&lan),
        &mut store,
        DjAction::Start,
        &clock(),
        &Client::Cli,
    )
    .unwrap();
    let previous = rig.file.with_extension("before-silence.json");
    std::fs::rename(&rig.file, &previous).unwrap();
    std::fs::create_dir(&rig.file).unwrap();
    let before = rig.sim.soap_log().len();
    let failed = dj
        .load_as(rig.at(&lan), &mut store, &clock(), &Client::McpStdio)
        .unwrap_err();
    assert!(failed.detail.contains("could not be saved"));
    assert_eq!(rig.sim.soap_log().len(), before);
    assert_eq!(observation(&rig).transport, Some(TransportState::Playing));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(previous).unwrap()).unwrap();
    assert_eq!(saved["feeds"][0]["suspended"], false);
}

#[test]
fn silent_preparation_rechecks_the_selected_group_before_any_changes() {
    let rig = Rig::new(2);
    let lan = rig.sim.lan();
    let mut store = library();
    let dj = rig.engine();
    let before = rig.sim.soap_log().len();
    let failed = dj
        .load_as_checked(
            rig.at(&lan),
            &mut store,
            &clock(),
            &Client::McpStdio,
            &|| Err(Failure::invalid("the selected group changed")),
        )
        .unwrap_err();
    assert_eq!(failed.detail, "the selected group changed");
    assert_eq!(rig.sim.soap_log().len(), before);
    assert!(!rig.file.exists());
    assert!(!dj.feeds(&rig.coordinator));
}

#[test]
fn regrouping_scenes_preserve_the_real_djs_retained_occurrence() {
    for generation in [1, 2] {
        for playing in [false, true] {
            let rig = Rig::new(generation);
            let lan = rig.sim.lan();
            let mut store = library();
            let dj = rig.engine();
            dj.act_as(
                rig.at(&lan),
                &mut store,
                DjAction::Start,
                &clock(),
                &Client::Cli,
            )
            .unwrap();
            let current = soap::get_position_info(&lan, rig.host).unwrap().track;
            assert!(current > 0);
            soap::seek_track(&lan, rig.host, current).unwrap();
            soap::seek_position(&lan, rig.host, 37).unwrap();
            let selected = soap::get_position_info(&lan, rig.host).unwrap();
            assert_eq!(selected.position_secs, Some(37));
            let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects;
            let office = PlayerId(rig.sim.player("Office").unwrap().uuid.clone());
            assert_ne!(
                rig.households[0].coordinator_of(&office),
                Some(&rig.coordinator)
            );
            // Keep the survey at the scene's initial state. The core's
            // confirmed post-join households must reach the real DJ engine.
            let cached = rig.households.clone();
            let surface = Surface::new(
                Box::new(rig.sim.lan()),
                Box::new(move |_| Ok(cached.clone())),
                Policy::default(),
                Box::new(clock()),
            )
            .with_action_log(Box::new(store), "scene-dj-regroup-test")
            .with_dj(Box::new(dj));
            let definition: SceneDefinition = serde_json::from_value(serde_json::json!({
                "groups": [{
                    "coordinator": "Den",
                    "members": ["Office"],
                    "source": {"kind": "dj"},
                    "playing": playing
                }]
            }))
            .unwrap();
            surface
                .save_scene_from(&Client::Cli, "Together", &definition)
                .unwrap();
            let before = rig.sim.soap_log().len();
            let applied = surface.apply_scene(&Client::Cli, "Together").unwrap();
            assert!(applied.changed && applied.complete, "{applied:?}");

            let topology = get_zone_group_state(&lan, rig.host).unwrap();
            let joined = topology
                .groups
                .iter()
                .find(|group| group.coordinator == rig.coordinator)
                .unwrap();
            assert!(joined.members.iter().any(|member| member.uuid == office));
            assert_eq!(
                browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects,
                queue
            );
            let after = soap::get_position_info(&lan, rig.host).unwrap();
            assert_eq!(after.track, selected.track);
            assert_eq!(after.uri, selected.uri);
            assert_eq!(after.position_secs, selected.position_secs);
            assert_eq!(
                observation(&rig).transport,
                Some(if playing {
                    TransportState::Playing
                } else {
                    TransportState::Paused
                })
            );
            assert_eq!(rig.saved()["feeds"][0]["suspended"], !playing);
            let calls = rig.sim.soap_log();
            let calls = &calls[before..];
            assert_eq!(
                calls.iter().filter(|call| call.action == "Play").count(),
                usize::from(playing),
                "{calls:?}"
            );
            assert!(
                !calls.iter().any(|call| {
                    call.room == "Den"
                        && matches!(
                            call.action.as_str(),
                            "SetAVTransportURI"
                                | "Seek"
                                | "AddURIToQueue"
                                | "RemoveAllTracksFromQueue"
                        )
                }),
                "the retained coordinator only resumes or pauses its selected music: {calls:?}"
            );
        }
    }
}
