//! The scene tools: `list_scenes`, `save_scene` and `apply_scene`, over the
//! surface's scenes (`fsonos_api::surface::scenes`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::scenes::{
    SceneApplyDto, SceneApplyRequest, SceneDefinition, SceneDeletedDto, SceneDto,
};
use serde::{Deserialize as _, Serialize};

use super::{Backend, respond, segment, with_backend_off_thread};

/// `list_scenes` structured content (MCP wants an object).
#[derive(Serialize)]
struct ScenesDto {
    scenes: Vec<SceneDto>,
}

/// A scene in a line: its groups, what each plays.
fn scene_line(scene: &SceneDto) -> String {
    let groups: Vec<String> = scene
        .groups
        .iter()
        .map(|g| {
            let rooms = std::iter::once(g.coordinator.as_str())
                .chain(g.members.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" + ");
            let source = &g.source;
            let what = match source.kind.as_str() {
                "favorite" => format!("favorite {}", source.name.as_deref().unwrap_or("?")),
                "uri" => source.uri.clone().unwrap_or_default(),
                "dj" if source.direction.is_some() => {
                    let direction = source.direction.as_ref().expect("matched direction");
                    if source.mood.is_none() && direction.constraints.is_empty() {
                        "the DJ (automatic program; clears current steering)".into()
                    } else {
                        format!("the DJ ({})", direction.summary(source.mood.as_deref()))
                    }
                }
                "dj" => source
                    .mood
                    .as_ref()
                    .map_or_else(|| "the DJ".to_string(), |m| format!("the DJ ({m})")),
                "queue" | "queue_backup" => source.queue.as_ref().map_or_else(
                    || "queue reference unavailable; capture the scene again".into(),
                    fsonos_api::surface::scenes::SceneQueueDto::text,
                ),
                _ => "what it has".to_string(),
            };
            let state = match g.playing {
                Some(true) => "playing",
                Some(false) if matches!(source.kind.as_str(), "queue" | "queue_backup") => {
                    "ready without playback"
                }
                Some(false) => "paused",
                None => "playing or not as it is",
            };
            format!("{rooms}: {what}, {state}")
        })
        .collect();
    format!("{}: {}", scene.name, groups.join("; "))
}

/// The apply in words: what was done, each step, and what failed.
fn apply_text(applied: &SceneApplyDto) -> String {
    let mut lines = vec![applied.done.clone()];
    lines.extend(applied.steps.iter().map(|s| format!("- {s}")));
    lines.extend(
        applied
            .failed
            .iter()
            .map(|f| format!("- failed: {}: {}", f.step, f.error)),
    );
    lines.extend(applied.notes.iter().map(|n| format!("Note: {}.", n.detail)));
    lines.join("\n")
}

