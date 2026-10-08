//! The scene tools: `list_scenes`, `save_scene` and `apply_scene`, over the
//! surface's scenes (`fsonos_api::surface::scenes`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::scenes::{SceneApplyDto, SceneDto};
use serde::Serialize;

use super::{Backend, respond, with_backend};

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
                _ => "what it has".to_string(),
            };
            let state = if g.playing { "playing" } else { "paused" };
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
            let scenes = self.surface.scenes(&self.client)?;
            let text = if scenes.is_empty() {
                "No scenes saved yet; save one with save_scene.".to_string()
            } else {
                scenes.iter().map(scene_line).collect::<Vec<_>>().join("\n")
            };
            Ok((text, ScenesDto { scenes }))
        })
    }

    /// The `save_scene` tool.
    pub fn save_scene(&self, name: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let scene = self.surface.save_scene(&self.client, name)?;
            Ok((format!("Saved {}", scene_line(&scene)), scene))
        })
    }

    /// The `apply_scene` tool.
    pub fn apply_scene(&self, name: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let applied = self.surface.apply_scene(&self.client, name)?;
            Ok((apply_text(&applied), applied))
        })
    }
}

#[tool(
    description = "List the saved scenes: named house states, each with how the rooms group, what each group plays (a favorite, a stream, the DJ in a mood, or whatever it has) and whether it plays, and each room's volume and mute. Put the house in one with apply_scene.",
    annotations(read_only, idempotent)
)]
fn list_scenes(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::list_scenes)
}

#[tool(
    description = "Save the house as it is now as a scene called `name` (1-64 characters): how the rooms group, each room's volume and mute, and what each group plays and whether it plays. A scene of the same name (case ignored) is replaced. Nothing on the speakers changes."
)]
fn save_scene(_ctx: &McpContext, name: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.save_scene(&name))
}

#[tool(
    description = "Put the house in the saved scene `name` (case ignored; list_scenes names them). Minimal: only the steps the house needs are sent (regroup, then volumes and mutes, then what plays, then play or pause), so applying a scene the house already matches changes nothing. Volumes stay within your caps. Undoable: undo_last restores the zones it changed. Rooms no longer in the house are skipped; a step that fails is reported and the rest still run.",
    annotations(idempotent)
)]
fn apply_scene(_ctx: &McpContext, name: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.apply_scene(&name))
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
                playing: true,
            }],
            volumes: BTreeMap::from([("Kitchen".into(), 30)]),
            mutes: BTreeMap::new(),
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
        let unknown = error_text(b.apply_scene("Morning"));
        assert!(
            unknown.starts_with("UNKNOWN_SCENE: ") && unknown.contains("Did you mean: Evening"),
            "{unknown}"
        );
        // An empty house: nothing to save or apply to.
        let empty = error_text(b.save_scene("Now"));
        assert!(empty.starts_with("NOT_READY: "), "{empty}");
        let denied = error_text(backend(Client::Unknown, &[evening()]).apply_scene("Evening"));
        assert!(denied.starts_with("POLICY_DENIED: "), "{denied}");
    }
}
