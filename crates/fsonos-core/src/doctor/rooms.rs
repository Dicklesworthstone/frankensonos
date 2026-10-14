//! The `rooms.aliases` check: the owner's `aliases.toml` parses, and every
//! alias (and default room) names a room the households show now.
//!
//! The surfaces read the file on each call that names a room and ignore it,
//! with only a log line, when it does not parse, so a typo there silently
//! turns every alias off. This check says so, with the line to fix.

use super::lan::LanProbe;
use super::{Check, CheckContext, CheckId, CheckResult, Runner};
use crate::HouseholdState;
use crate::rooms::Aliases;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const ALIASES: CheckId = CheckId("rooms.aliases");

/// Register the alias check for `file` (the data directory's
/// `aliases.toml`), comparing it with what `probe` surveys.
pub fn register(runner: &mut Runner, file: PathBuf, probe: &Arc<LanProbe>) {
    runner.register(AliasesCheck {
        file,
        probe: Arc::clone(probe),
    });
}

struct AliasesCheck {
    file: PathBuf,
    probe: Arc<LanProbe>,
}

impl Check for AliasesCheck {
    fn id(&self) -> CheckId {
        ALIASES
    }
    fn title(&self) -> &'static str {
        "Room aliases"
    }
    // No prerequisite: a file that does not parse is worth reporting even
    // when no household answered.
    fn run(&self, _ctx: &CheckContext) -> CheckResult {
        aliases(&self.file, self.probe.households())
    }
}

/// The verdict on the aliases in `file` against `households` (or why there
/// are none to compare with: the file is still parsed).
#[must_use]
pub fn aliases(file: &Path, households: Result<&[HouseholdState], &str>) -> CheckResult {
    if !file.exists() {
        return CheckResult::skip("no aliases.toml: rooms are named as the speakers name them");
    }
    let parsed = match Aliases::load(file) {
        Ok(parsed) => parsed,
        Err(e) => {
            return CheckResult::fail(
                format!("{e}; every alias is ignored until it parses"),
                format!("Fix that line of {}.", file.display()),
            )
            .with_evidence(json!({ "line": e.line }));
        }
    };
    let households = match households {
        Ok(households) => households,
        Err(why) => {
            return CheckResult::pass(format!(
                "aliases.toml parses; its rooms were not compared: no households ({why})"
            ));
        }
    };
    let warnings = parsed.check(households);
    if warnings.is_empty() {
        return CheckResult::pass("every alias names a room the house has");
    }
    let lines: Vec<String> = warnings
        .iter()
        .map(|w| format!("line {}: {}", w.line, w.message))
        .collect();
    let summary = match warnings.len() {
        1 => "1 alias entry does not match the house".to_string(),
        n => format!("{n} alias entries do not match the house"),
    };
    CheckResult::warn(
        summary,
        format!(
            "Edit {} at the lines named (a room that is only offline needs no change).",
            file.display()
        ),
    )
    .with_detail(lines.join("\n"))
    .with_evidence(json!({
        "warnings": warnings
            .iter()
            .map(|w| json!({ "line": w.line, "message": w.message }))
            .collect::<Vec<_>>()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Room;
    use crate::doctor::Status;
    use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};

    fn house() -> Vec<HouseholdState> {
        let id = |s: &str| PlayerId(s.into());
        let player = |pid: &str, room: &str| Player {
            id: id(pid),
            room_name: room.into(),
            ip: "192.0.2.10".parse().unwrap(),
            model: String::new(),
            generation: Generation::S2,
        };
        let room = |name: &str, pid: &str| Room {
            name: name.into(),
            primary: id(pid),
            players: vec![id(pid)],
            missing: Vec::new(),
            coordinator: id(pid),
        };
        vec![HouseholdState {
            players: vec![player("RINCON_DEN", "Den"), player("RINCON_KIT", "Kitchen")],
            groups: vec![
                ZoneGroup {
                    coordinator: id("RINCON_DEN"),
                    members: vec![id("RINCON_DEN")],
                },
                ZoneGroup {
                    coordinator: id("RINCON_KIT"),
                    members: vec![id("RINCON_KIT")],
                },
            ],
            rooms: vec![room("Den", "RINCON_DEN"), room("Kitchen", "RINCON_KIT")],
            ..Default::default()
        }]
    }

    /// `text` as an aliases.toml in a fresh directory of its own.
    fn file(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsonos-doctor-aliases-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aliases.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn aliases_are_checked_against_the_house() {
        let house = house();
        let missing = std::env::temp_dir().join("fsonos-doctor-no-such-dir/aliases.toml");
        assert_eq!(aliases(&missing, Ok(house.as_slice())).status, Status::Skip);

        let good = file(
            "good",
            "[aliases]\ncook = \"Kitchen\"\ndownstairs = [\"Den\", \"Kitchen\"]\n",
        );
        assert_eq!(aliases(&good, Ok(house.as_slice())).status, Status::Pass);

        // An alias naming a room the house does not have warns, with its line.
        let stale = file("stale", "[aliases]\ndownstairs = [\"Den\", \"Attic\"]\n");
        let r = aliases(&stale, Ok(house.as_slice()));
        assert_eq!(r.status, Status::Warn, "{r:?}");
        assert!(
            r.detail.as_deref().unwrap_or("").contains("line 2"),
            "{r:?}"
        );
        assert!(r.detail.as_deref().unwrap_or("").contains("Attic"), "{r:?}");

        // An alias that is also a room's name warns: the room wins.
        let shadowed = file("shadowed", "[aliases]\nkitchen = \"Den\"\n");
        let r = aliases(&shadowed, Ok(house.as_slice()));
        assert_eq!(r.status, Status::Warn, "{r:?}");
        assert!(
            r.detail.as_deref().unwrap_or("").contains("the room wins"),
            "{r:?}"
        );

        // A file that does not parse fails: every alias is off until fixed.
        let broken = file("broken", "[aliases]\nkitchen = 5\n");
        let r = aliases(&broken, Ok(house.as_slice()));
        assert_eq!(r.status, Status::Fail, "{r:?}");
        assert!(r.summary.contains("line 2"), "{r:?}");
        assert!(r.summary.contains("every alias is ignored"), "{r:?}");

        // With no household to compare with, the file is still parsed.
        let r = aliases(&broken, Err("no household answered"));
        assert_eq!(r.status, Status::Fail, "{r:?}");
        let r = aliases(&stale, Err("no household answered"));
        assert_eq!(r.status, Status::Pass, "{r:?}");
        assert!(r.summary.contains("not compared"), "{r:?}");

        for path in [good, stale, shadowed, broken] {
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }
}
