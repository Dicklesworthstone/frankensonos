//! `aliases.toml` in the data dir: the owner's names for rooms and sets of
//! rooms, and the default room `here` means for each client.
//!
//! ```toml
//! [aliases]
//! kitchen = "Kitchen Counter"
//! downstairs = ["Kitchen Counter", "Living Room"]
//!
//! [defaults]
//! room = "Office"
//!
//! [defaults.clients."alice@example.com"]
//! room = "Bedroom"
//! ```
//!
//! Aliases name real rooms only (never other aliases), so they cannot form
//! cycles. A malformed file is an [`AliasError`] with a line number; a room
//! the households do not show right now is only an [`AliasWarning`] from
//! [`Aliases::check`] (the room may be offline).

use super::{RESERVED, direct_hits, household_labels, normalize_room};
use crate::HouseholdState;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use toml::Spanned;

/// One alias: a name for one or more rooms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// As written in the file.
    pub name: String,
    /// Room names (or `Name@Label` / player ids), never other aliases.
    pub rooms: Vec<String>,
    /// 1-based line of the entry.
    pub line: usize,
}

/// The parsed `aliases.toml`. The default value has no aliases or defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aliases {
    entries: Vec<Alias>,
    default_room: Option<(String, usize)>,
    client_rooms: BTreeMap<String, (String, usize)>,
}

/// A malformed `aliases.toml`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("aliases.toml{}: {message}", .line.map(|l| format!(" line {l}")).unwrap_or_default())]
pub struct AliasError {
    pub line: Option<usize>,
    pub message: String,
}

