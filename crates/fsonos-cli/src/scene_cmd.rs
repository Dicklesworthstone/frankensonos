//! `fsonos scene save|apply|list|show|rm`: the house's named states, through
//! the same surface calls as `/scenes` and the scene tools, as the house
//! policy's `cli` client. Listing, showing and removing read only the store
//! in the data directory; saving and applying survey the LAN first.

use clap::{Args, Subcommand};
use fsonos_api::Failure;
use fsonos_api::surface::Surface;
use fsonos_api::surface::scenes::{SceneApplyDto, SceneDto, SceneGroupDto};
use fsonos_core::policy::Client;
use std::fmt::Write as _;

use crate::config::GlobalArgs;

#[derive(Args)]
pub struct SceneArgs {
    #[command(subcommand)]
    action: SceneAction,
}

#[derive(Subcommand)]
enum SceneAction {
    /// Save the house as it is now as a scene: grouping, volumes and mutes,
    /// what each group plays (replaces a scene of that name).
    Save { name: String },
    /// Put the house in a scene: only the steps it needs are sent;
    /// `fsonos undo` puts it back. Exits 1 if a step failed.
    Apply { name: String },
    /// Every saved scene.
    List,
    /// A scene's groups, what each plays, and each room's volume.
    Show { name: String },
    /// Forget a scene.
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
    let s = surface(global)?;
    let cli = Client::Cli;
    match &args.action {
        SceneAction::Save { name } => {
            let scene = s.save_scene(&cli, name)?;
            crate::emit(global.json, &scene, |scene| {
                format!("saved scene {}", show_text(scene))
            })
        }
        SceneAction::Apply { name } => {
            let applied = s.apply_scene(&cli, name)?;
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
            let scenes = s.scenes(&cli)?;
            crate::emit(global.json, &scenes, |scenes| list_text(scenes))
        }
        SceneAction::Show { name } => {
            let scene = s.scene(&cli, name)?;
            crate::emit(global.json, &scene, show_text)
        }
        SceneAction::Rm { name } => {
            let deleted = s.delete_scene(&cli, name)?;
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
        _ => "what it has".to_string(),
    };
    let state = if g.playing { "playing" } else { "paused" };
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
                    playing: true,
                },
                SceneGroupDto {
                    coordinator: "Bedroom".into(),
                    members: Vec::new(),
                    source: source("keep"),
                    playing: false,
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
}