impl Backend {
    /// The `list_scenes` tool.
    pub fn list_scenes(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let scenes = self.read("/scenes", || self.surface.scenes(&self.client))?;
            let text = if scenes.is_empty() {
                "No scenes saved yet; save one with save_scene.".to_string()
            } else {
                scenes.iter().map(scene_line).collect::<Vec<_>>().join("\n")
            };
            Ok((text, ScenesDto { scenes }))
        })
    }

    /// The `save_scene` tool: on the daemon when one runs, whose DJ is the
    /// one a group plays (saved as the DJ, not as what it has), else here.
    pub fn save_scene(&self, name: &str) -> McpResult<FinalCallToolResult> {
        self.capture_scene(name, false)
    }

    /// Capture complete ordinary queues for durable scene recall.
    pub fn save_scene_with_queue_backups(&self, name: &str) -> McpResult<FinalCallToolResult> {
        self.capture_scene(name, true)
    }

    fn capture_scene(&self, name: &str, backup_queues: bool) -> McpResult<FinalCallToolResult> {
        let path = format!("/scenes/{}", segment(name));
        let request = backup_queues.then(|| serde_json::json!({"backup_queues": true}));
        if let Some(answer) = self.on_daemon("PUT", &path, request.as_ref(), |scene: &SceneDto| {
            format!("Saved {}", scene_line(scene))
        }) {
            return answer;
        }
        respond(|| {
            let scene = if backup_queues {
                self.surface
                    .save_scene_with_queue_backups(&self.client, name)?
            } else {
                self.surface.save_scene(&self.client, name)?
            };
            Ok((format!("Saved {}", scene_line(&scene)), scene))
        })
    }

    /// The `save_scene` tool with a `scene` written out.
    pub fn save_scene_from(
        &self,
        name: &str,
        scene: &serde_json::Value,
    ) -> McpResult<FinalCallToolResult> {
        let def = SceneDefinition::deserialize(scene).map_err(|error| {
            crate::tool_error(&fsonos_api::Failure::invalid(format!("scene: {error}")))
        })?;
        if def == SceneDefinition::default() {
            return Err(crate::tool_error(
                &fsonos_api::Failure::invalid(
                    "the scene written out names no group, volume or mute",
                )
                .with_hint("Name at least one, or leave the scene out to save the house as it is."),
            ));
        }
        def.validate_sources()
            .map_err(|error| crate::tool_error(&error))?;
        let path = format!("/scenes/{}", segment(name));
        if let Some(answer) = self.on_daemon("PUT", &path, Some(scene), |saved: &SceneDto| {
            format!("Saved {}", scene_line(saved))
        }) {
            return answer;
        }
        respond(|| {
            let saved = self.surface.save_scene_from(&self.client, name, &def)?;
            Ok((format!("Saved {}", scene_line(&saved)), saved))
        })
    }

    /// The `apply_scene` tool.
    pub fn apply_scene(
        &self,
        name: &str,
        req: &SceneApplyRequest,
    ) -> McpResult<FinalCallToolResult> {
        // A scene that starts the DJ needs the daemon's DJ to keep it going.
        let path = format!("/scenes/{}/apply", segment(name));
        let body = serde_json::to_value(req).unwrap_or_default();
        if let Some(answer) = self.on_daemon("POST", &path, Some(&body), apply_text) {
            return answer;
        }
        respond(|| {
            let applied = self.surface.apply_scene_with(&self.client, name, req)?;
            Ok((apply_text(&applied), applied))
        })
    }

    /// The `get_scene` tool.
    pub fn get_scene(&self, name: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = format!("/scenes/{}", segment(name));
            let scene = self.read(&path, || self.surface.scene(&self.client, name))?;
            Ok((scene_line(&scene), scene))
        })
    }

    /// The `delete_scene` tool.
    pub fn delete_scene(&self, name: &str) -> McpResult<FinalCallToolResult> {
        let path = format!("/scenes/{}", segment(name));
        if let Some(answer) = self.on_daemon("DELETE", &path, None, |deleted: &SceneDeletedDto| {
            deleted.done.clone()
        }) {
            return answer;
        }
        respond(|| {
            let deleted = self.surface.delete_scene(&self.client, name)?;
            Ok((deleted.done.clone(), deleted))
        })
    }
}

#[tool(
    description = "List the saved scenes: named house states, each with how the rooms group, what each group plays (a favorite, a URI, the DJ with a saved mood and full direction, a captured queue position, or whatever it has) and whether it plays, and each room's volume and mute. A queue reference requires the original unchanged queue. A queue_backup retains complete contents and rebuilds the original player's queue on recall. Put the house in a scene with apply_scene.",
    annotations(read_only, idempotent)
)]
async fn list_scenes(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::list_scenes).await
}

#[tool(
    description = "Save a scene called `name` (1-64 characters): the house as it is now (how the rooms group, each room's volume and mute, what each group plays and whether it plays), or, with `scene`, the one it writes out: {\"groups\": [{\"coordinator\": room, \"members\": [rooms], \"source\": {\"kind\": \"favorite\", \"name\": title} (or kind dj with optional mood and direction {constraints: {include_artists: [names], include_genres: [genres], ...}, for_secs: 7200}, uri with a stream uri or a Spotify track, album, playlist, artist, episode or show link, keep), \"playing\": bool}], \"volumes\": {room: 0-100}, \"mutes\": {room: bool}}. A captured DJ retains its added filters and remaining duration, renewed on recall; an explicit empty direction without a mood clears prior steering. All steering constraint fields supported by add_schedule are accepted. Capturing an ordinary queue saves its selected occurrence, elapsed time and known play mode as a bookmark by default. With backup_queues true, complete ordered contents are retained (up to 1000 entries and 4 MiB per queue) and recall can rebuild the original player's queue after edits or a restart. Recall replaces that player's current queue; undo_last cannot recover the replaced contents. Capture fails if a queue cannot be saved completely. The option cannot accompany scene. Captured queue sources cannot be authored; omit scene to capture them. Its rooms must be in the house now, and its favorites in their group's household. A scene of the same name (case ignored) is replaced. Nothing on the speakers changes."
)]
async fn save_scene(
    ctx: &McpContext,
    name: String,
    scene: Option<serde_json::Value>,
    backup_queues: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| {
        match (&scene, backup_queues.unwrap_or(false)) {
            (Some(_), true) => Err(crate::tool_error(&fsonos_api::Failure::invalid(
                "backup_queues captures the current house and cannot accompany an authored scene",
            ))),
            (Some(scene), false) => b.save_scene_from(&name, scene),
            (None, true) => b.save_scene_with_queue_backups(&name),
            (None, false) => b.save_scene(&name),
        }
    })
    .await
}

