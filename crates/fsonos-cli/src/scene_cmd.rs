//! `fsonos scene save|apply|list|show|rm`: the house's named states, through
//! the same surface calls as `/scenes` and the scene tools, as the house
//! policy's `cli` client. Every one goes through the daemon when one answers
//! (found as every command finds it, see `crate::remote`), so its scenes are
//! the ones listed, shown and removed, from another machine too: a scene
//! records the DJ where the daemon's DJ plays, and a DJ the scene starts is
//! the daemon's, which keeps the group's queue going. Without a daemon they
//! use the store in the data directory and survey and act right here; no DJ
//! plays to be saved, and one the scene starts plays the works it queues and
//! no more.

use anyhow::Context as _;
use clap::{Args, Subcommand};
use fsonos_api::Failure;
use fsonos_api::surface::Surface;
use fsonos_api::surface::scenes::{
    SceneApplyDto, SceneApplyRequest, SceneDefinition, SceneDeletedDto, SceneDto, SceneGroupDto,
};
use fsonos_core::policy::Client;
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::config::GlobalArgs;
use crate::remote::{Daemon, pct};

#[derive(Args)]
pub struct SceneArgs {
    #[command(subcommand)]
    action: SceneAction,
}

#[derive(Subcommand)]
enum SceneAction {
    /// Save the house as it is now as a scene: grouping, volumes and mutes,
    /// what each group plays (replaces a scene of that name). With --from, the
    /// scene a TOML file writes out instead. Ordinary queues save a position
    /// reference: recall requires the original unchanged queue; contents are
    /// not backed up.
    Save {
        name: String,
        /// A TOML file writing the scene out: `[[groups]]` with `coordinator`,
        /// `members`, `source = { kind = "favorite", name = "…" }` (or `dj`
        /// with a `mood`, a `uri`, `keep`) and `playing`; `[volumes]` and
        /// `[mutes]` by room.
        #[arg(long, value_name = "FILE")]
        from: Option<PathBuf>,
    },
    /// Put the house in a scene: only the steps it needs are sent;
    /// captured queue positions require the original unchanged queue.
    /// `fsonos undo` attempts to restore prior speaker state. Exits 1 if a step failed.
    Apply {
        name: String,
        /// Ramp the volumes to the scene's over this long instead of at once
        /// (`10`, `10s` or `1m`; at most 60 s).
        #[arg(long, value_parser = crate::parse_fade)]
        fade: Option<u32>,
    },
    /// Every saved scene.
    List,
    /// A scene's groups, what each plays, and each room's volume.
    Show { name: String },
    /// Forget a scene.
    #[command(visible_alias = "remove")]
    Rm { name: String },
}

/// The surface over the LAN and the data directory's store; it surveys only
/// when a call needs the speakers.
fn surface(global: &GlobalArgs) -> Result<Surface, Failure> {
    let dir = crate::daemon::data_dir(global)?;
    let policy = crate::daemon::policy(&dir)?;
    let surface = crate::daemon::surface(global, policy)?;
    Ok(crate::daemon::with_action_log(surface, &dir, "cli"))
}

pub fn run(global: &GlobalArgs, args: &SceneArgs) -> anyhow::Result<()> {
    let s = || surface(global);
    let cli = Client::Cli;
    match &args.action {
        SceneAction::Save {
            name,
            from: Some(file),
        } => {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("read {}", file.display()))?;
            let def: SceneDefinition = toml::from_str(&text).map_err(|e| {
                Failure::invalid(format!("{}: {e}", file.display())).with_hint(
                    "Write [[groups]] with a coordinator, and [volumes] and [mutes] by room.",
                )
            })?;
            // Checked here: to the daemon an empty body is "the house as it is".
            if def == SceneDefinition::default() {
                return Err(Failure::invalid(format!(
                    "{} names no group, volume or mute",
                    file.display()
                ))
                .with_hint("Write at least one, or drop --from to save the house as it is.")
                .into());
            }
            def.validate_sources()?;
            let scene: SceneDto = match Daemon::find(global, global.daemon)? {
                Some(daemon) => daemon.put_json(&format!("/scenes/{}", pct(name)), &def)?,
                None => s()?.save_scene_from(&cli, name, &def)?,
            };
            crate::emit(global.json, &scene, |scene| {
                format!("saved scene {}", show_text(scene))
            })
        }
        SceneAction::Save { name, from: None } => {
            // The DJ plays in the daemon: only it knows where.
            let scene: SceneDto = match Daemon::find(global, global.daemon)? {
                Some(daemon) => daemon.put(&format!("/scenes/{}", pct(name)))?,
                None => s()?.save_scene(&cli, name)?,
            };
            crate::emit(global.json, &scene, |scene| {
                format!("saved scene {}", show_text(scene))
            })
        }
        SceneAction::Apply { name, fade } => {
            let req = SceneApplyRequest { fade_secs: *fade };
            // A DJ the scene starts keeps the group's queue going only in
            // the daemon.
            let applied: SceneApplyDto = match Daemon::find(global, global.daemon)? {
                Some(daemon) => daemon.post(&format!("/scenes/{}/apply", pct(name)), &req)?,
                None => s()?.apply_scene_with(&cli, name, &req)?,
            };
            crate::emit(global.json, &applied, apply_text)?;
            if applied.complete {
                Ok(())
            } else {
                anyhow::bail!(
                    "{} step(s) of scene {} failed",
                    applied.failed.len(),
                    applied.scene
                )
            }
        }
        SceneAction::List => {
            let scenes: Vec<SceneDto> = match Daemon::find(global, global.daemon)? {
                Some(daemon) => daemon.get("/scenes")?,
                None => s()?.scenes(&cli)?,
            };
            crate::emit(global.json, &scenes, |scenes| list_text(scenes))
        }
        SceneAction::Show { name } => {
            let scene: SceneDto = match Daemon::find(global, global.daemon)? {
                Some(daemon) => daemon.get(&format!("/scenes/{}", pct(name)))?,
                None => s()?.scene(&cli, name)?,
            };
            crate::emit(global.json, &scene, show_text)
        }
        SceneAction::Rm { name } => {
            let deleted: SceneDeletedDto = match Daemon::find(global, global.daemon)? {
                Some(daemon) => daemon.delete(&format!("/scenes/{}", pct(name)))?,
                None => s()?.delete_scene(&cli, name)?,
            };
            crate::emit(global.json, &deleted, |d| format!("{}\n", d.done))
        }
    }
}

