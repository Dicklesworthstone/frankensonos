//! Authored selections use household renderers without a Spotify catalog session.

use super::*;
use fsonos_proto::play_mode::set_play_mode;

const TRACK: &str = "spotify:track:0000000000000000000001";
const EPISODE: &str = "spotify:episode:0000000000000000000002";

fn source() -> Value {
    json!({"kind":"tracks", "tracks":[
        {"source_uri":"https://open.spotify.com/track/0000000000000000000001?si=scene", "title":"  Opening  "},
        {"source_uri":EPISODE, "title":"Interlude"},
        {"source_uri":TRACK, "title":"Reprise"}
    ]})
}

fn expected() -> Vec<(String, String)> {
    [
        (TRACK, "Opening"),
        (EPISODE, "Interlude"),
        (TRACK, "Reprise"),
    ]
    .into_iter()
    .map(|(uri, title)| (uri.into(), title.into()))
    .collect()
}

fn definition(playing: bool) -> SceneDefinition {
    serde_json::from_value(json!({
        "groups": [
            {"coordinator":"Kitchen", "members":["Office"], "source":source(), "playing":playing},
            {"coordinator":"Den", "members":["Study"], "source":source(), "playing":playing}
        ],
        "volumes":{"Kitchen":20, "Office":15, "Den":20, "Study":15}
    }))
    .unwrap()
}

#[test]
fn authored_scenes_keep_order_duplicates_and_local_renderers_on_both_generations() {
    for playing in [true, false] {
        let rig = Rig::new();
        rig.seed();
        let catalog = Catalog::ready();
        catalog.ready.store(false, Ordering::SeqCst);
        let calls = Arc::clone(&catalog.calls);
        let surface = rig.surface(capped(), catalog);
        let saved = surface
            .save_scene_from(&Client::Cli, "Selection", &definition(playing))
            .unwrap();
        assert_eq!(surface.scene(&Client::Cli, "selection").unwrap(), saved);
        let selected = saved.groups[0].source.tracks.as_ref().unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|track| (track.source_uri.clone(), track.title.clone().unwrap()))
                .collect::<Vec<_>>(),
            expected()
        );
        let marker = rig.sim.soap_log().len();
        let result = surface.apply_scene(&Client::McpStdio, "Selection").unwrap();
        assert!(result.complete && result.changed, "{result:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "concrete identities need no catalog account"
        );
        let writes = rig.writes(marker);
        assert_eq!(
            writes
                .iter()
                .filter(|entry| entry.action == "AddURIToQueue")
                .count(),
            6
        );
        assert_eq!(
            writes.iter().filter(|entry| entry.action == "Play").count(),
            if playing { 2 } else { 0 }
        );
        assert!(
            writes
                .iter()
                .all(|entry| entry.action != "RemoveAllTracksFromQueue")
        );
        for room in ["Kitchen", "Den"] {
            let player = rig.sim.player(room).unwrap();
            assert_eq!(rig.queue(room), expected());
            assert_eq!(
                get_play_mode(&rig.sim.lan(), player.ip).unwrap(),
                PlayMode::Normal
            );
            assert_eq!(
                soap::get_position_info(&rig.sim.lan(), player.ip)
                    .unwrap()
                    .track,
                1
            );
            assert_eq!(
                soap::get_transport_info(&rig.sim.lan(), player.ip)
                    .unwrap()
                    .state
                    == TransportState::Playing,
                playing
            );
            let params =
                learn_spotify_params(&browse_all(&rig.sim.lan(), player.ip, "FV:2").unwrap())
                    .unwrap();
            for entry in writes
                .iter()
                .filter(|entry| entry.player == player.uuid && entry.action == "AddURIToQueue")
            {
                let uri = &entry
                    .args
                    .iter()
                    .find(|(key, _)| key == "EnqueuedURI")
                    .unwrap()
                    .1;
                let metadata = &entry
                    .args
                    .iter()
                    .find(|(key, _)| key == "EnqueuedURIMetaData")
                    .unwrap()
                    .1;
                if uri.contains("episode") {
                    assert!(uri.ends_with(&format!("?sid={}", params.sid)));
                    assert!(metadata.contains("object.item.audioItem.musicTrack"));
                } else {
                    assert!(metadata.contains(&params.cdudn));
                }
            }
            soap::add_uri_to_queue(
                &rig.sim.lan(),
                player.ip,
                "http://example.invalid/later.mp3",
                "",
                false,
            )
            .unwrap();
        }
        let recalled = surface.apply_scene(&Client::Cli, "Selection").unwrap();
        assert!(recalled.complete && recalled.changed, "{recalled:?}");
        for room in ["Kitchen", "Den"] {
            assert_eq!(
                rig.queue(room),
                expected(),
                "each recall reconstructs the authored order"
            );
        }
    }
}

