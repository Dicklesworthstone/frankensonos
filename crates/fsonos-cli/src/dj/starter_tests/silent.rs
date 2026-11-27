//! Silent preparation uses the daemon's durable feed and caller lifecycle.

use super::*;
use fsonos_core::playback::{Playback, PlaybackObservation};
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
        dj.load_ready(rig.at(&lan), &store, None, &clock())
            .unwrap();
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
        assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects, queue);
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
        dj.load_as(
            rig.at(&observed),
            &mut store,
            &clock(),
            &Client::McpStdio,
        )
        .unwrap();
        assert!(observed.checked.get());
        assert_eq!(observation(&rig).transport, Some(TransportState::Paused));
        assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects, queue);
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
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(previous).unwrap()).unwrap();
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