/// A group's rooms, what it plays, and whether it plays.
fn group_text(g: &SceneGroupDto) -> String {
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
}

/// `fsonos scene list`: a scene a line.
#[must_use]
pub fn list_text(scenes: &[SceneDto]) -> String {
    if scenes.is_empty() {
        return "no scenes saved yet (fsonos scene save <name>)\n".to_string();
    }
    scenes.iter().fold(String::new(), |mut out, scene| {
        let groups: Vec<String> = scene.groups.iter().map(group_text).collect();
        let _ = writeln!(out, "{}: {}", scene.name, groups.join("; "));
        out
    })
}

/// `fsonos scene show`: its groups, then its rooms' levels.
#[must_use]
pub fn show_text(scene: &SceneDto) -> String {
    let mut out = format!("{}\n", scene.name);
    for g in &scene.groups {
        let _ = writeln!(out, "  {}", group_text(g));
    }
    if !scene.volumes.is_empty() {
        let levels: Vec<String> = scene
            .volumes
            .iter()
            .map(|(room, level)| {
                let muted = scene.mutes.get(room).copied().unwrap_or(false);
                format!("{room} {level}{}", if muted { " (muted)" } else { "" })
            })
            .collect();
        let _ = writeln!(out, "  volumes: {}", levels.join(", "));
    }
    out
}

/// `fsonos scene apply`: what was done, each step, what failed.
#[must_use]
pub fn apply_text(applied: &SceneApplyDto) -> String {
    let mut out = format!("{}\n", applied.done);
    for step in &applied.steps {
        let _ = writeln!(out, "  {step}");
    }
    for f in &applied.failed {
        let _ = writeln!(out, "  failed: {}: {}", f.step, f.error);
    }
    for n in &applied.notes {
        let _ = writeln!(out, "  note: {}", n.detail);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_api::surface::scenes::SceneSourceDto;
    use std::collections::BTreeMap;

    fn evening() -> SceneDto {
        let source = |kind: &str| SceneSourceDto {
            kind: kind.into(),
            name: None,
            uri: None,
            mood: None,
            queue: None,
        };
        SceneDto {
            name: "Evening".into(),
            saved: 0,
            groups: vec![
                SceneGroupDto {
                    coordinator: "Kitchen".into(),
                    members: vec!["Office".into()],
                    source: SceneSourceDto {
                        mood: Some("calm".into()),
                        ..source("dj")
                    },
                    playing: Some(true),
                },
                SceneGroupDto {
                    coordinator: "Bedroom".into(),
                    members: Vec::new(),
                    source: source("keep"),
                    playing: Some(false),
                },
            ],
            volumes: BTreeMap::from([("Kitchen".into(), 30), ("Office".into(), 25)]),
            mutes: BTreeMap::from([("Office".into(), true)]),
        }
    }

    #[test]
    fn scenes_read_as_text() {
        assert_eq!(
            list_text(&[evening()]),
            "Evening: Kitchen + Office: the DJ (calm), playing; Bedroom: what it has, paused\n"
        );
        assert_eq!(
            show_text(&evening()),
            "Evening\n  Kitchen + Office: the DJ (calm), playing\n  Bedroom: what it has, \
             paused\n  volumes: Kitchen 30, Office 25 (muted)\n"
        );
        assert!(list_text(&[]).starts_with("no scenes saved yet"));
    }

    #[test]
    fn captured_queue_text_explains_occurrence_and_recall_limits() {
        let mut scene = evening();
        scene.groups[0].source = SceneSourceDto {
            kind: "queue".into(),
            name: None,
            uri: None,
            mood: None,
            queue: Some(fsonos_api::surface::scenes::SceneQueueDto {
                coordinator: "RINCON_SYNTHETIC".into(),
                household: Some("TEST-HOUSE".into()),
                update_id: 42,
                track: 3,
                position_secs: 83,
                play_mode: Some("SHUFFLE_NOREPEAT".into()),
                boot_seq: Some(7),
            }),
        };
        scene.groups[0].playing = Some(false);
        let text = show_text(&scene);
        assert!(text.contains("original unchanged queue, track 3 at 1:23, SHUFFLE_NOREPEAT"));
        assert!(text.contains("queue contents are not saved"));
        assert!(text.contains("ready without playback"));
        assert!(!text.contains("Kitchen + Office: what it has"));
    }
}