#[test]
fn silent_track_recall_has_a_complete_queue_and_playback_undo_on_unchanged_groups() {
    for room in ["Kitchen", "Den"] {
        for originally_playing in [true, false] {
            let rig = Rig::new();
            rig.seed();
            let player = rig.sim.player(room).unwrap();
            let lan = rig.sim.lan();
            for name in ["repeated", "repeated", "last"] {
                soap::add_uri_to_queue(
                    &lan,
                    player.ip,
                    &format!("http://example.invalid/{name}.mp3"),
                    "",
                    false,
                )
                .unwrap();
            }
            soap::play_from_queue(&lan, player.ip, &PlayerId(player.uuid.clone())).unwrap();
            set_play_mode(&lan, player.ip, PlayMode::RepeatAll).unwrap();
            soap::seek_track(&lan, player.ip, 2).unwrap();
            soap::play(&lan, player.ip).unwrap();
            rig.sim.advance(Duration::from_secs(17));
            if !originally_playing {
                soap::pause(&lan, player.ip).unwrap();
            }
            let before_queue = browse_all(&lan, player.ip, QUEUE).unwrap();
            let before_media = soap::get_media_info(&lan, player.ip).unwrap();
            let before_position = soap::get_position_info(&lan, player.ip).unwrap();
            let before_transport = soap::get_transport_info(&lan, player.ip).unwrap();
            let surface = rig.surface(Policy::default(), Catalog::ready());
            let scene: SceneDefinition = serde_json::from_value(json!({
                "groups":[{"coordinator":room, "source":source(), "playing":false}],
                "volumes":{(room):20}
            }))
            .unwrap();
            surface
                .save_scene_from(&Client::Cli, "Quiet", &scene)
                .unwrap();
            let marker = rig.sim.soap_log().len();
            let applied = surface.apply_scene(&Client::McpStdio, "Quiet").unwrap();
            assert!(applied.complete && applied.changed, "{applied:?}");
            assert_eq!(rig.queue(room), expected());
            assert!(
                rig.writes(marker)
                    .iter()
                    .all(|entry| entry.action != "Play")
            );
            let undo = surface
                .undo(&Client::McpStdio, false)
                .unwrap()
                .expect("verified scene replacement undo");
            assert!(undo.failures.is_empty(), "{undo:?}");
            assert_eq!(browse_all(&lan, player.ip, QUEUE).unwrap(), before_queue);
            assert_eq!(soap::get_media_info(&lan, player.ip).unwrap(), before_media);
            assert_eq!(
                soap::get_position_info(&lan, player.ip).unwrap(),
                before_position
            );
            assert_eq!(
                soap::get_transport_info(&lan, player.ip).unwrap(),
                before_transport
            );
            assert_eq!(get_play_mode(&lan, player.ip).unwrap(), PlayMode::RepeatAll);
            assert_eq!(soap::get_volume(&lan, player.ip).unwrap(), 85);
        }
    }
}

#[test]
fn either_caller_or_writer_can_refuse_play_or_silent_pause_without_queue_writes() {
    for permission in ["play", "pause"] {
        for denied_writer in [true, false] {
            let rig = Rig::new();
            rig.seed();
            let policy = Policy::from_toml(&format!(
                "[clients.\"tag:blocked\"]\ndeny = ['{permission}']\n"
            ))
            .unwrap();
            let surface = rig.surface(policy, Catalog::ready());
            let blocked = Client::Tailnet("tag:blocked".into());
            let cli = Client::Cli;
            let (writer, caller) = if denied_writer {
                (&blocked, &cli)
            } else {
                (&cli, &blocked)
            };
            surface
                .save_scene_from(writer, "Quiet", &definition(false))
                .unwrap();
            let marker = rig.sim.soap_log().len();
            let applied = surface.apply_scene(caller, "Quiet").unwrap();
            assert!(applied.complete && applied.changed, "{applied:?}");
            assert!(
                applied
                    .notes
                    .iter()
                    .any(|note| note.code == NoteCode::Skipped)
            );
            assert!(rig.writes(marker).iter().all(|entry| !matches!(
                entry.action.as_str(),
                "Play" | "Pause" | "Stop" | "AddURIToQueue" | "RemoveTrackRangeFromQueue"
            )));
            assert_eq!(
                soap::get_volume(&rig.sim.lan(), rig.sim.player("Den").unwrap().ip).unwrap(),
                20
            );
        }
    }
}