#[tool(
    description = "Put the house in the saved scene `name` (case ignored; list_scenes names them). Only the needed steps are sent: regroup, volumes and mutes, sources and playback. A captured queue reference recalls its saved occurrence, elapsed time and known play mode only on the original unchanged queue; changed queues and known speaker restarts fail that step without playing a different source. A queue_backup reconstructs its saved contents on the original player; this replaces the current queue, whose contents cannot be recovered by undo_last. Volumes stay within your caps. undo_last can attempt to restore prior speaker state. Rooms no longer in the house are skipped; a step that fails is reported and independent groups still run. With `fade_secs` (1-60) the volumes ramp to the scene's over that long instead of jumping; the call answers when the fade is done.",
    annotations(idempotent)
)]
async fn apply_scene(
    ctx: &McpContext,
    name: String,
    fade_secs: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| {
        b.apply_scene(&name, &SceneApplyRequest { fade_secs })
    })
    .await
}

#[tool(
    description = "Show one saved scene by `name` (case ignored; list_scenes names the saved ones): how its rooms group, what each group plays and whether it plays, and each room's volume and mute. Queue references show an occurrence in the original unchanged queue; queue_backup summaries also show the number of saved entries that recall can reconstruct. Nothing on the speakers changes; put the house in it with apply_scene.",
    annotations(read_only, idempotent)
)]
async fn get_scene(
    ctx: &McpContext,
    name: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| b.get_scene(&name)).await
}