/// A well-formed entry that does not match the households right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasWarning {
    pub line: usize,
    pub message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    aliases: BTreeMap<String, Spanned<Target>>,
    #[serde(default)]
    defaults: Defaults,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Target {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Defaults {
    room: Option<Spanned<String>>,
    #[serde(default)]
    clients: BTreeMap<String, ClientDefaults>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientDefaults {
    room: Spanned<String>,
}

/// 1-based line of byte `offset` in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text.char_indices()
        .take_while(|(i, _)| *i < offset)
        .filter(|(_, c)| *c == '\n')
        .count()
        + 1
}

impl Aliases {
    /// Parse the text of an `aliases.toml`.
    pub fn parse(text: &str) -> Result<Self, AliasError> {
        let file: File = toml::from_str(text).map_err(|e| AliasError {
            line: e.span().map(|s| line_of(text, s.start)),
            message: e.message().to_string(),
        })?;

        // In file order, so a repeated name is reported on its later line.
        let mut entries: Vec<(String, Spanned<Target>)> = file.aliases.into_iter().collect();
        entries.sort_by_key(|(_, target)| target.span().start);
        let mut aliases: Vec<Alias> = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for (name, target) in entries {
            let line = line_of(text, target.span().start);
            let fail = |message: String| AliasError {
                line: Some(line),
                message,
            };
            let key = normalize_room(&name);
            if key.is_empty() {
                return Err(fail("an alias name cannot be empty".into()));
            }
            if RESERVED.contains(&key.as_str()) {
                return Err(fail(format!(
                    "{name:?} is reserved (all, everywhere, here) and cannot be an alias"
                )));
            }
            if let Some(first) = seen.insert(key, line) {
                return Err(fail(format!(
                    "alias {name:?} repeats the alias on line {first} \
                     (names match case- and apostrophe-insensitively)"
                )));
            }
            let rooms = match target.into_inner() {
                Target::One(room) => vec![room],
                Target::Many(rooms) => rooms,
            };
            if rooms.is_empty() || rooms.iter().any(|r| r.trim().is_empty()) {
                return Err(fail(format!(
                    "alias {name:?} must name at least one room, and no empty names"
                )));
            }
            aliases.push(Alias { name, rooms, line });
        }

        let located = |room: Spanned<String>| -> Result<(String, usize), AliasError> {
            let line = line_of(text, room.span().start);
            let room = room.into_inner();
            if room.trim().is_empty() {
                return Err(AliasError {
                    line: Some(line),
                    message: "a default room cannot be empty".into(),
                });
            }
            Ok((room, line))
        };
        let default_room = file.defaults.room.map(located).transpose()?;
        let client_rooms = file
            .defaults
            .clients
            .into_iter()
            .map(|(client, d)| Ok((client, located(d.room)?)))
            .collect::<Result<_, AliasError>>()?;

        Ok(Self {
            entries: aliases,
            default_room,
            client_rooms,
        })
    }

    /// Load `path`; a missing file means no aliases.
    pub fn load(path: &Path) -> Result<Self, AliasError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(AliasError {
                line: None,
                message: format!("cannot read {}: {e}", path.display()),
            }),
        }
    }

    /// Every alias, in file order.
    #[must_use]
    pub fn aliases(&self) -> &[Alias] {
        &self.entries
    }

    /// The alias named `query` (case- and apostrophe-insensitive).
    #[must_use]
    pub fn get(&self, query: &str) -> Option<&Alias> {
        let wanted = normalize_room(query);
        self.entries
            .iter()
            .find(|a| normalize_room(&a.name) == wanted)
    }

    /// The room `here` means for `client`: its own default, else the global one.
    #[must_use]
    pub fn default_room(&self, client: Option<&str>) -> Option<&str> {
        client
            .and_then(|c| self.client_rooms.get(c))
            .or(self.default_room.as_ref())
            .map(|(room, _)| room.as_str())
    }

    /// Entries that do not match `households` right now: rooms no household
    /// shows (perhaps offline), targets that are aliases rather than rooms,
    /// and aliases a real room name shadows (the room wins).
    #[must_use]
    pub fn check(&self, households: &[HouseholdState]) -> Vec<AliasWarning> {
        let labels = household_labels(households);
        let mut warnings = Vec::new();
        let mut check_room = |room: &str, line: usize, what: &str| {
            if !direct_hits(households, &labels, room).is_empty() {
                return;
            }
            let message = if self.get(room).is_some() {
                format!("{what} names the alias {room:?}; aliases may name only rooms")
            } else {
                format!("{what} names {room:?}, which no household shows right now")
            };
            warnings.push(AliasWarning { line, message });
        };
        for alias in &self.entries {
            for room in &alias.rooms {
                check_room(room, alias.line, &format!("alias {:?}", alias.name));
            }
        }
        if let Some((room, line)) = &self.default_room {
            check_room(room, *line, "the default room");
        }
        for (client, (room, line)) in &self.client_rooms {
            check_room(room, *line, &format!("the default room for {client:?}"));
        }
        for alias in &self.entries {
            if !direct_hits(households, &labels, &alias.name).is_empty() {
                warnings.push(AliasWarning {
                    line: alias.line,
                    message: format!("alias {:?} is also a room name; the room wins", alias.name),
                });
            }
        }
        warnings.sort_by_key(|w| w.line);
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"# The owner's names for rooms.
[aliases]
kitchen = "Kitchen Counter"
downstairs = ["Kitchen Counter", "Living Room"]
"Ada's" = "Ada’s Studio"

[defaults]
room = "Office"

[defaults.clients."alice@example.com"]
room = "Bedroom"
"#;

    #[test]
    fn parses_aliases_sets_and_defaults_with_lines() {
        let a = Aliases::parse(FILE).unwrap();
        let names: Vec<_> = a
            .aliases()
            .iter()
            .map(|x| (x.name.as_str(), x.line))
            .collect();
        assert_eq!(names, [("kitchen", 3), ("downstairs", 4), ("Ada's", 5)]);
        assert_eq!(
            a.get("DOWNSTAIRS").unwrap().rooms,
            ["Kitchen Counter", "Living Room"]
        );
        assert_eq!(
            a.get("ada\u{2019}s").unwrap().rooms,
            ["Ada\u{2019}s Studio"]
        );
        assert!(a.get("upstairs").is_none());
        assert_eq!(a.default_room(None), Some("Office"));
        assert_eq!(a.default_room(Some("alice@example.com")), Some("Bedroom"));
        assert_eq!(a.default_room(Some("bob@example.com")), Some("Office"));
        assert_eq!(Aliases::parse("").unwrap(), Aliases::default());
        assert_eq!(Aliases::default().default_room(Some("x")), None);
    }

    #[test]
    fn malformed_files_report_the_line() {
        let err = |text: &str| Aliases::parse(text).unwrap_err();

        let e = err("[aliases]\nkitchen = \"Kitchen\"\nhere = \"Office\"\n");
        assert_eq!(e.line, Some(3));
        assert!(e.message.contains("reserved"), "{e}");

        let e = err("[aliases]\nKitchen = \"A\"\n\"kitchen\" = \"B\"\n");
        assert_eq!(e.line, Some(3));
        assert!(e.message.contains("line 2"), "{e}");
        // Key order differs from file order: still the later line.
        let e = err("[aliases]\nkitchen = \"A\"\nKitchen = \"B\"\n");
        assert_eq!(e.line, Some(3));
        assert!(e.message.contains("line 2"), "{e}");

        let e = err("[aliases]\nnothing = []\n");
        assert_eq!(e.line, Some(2));

        let e = err("[aliases]\nkitchen = 3\n");
        assert_eq!(e.line, Some(2));

        let e = err("[defaults]\nroom = \"Office\"\ncolour = \"red\"\n");
        assert_eq!(e.line, Some(3));

        let e = err("[aliases\nkitchen = \"Kitchen\"\n");
        assert_eq!(e.line, Some(1));
        assert!(e.to_string().starts_with("aliases.toml line 1: "), "{e}");
    }

    #[test]
    fn a_missing_file_means_no_aliases() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Aliases::load(&dir.path().join("aliases.toml")).unwrap(),
            Aliases::default()
        );
        let path = dir.path().join("aliases.toml");
        std::fs::write(&path, FILE).unwrap();
        assert_eq!(Aliases::load(&path).unwrap().aliases().len(), 3);
    }
}