#[test]
fn writer_and_caller_caps_cover_every_future_member_before_authored_playback() {
    let rig = Rig::new();
    rig.seed();
    let policy = Policy::from_toml(
        "[defaults]\nmax_volume = 25\n[clients.\"tag:writer\"]\nmax_volume = 12\n",
    )
    .unwrap();
    let surface = rig.surface(policy, Catalog::ready());
    surface
        .save_scene_from(
            &Client::Tailnet("tag:writer".into()),
            "Quiet",
            &definition(true),
        )
        .unwrap();
    let marker = rig.sim.soap_log().len();
    let applied = surface.apply_scene(&Client::Cli, "Quiet").unwrap();
    assert!(applied.complete && applied.changed, "{applied:?}");
    let writes = rig.writes(marker);
    let first_play = writes
        .iter()
        .position(|entry| entry.action == "Play")
        .unwrap();
    for room in ["Kitchen", "Office", "Den", "Study"] {
        let player = rig.sim.player(room).unwrap();
        assert_eq!(soap::get_volume(&rig.sim.lan(), player.ip).unwrap(), 12);
        assert!(
            writes[..first_play]
                .iter()
                .any(|entry| entry.player == player.uuid && entry.action == "SetVolume")
        );
    }
}

#[test]
fn malformed_authored_sources_and_missing_renderer_refuse_before_any_write() {
    let rig = Rig::new();
    rig.seed();
    let surface = rig.surface(capped(), Catalog::ready());
    let mut malformed = definition(true);
    malformed.groups[1]
        .source
        .as_mut()
        .unwrap()
        .tracks
        .as_mut()
        .unwrap()[1]
        .source_uri = "spotify:episode:short".into();
    let marker = rig.sim.soap_log().len();
    assert_eq!(
        surface
            .save_scene_from(&Client::Cli, "Invalid", &malformed)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        rig.sim.soap_log().len(),
        marker,
        "malformed selection must fail before discovery"
    );
    surface
        .save_scene_from(&Client::Cli, "Selection", &definition(true))
        .unwrap();
    rig.sim
        .retain_favorites(2, |uri| !uri.contains("spotify"))
        .unwrap();
    let marker = rig.sim.soap_log().len();
    assert_eq!(
        surface
            .apply_scene(&Client::McpStdio, "Selection")
            .unwrap_err()
            .code,
        ErrorCode::RenderParamsMissing
    );
    assert!(rig.writes(marker).is_empty());
    for room in ["Kitchen", "Office", "Den", "Study"] {
        let host = rig.sim.player(room).unwrap().ip;
        assert_eq!(soap::get_volume(&rig.sim.lan(), host).unwrap(), 85);
        assert_eq!(
            soap::get_media_info(&rig.sim.lan(), host).unwrap().uri,
            ORIGINAL
        );
    }
}