#[tool(
    description = "Forget the saved scene `name` (case ignored; list_scenes names the saved ones). Only the saved scene is removed; nothing on the speakers changes. An unknown name is an error."
)]
async fn delete_scene(
    ctx: &McpContext,
    name: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| b.delete_scene(&name)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_api::surface::{Surface, Survey};
    use fsonos_core::clock::SystemClock;
    use fsonos_core::policy::{Client, Policy};
    use fsonos_core::scenes::{self, Scene, SceneGroup, SceneSource};
    use fsonos_core::store::MemStore;
    use fsonos_proto::{ProtoError, Transport};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::sync::{Arc, Mutex};

    /// No speakers answer.
    struct Silent;

    impl Transport for Silent {
        fn soap_post(&self, ip: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            Err(ProtoError::Network {
                target: ip.to_string(),
                detail: "does not answer".into(),
            })
        }
    }

    /// A backend for `client` whose store holds `saved`, over an empty house.
    fn backend(client: Client, saved: &[Scene]) -> Backend {
        let mut store = MemStore::default();
        for scene in saved {
            scenes::save(&mut store, scene, 1_700_000_000).unwrap();
        }
        let survey: Survey = Box::new(|_| Ok(Vec::new()));
        let surface = Surface::new(
            Box::new(Silent),
            survey,
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(store), "mcp");
        Backend::shared(Arc::new(surface), client)
    }

    fn evening() -> Scene {
        Scene {
            name: "Evening".into(),
            groups: vec![SceneGroup {
                coordinator: "Kitchen".into(),
                members: vec!["Office".into()],
                source: SceneSource::Favorite {
                    name: "Night Radio".into(),
                    uri: None,
                },
                playing: Some(true),
            }],
            volumes: BTreeMap::from([("Kitchen".into(), 30)]),
            mutes: BTreeMap::new(),
            writer: None,
        }
    }

    fn text(result: &FinalCallToolResult) -> String {
        serde_json::to_value(result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn error_text(result: McpResult<FinalCallToolResult>) -> String {
        match result {
            Ok(r) => panic!("expected a tool error, got {}", text(&r)),
            Err(e) => e.message,
        }
    }

    #[test]
    fn list_scenes_describes_each_scene() {
        let empty = backend(Client::McpStdio, &[]).list_scenes().unwrap();
        assert!(
            text(&empty).starts_with("No scenes saved yet"),
            "{}",
            text(&empty)
        );

        let listed = backend(Client::Unknown, &[evening()])
            .list_scenes()
            .unwrap();
        assert_eq!(
            text(&listed),
            "Evening: Kitchen + Office: favorite Night Radio, playing"
        );
        let content = listed.structured_content.unwrap();
        assert_eq!(content["scenes"][0]["name"], "Evening");
        assert_eq!(content["scenes"][0]["volumes"]["Kitchen"], 30);
    }

    #[test]
    fn scene_failures_are_tool_errors_with_codes() {
        let b = backend(Client::McpStdio, &[evening()]);
        let unknown = error_text(b.apply_scene("Morning", &SceneApplyRequest::default()));
        assert!(
            unknown.starts_with("UNKNOWN_SCENE: ") && unknown.contains("Did you mean: Evening"),
            "{unknown}"
        );
        // An empty house: nothing to save or apply to.
        let empty = error_text(b.save_scene("Now"));
        assert!(empty.starts_with("NOT_READY: "), "{empty}");
        let denied = error_text(
            backend(Client::Unknown, &[evening()])
                .apply_scene("Evening", &SceneApplyRequest::default()),
        );
        assert!(denied.starts_with("POLICY_DENIED: "), "{denied}");
    }

    #[test]
    fn get_scene_shows_one_scene() {
        let b = backend(Client::McpStdio, &[evening()]);
        let got = b.get_scene("Evening").unwrap();
        assert_eq!(
            text(&got),
            "Evening: Kitchen + Office: favorite Night Radio, playing"
        );
        let unknown = error_text(b.get_scene("Morning"));
        assert!(unknown.starts_with("UNKNOWN_SCENE: "), "{unknown}");
    }

    #[test]
    fn queue_scene_tools_describe_the_original_occurrence_and_preserve_its_wire_fields() {
        let mut scene = evening();
        scene.groups[0].source = SceneSource::Queue {
            queue: scenes::QueueReference {
                coordinator: fsonos_types::PlayerId("RINCON_TEST".into()),
                household: Some(fsonos_types::HouseholdId("TEST-HOUSE".into())),
                update_id: 42,
                track: 3,
                position_secs: 83,
                play_mode: Some(fsonos_types::PlayMode::ShuffleNoRepeat),
                boot_seq: Some(7),
            },
        };
        scene.groups[0].playing = Some(false);
        let b = backend(Client::McpStdio, &[scene]);
        for result in [b.list_scenes().unwrap(), b.get_scene("Evening").unwrap()] {
            let summary = text(&result);
            assert!(
                summary.contains("original unchanged queue, track 3 at 1:23, SHUFFLE_NOREPEAT")
            );
            assert!(summary.contains("queue contents are not saved"));
            assert!(summary.contains("ready without playback"));
            assert!(!summary.contains("what it has"));
        }
        let content = b.get_scene("Evening").unwrap().structured_content.unwrap();
        assert_eq!(content["groups"][0]["source"]["kind"], "queue");
        assert_eq!(content["groups"][0]["source"]["queue"]["track"], 3);
        assert_eq!(content["groups"][0]["source"]["queue"]["boot_seq"], 7);
    }

    struct NoDaemon;

    impl crate::tools::Daemon for NoDaemon {
        fn call(
            &self,
            _: &str,
            _: &str,
            _: Option<&serde_json::Value>,
        ) -> Option<Result<serde_json::Value, fsonos_api::Failure>> {
            panic!("invalid authored input must fail before daemon selection")
        }
    }

    #[test]
    fn queue_references_cannot_be_authored_before_any_daemon_or_house_read() {
        let b = backend(Client::McpStdio, &[]).with_daemon(Arc::new(NoDaemon));
        for source in [
            serde_json::json!({"kind": "queue"}),
            serde_json::json!({"kind": "queue_backup"}),
            serde_json::json!({"kind": "queue_backup", "queue": {
                "coordinator": "RINCON_TEST", "update_id": 42,
                "track": 3, "position_secs": 83, "total": 7
            }}),
            serde_json::json!({"kind": "keep", "queue": {
                "coordinator": "RINCON_TEST", "update_id": 42,
                "track": 3, "position_secs": 83
            }}),
        ] {
            let failure = b
                .save_scene_from(
                    "Copied",
                    &serde_json::json!({
                        "groups": [{"coordinator": "Kitchen", "source": source}]
                    }),
                )
                .unwrap_err();
            assert!(failure.message.contains("only be captured"));
            assert_eq!(failure.data.unwrap()["code"], "INVALID_ARGUMENT");
        }
    }

    #[derive(Default)]
    struct CaptureDaemon(Mutex<Vec<Value>>);

    impl crate::tools::Daemon for CaptureDaemon {
        fn call(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Option<Result<Value, fsonos_api::Failure>> {
            self.0.lock().unwrap().push(json!([method, path, body]));
            Some(Ok(
                serde_json::to_value(SceneDto::new(&evening(), 0)).unwrap()
            ))
        }
    }

    #[test]
    fn backup_capture_forwards_only_its_opt_in_and_plain_capture_keeps_no_body() {
        let daemon = Arc::new(CaptureDaemon::default());
        let b = backend(Client::McpStdio, &[]).with_daemon(daemon.clone());
        let ordinary = b.save_scene("Evening").unwrap();
        let backup = b.save_scene_with_queue_backups("Evening").unwrap();
        assert_eq!(ordinary.structured_content, backup.structured_content);
        assert_eq!(
            *daemon.0.lock().unwrap(),
            [
                json!(["PUT", "/scenes/Evening", null]),
                json!(["PUT", "/scenes/Evening", {"backup_queues": true}]),
            ]
        );
    }

    #[test]
    fn save_scene_schema_advertises_an_optional_boolean_backup_switch() {
        let definition = fastmcp_server::ToolHandler::definition(&SaveScene);
        let schema = definition.input_schema;
        assert_eq!(
            schema["properties"]["backup_queues"]["type"],
            json!(["boolean", "null"])
        );
        assert_eq!(schema["required"], json!(["name"]));
        assert!(schema["properties"].get("scene").is_some());
        let description = definition.description.unwrap();
        assert!(description.contains("backup_queues"));
        assert!(description.contains("bookmark by default"));
    }

    fn direction() -> Value {
        json!({"constraints": {
            "include_composers": ["Bach"], "exclude_composers": ["Wagner"],
            "include_artists": ["Nina Simone"], "exclude_artists": ["Other artist"],
            "include_genres": ["jazz"], "exclude_genres": ["metal"],
            "periods": ["baroque"], "exclude_periods": ["late_romantic"],
            "decades": [1960], "exclude_decades": [1980],
            "include_keywords": ["piano"], "exclude_keywords": ["opera"],
            "min_work_minutes": 2, "max_work_minutes": 45,
            "energy_bias": -1, "allow_long_works": true
        }, "for_secs": 27})
    }

    #[test]
    fn saved_dj_direction_is_complete_in_scene_text_and_structured_content() {
        let mut scene = evening();
        scene.groups[0].source = serde_json::from_value(json!({
            "kind": "dj", "mood": "calm", "direction": direction()
        }))
        .unwrap();
        let b = backend(Client::McpStdio, &[scene]);
        let got = b.get_scene("Evening").unwrap();
        let listed = b.list_scenes().unwrap();
        for result in [&got, &listed] {
            let text = text(result);
            for fragment in [
                "calm",
                "Nina Simone",
                "Bach",
                "no metal",
                "1960s",
                "1980s",
                "without opera",
                "2 to 45 minutes",
                "calmer",
                "long works allowed",
                "27 seconds on each start",
            ] {
                assert!(text.contains(fragment), "{fragment}: {text}");
            }
        }
        assert_eq!(
            got.structured_content.unwrap()["groups"][0]["source"]["direction"],
            direction()
        );
        assert_eq!(
            listed.structured_content.unwrap()["scenes"][0]["groups"][0]["source"]["direction"],
            direction()
        );

        let mut scene = evening();
        scene.groups[0].source =
            serde_json::from_value(json!({"kind": "dj", "direction": {}})).unwrap();
        let automatic = backend(Client::McpStdio, &[scene])
            .get_scene("Evening")
            .unwrap();
        assert!(text(&automatic).contains("automatic program; clears current steering"));
        assert_eq!(
            automatic.structured_content.unwrap()["groups"][0]["source"]["direction"],
            json!({})
        );
    }

    #[test]
    fn malformed_scene_directions_fail_before_daemon_selection() {
        let b = backend(Client::McpStdio, &[]).with_daemon(Arc::new(NoDaemon));
        for source in [
            json!({"kind":"keep", "direction":{}}),
            json!({"kind":"dj", "direction":{"for_secs":27}}),
            json!({"kind":"dj", "mood":"calm", "direction":{"for_secs":0}}),
            json!({"kind":"dj", "direction":{"constraints":{"energy_bias":3}}}),
            json!({"kind":"dj", "direction":{"constraints":{"include_genre":["jazz"]}}}),
            json!({"kind":"dj", "direction":{"expires_at":123}}),
        ] {
            let failure = b
                .save_scene_from(
                    "Directed",
                    &json!({
                        "groups":[{"coordinator":"Kitchen", "source":source}]
                    }),
                )
                .unwrap_err();
            assert_eq!(failure.data.unwrap()["code"], "INVALID_ARGUMENT");
        }
    }

    #[derive(Default)]
    struct ScheduleDaemon(Mutex<Vec<Value>>);

    impl crate::tools::Daemon for ScheduleDaemon {
        fn call(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Option<Result<Value, fsonos_api::Failure>> {
            assert_eq!((method, path), ("POST", "/schedules"));
            let body = body.unwrap();
            self.0.lock().unwrap().push(body.clone());
            let mut reply = body.clone();
            for (key, value) in [
                ("id", json!(7)),
                ("creator", json!("mcp-stdio")),
                ("enabled", json!(true)),
                ("next", json!("2030-01-02T07:30:00Z")),
                (
                    "summary",
                    json!("start the DJ with saved filters for 27 seconds on each start"),
                ),
            ] {
                reply[key] = value;
            }
            Some(Ok(reply))
        }
    }

    fn scheduled(direction: &Value) -> fsonos_api::surface::schedules::ScheduleRequest {
        serde_json::from_value(json!({
            "when":"daily 07:30", "action":"dj_start", "zone":"Kitchen", "mood":"calm",
            "direction":direction
        }))
        .unwrap()
    }

    #[test]
    fn scheduled_directions_are_forwarded_once_without_losing_fields() {
        let daemon = Arc::new(ScheduleDaemon::default());
        let b = backend(Client::McpStdio, &[]).with_daemon(daemon.clone());
        let req = scheduled(&direction());
        let response = b.add_schedule(&req).unwrap();
        assert_eq!(
            *daemon.0.lock().unwrap(),
            [serde_json::to_value(&req).unwrap()]
        );
        assert_eq!(
            response.structured_content.as_ref().unwrap()["direction"],
            direction()
        );
        assert!(text(&response).contains("27 seconds on each start"));
    }

    #[test]
    fn invalid_scheduled_directions_fail_before_daemon_selection() {
        let b = backend(Client::McpStdio, &[]).with_daemon(Arc::new(NoDaemon));
        let mut empty_timed = scheduled(&json!({"for_secs":27}));
        empty_timed.mood = None;
        let mut other_action = scheduled(&direction());
        other_action.action = "pause".into();
        other_action.mood = None;
        for req in [
            scheduled(&json!({"constraints":{"energy_bias":3}})),
            scheduled(&json!({"for_secs":604_801})),
            empty_timed,
            other_action,
        ] {
            let failure = b.add_schedule(&req).unwrap_err();
            assert_eq!(failure.data.unwrap()["code"], "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn delete_scene_forgets_it() {
        // The owner's scene (saved before savers were kept) is not its to
        // delete; its own is.
        let theirs = error_text(backend(Client::McpStdio, &[evening()]).delete_scene("Evening"));
        assert!(theirs.starts_with("POLICY_DENIED: "), "{theirs}");
        let mine = Scene {
            writer: Some("mcp-stdio".into()),
            ..evening()
        };
        let b = backend(Client::McpStdio, &[mine]);
        let gone = b.delete_scene("Evening").unwrap();
        assert_eq!(text(&gone), "deleted scene Evening");
        assert!(
            text(&b.list_scenes().unwrap()).starts_with("No scenes saved yet"),
            "still listed after delete"
        );
        let twice = error_text(b.delete_scene("Evening"));
        assert!(twice.starts_with("UNKNOWN_SCENE: "), "{twice}");
    }
}
