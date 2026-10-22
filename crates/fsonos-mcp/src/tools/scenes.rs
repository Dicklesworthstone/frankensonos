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
                "dj" => source
                    .mood
                    .as_ref()
                    .map_or_else(|| "the DJ".to_string(), |m| format!("the DJ ({m})")),
                "queue" => source.queue.as_ref().map_or_else(
                    || "queue reference unavailable; capture the scene again".into(),
                    fsonos_api::surface::scenes::SceneQueueDto::text,
                ),
                _ => "what it has".to_string(),
            };
            let state = match g.playing {
                Some(true) => "playing",
                Some(false) if source.kind == "queue" => "ready without playback",
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
        let path = format!("/scenes/{}", segment(name));
        if let Some(answer) = self.on_daemon("PUT", &path, None, |scene: &SceneDto| {
            format!("Saved {}", scene_line(scene))
        }) {
            return answer;
        }
        respond(|| {
            let scene = self.surface.save_scene(&self.client, name)?;
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
    description = "List the saved scenes: named house states, each with how the rooms group, what each group plays (a favorite, a URI, the DJ in a mood, a captured queue position, or whatever it has) and whether it plays, and each room's volume and mute. A queue reference requires the original unchanged queue; it is not a backup of its contents. Put the house in a scene with apply_scene.",
    annotations(read_only, idempotent)
)]
async fn list_scenes(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::list_scenes).await
}

#[tool(
    description = "Save a scene called `name` (1-64 characters): the house as it is now (how the rooms group, each room's volume and mute, what each group plays and whether it plays), or, with `scene`, the one it writes out: {\"groups\": [{\"coordinator\": room, \"members\": [rooms], \"source\": {\"kind\": \"favorite\", \"name\": title} (or kind dj with an optional mood, uri with a stream uri or a Spotify track, album or playlist link, keep), \"playing\": bool}], \"volumes\": {room: 0-100}, \"mutes\": {room: bool}}. Capturing an ordinary queue saves its selected occurrence, elapsed time and known play mode as a reference to the original coordinator's unchanged queue; queue contents are not backed up. A changed queue or known speaker restart prevents recall. Queue references cannot be authored in scene; omit scene to capture them. Its rooms must be in the house now, and its favorites in their group's household. A scene of the same name (case ignored) is replaced. Nothing on the speakers changes."
)]
async fn save_scene(
    ctx: &McpContext,
    name: String,
    scene: Option<serde_json::Value>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| match &scene {
        Some(scene) => b.save_scene_from(&name, scene),
        None => b.save_scene(&name),
    })
    .await
}

#[tool(
    description = "Put the house in the saved scene `name` (case ignored; list_scenes names them). Only the needed steps are sent: regroup, volumes and mutes, sources and playback. A captured queue reference recalls its saved occurrence, elapsed time and known play mode only on the original unchanged queue; changed queues and known speaker restarts fail that step without playing a different source. It never reconstructs queue contents. Volumes stay within your caps. undo_last can attempt to restore prior speaker state. Rooms no longer in the house are skipped; a step that fails is reported and independent groups still run. With `fade_secs` (1-60) the volumes ramp to the scene's over that long instead of jumping; the call answers when the fade is done.",
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
    description = "Show one saved scene by `name` (case ignored; list_scenes names the saved ones): how its rooms group, what each group plays and whether it plays, and each room's volume and mute. Queue references show a saved occurrence and time in the original unchanged queue, not saved queue contents. Nothing on the speakers changes; put the house in it with apply_scene.",
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
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::sync::Arc;

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
            panic!("an authored queue reference must fail before daemon selection")
        }
    }

    #[test]
    fn queue_references_cannot_be_authored_before_any_daemon_or_house_read() {
        let b = backend(Client::McpStdio, &[]).with_daemon(Arc::new(NoDaemon));
        for source in [
            serde_json::json!({"kind": "queue"}),
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