#[test]
fn authored_recall_retains_original_queue_and_source_proofs_through_caps_and_regrouping() {
    for layout in ["unchanged", "join", "future"] {
        for change in ["queue", "source"] {
            if layout == "future" && change == "source" {
                continue;
            }
            let rig = Rig::new();
            rig.seed();
            let kitchen = rig.sim.player("Kitchen").unwrap();
            if layout == "future" {
                soap::join_group(
                    &rig.sim.lan(),
                    rig.sim.player("Office").unwrap().ip,
                    &PlayerId(kitchen.uuid.clone()),
                )
                .unwrap();
            }
            let coordinator = if layout == "future" {
                "Office"
            } else {
                "Kitchen"
            };
            let target = rig.sim.player(coordinator).unwrap().ip;
            let members = match layout {
                "join" => vec!["Office"],
                "future" => vec!["Kitchen"],
                _ => Vec::new(),
            };
            let surface = rig.surface_over(
                capped(),
                Catalog::ready(),
                Box::new(ChangeDuringVolume {
                    lan: rig.sim.lan(),
                    once: AtomicBool::new(false),
                    target,
                    change,
                }),
            );
            let scene: SceneDefinition = serde_json::from_value(json!({
                "groups":[{"coordinator":coordinator, "members":members, "source":source(), "playing":true}],
                "volumes":{(coordinator):20}
            })).unwrap();
            surface
                .save_scene_from(&Client::Cli, "Music", &scene)
                .unwrap();
            let marker = rig.sim.soap_log().len();
            let applied = surface.apply_scene(&Client::Cli, "Music").unwrap();
            assert!(!applied.complete, "{layout} {change}: {applied:?}");
            assert!(
                applied
                    .failed
                    .iter()
                    .any(|failure| failure.error.contains("changed")),
                "{applied:?}"
            );
            let writes = rig.writes(marker);
            assert!(
                writes
                    .iter()
                    .all(|entry| !matches!(entry.action.as_str(), "Play" | "Pause" | "Stop"))
            );
            assert_eq!(
                writes
                    .iter()
                    .filter(|entry| entry.action == "AddURIToQueue")
                    .count(),
                usize::from(change == "queue")
            );
            if change == "queue" {
                assert_eq!(
                    browse_all(&rig.sim.lan(), target, QUEUE)
                        .unwrap()
                        .last()
                        .unwrap()
                        .res
                        .as_ref()
                        .unwrap()
                        .uri,
                    "http://example.invalid/after-cap.mp3"
                );
            } else {
                assert_eq!(
                    soap::get_media_info(&rig.sim.lan(), target).unwrap().uri,
                    "http://example.invalid/after-cap.mp3"
                );
            }
        }
    }
}

struct JoinDuringDestination {
    lan: SimLan,
    future_host: IpAddr,
    future_id: PlayerId,
    unexpected_host: IpAddr,
    regrouped: AtomicBool,
    joined: Arc<AtomicBool>,
}

impl Transport for JoinDuringDestination {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.lan.soap_post(host, path, action, body);
        if host == self.future_host
            && action.contains("#SetAVTransportURI")
            && body.contains(&format!("x-rincon:{}", self.future_id.0))
        {
            // A member's self-join can return 402 after successfully leaving.
            self.regrouped.store(true, Ordering::SeqCst);
        }
        if host == self.future_host
            && self.regrouped.load(Ordering::SeqCst)
            && action.contains("#Browse")
            && body.contains("<ObjectID>Q:0</ObjectID>")
            && !self.joined.swap(true, Ordering::SeqCst)
        {
            // The first local-queue confirmation follows the execution
            // engine's audience check and precedes its fresh bound snapshot.
            soap::join_group(&self.lan, self.unexpected_host, &self.future_id)?;
        }
        answer
    }
}

#[test]
fn a_new_audience_during_regrouped_destination_capture_cannot_start_music() {
    for (old_room, future_room) in [("Kitchen", "Office"), ("Den", "Study")] {
        for kind in ["tracks", "artist"] {
            let rig = Rig::new();
            rig.seed();
            let old = rig.sim.player(old_room).unwrap();
            let future = rig.sim.player(future_room).unwrap();
            soap::join_group(&rig.sim.lan(), future.ip, &PlayerId(old.uuid.clone())).unwrap();
            let joined = Arc::new(AtomicBool::new(false));
            let surface = rig.surface_over(
                capped(),
                Catalog::ready(),
                Box::new(JoinDuringDestination {
                    lan: rig.sim.lan(),
                    future_host: future.ip,
                    future_id: PlayerId(future.uuid.clone()),
                    unexpected_host: old.ip,
                    regrouped: AtomicBool::new(false),
                    joined: Arc::clone(&joined),
                }),
            );
            let selected = if kind == "tracks" {
                source()
            } else {
                super::source(kind)
            };
            let scene: SceneDefinition = serde_json::from_value(json!({
                "groups":[{"coordinator":future_room, "source":selected, "playing":true}],
                "volumes":{(future_room):20}
            }))
            .unwrap();
            surface
                .save_scene_from(&Client::Cli, "Solo", &scene)
                .unwrap();
            let marker = rig.sim.soap_log().len();
            let applied = surface.apply_scene(&Client::Cli, "Solo").unwrap();
            assert!(
                joined.load(Ordering::SeqCst),
                "the synthetic join must occur during destination preparation"
            );
            assert!(!applied.complete, "{kind} {future_room}: {applied:?}");
            assert!(
                applied
                    .failed
                    .iter()
                    .any(|failure| failure.error.contains("changed")),
                "{applied:?}"
            );
            assert!(rig.writes(marker).iter().all(|entry| !matches!(
                entry.action.as_str(),
                "Play" | "Pause" | "Stop" | "AddURIToQueue"
            )));
        }
    }
}

struct RendererGate {
    lan: SimLan,
    host: IpAddr,
    unavailable: Arc<AtomicBool>,
}

impl Transport for RendererGate {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.lan.soap_post(host, path, action, body)?;
        if host == self.host
            && action.contains("#Browse")
            && body.contains("<ObjectID>FV:2</ObjectID>")
            && self.unavailable.load(Ordering::SeqCst)
        {
            return Err(ProtoError::Network {
                target: "synthetic renderer catalog".into(),
                detail: "temporarily unavailable".into(),
            });
        }
        Ok(answer)
    }
}

#[test]
fn scheduled_track_scenes_retry_local_readiness_then_load_the_selection_once() {
    let rig = Rig::new();
    rig.seed();
    let unavailable = Arc::new(AtomicBool::new(false));
    let catalog = Catalog::ready();
    catalog.ready.store(false, Ordering::SeqCst);
    let calls = Arc::clone(&catalog.calls);
    let surface = Arc::new(rig.surface_over(
        capped(),
        catalog,
        Box::new(RendererGate {
            lan: rig.sim.lan(),
            host: rig.sim.player("Den").unwrap().ip,
            unavailable: Arc::clone(&unavailable),
        }),
    ));
    surface
        .save_scene_from(&Client::Cli, "Morning", &definition(false))
        .unwrap();
    let saved = surface
        .add_schedule(
            &Client::Cli,
            &ScheduleRequest {
                when: "in 1m".into(),
                action: "apply_scene".into(),
                scene: Some("Morning".into()),
                ..ScheduleRequest::default()
            },
        )
        .unwrap();
    let at = DateTime::parse_from_rfc3339(saved.next.as_deref().unwrap())
        .unwrap()
        .to_utc();
    let scheduler = Scheduler::new(&surface);
    unavailable.store(true, Ordering::SeqCst);
    let marker = rig.sim.soap_log().len();
    assert_eq!(
        scheduler.tick_at(at, &Utc),
        [Ran::Fired { id: saved.id, at }]
    );
    assert!(scheduler.wait(Duration::from_secs(10)));
    assert!(
        surface.schedules(&Client::Cli).unwrap()[0]
            .last_fired
            .is_none()
    );
    assert!(rig.writes(marker).is_empty());
    unavailable.store(false, Ordering::SeqCst);
    assert_eq!(
        scheduler.tick_at(at + chrono::TimeDelta::minutes(1), &Utc),
        [Ran::Fired { id: saved.id, at }]
    );
    assert!(scheduler.wait(Duration::from_secs(10)));
    assert!(
        surface.schedules(&Client::Cli).unwrap()[0]
            .last_fired
            .is_some()
    );
    assert!(
        scheduler
            .tick_at(at + chrono::TimeDelta::minutes(2), &Utc)
            .is_empty()
    );
    for room in ["Kitchen", "Den"] {
        assert_eq!(rig.queue(room), expected());
    }
    let writes = rig.writes(marker);
    assert_eq!(
        writes
            .iter()
            .filter(|entry| entry.action == "AddURIToQueue")
            .count(),
        6
    );
    assert!(writes.iter().all(|entry| entry.action != "Play"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failed_group_makes_the_whole_scene_an_undo_barrier() {
    let rig = Rig::new();
    let surface = rig.surface(Policy::default(), Catalog::ready());
    let scene: SceneDefinition = serde_json::from_value(json!({
        "groups":[{"coordinator":"Kitchen", "source":source()}, {"coordinator":"Den", "source":source()}]
    })).unwrap();
    surface
        .save_scene_from(&Client::Cli, "Selection", &scene)
        .unwrap();
    rig.sim.upnp_fault("Den", "AddURIToQueue", 701).unwrap();
    let applied = surface.apply_scene(&Client::Cli, "Selection").unwrap();
    assert!(applied.changed && !applied.complete, "{applied:?}");
    assert_eq!(rig.queue("Kitchen"), expected());
    let marker = rig.sim.soap_log().len();
    let refused = surface.undo(&Client::Cli, true).unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidArgument);
    assert!(refused.detail.contains("restoration proof"));
    assert!(rig.writes(marker).is_empty());
}
