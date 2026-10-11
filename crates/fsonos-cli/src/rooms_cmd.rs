//! `fsonos rooms`: the rooms with their aliases, and `fsonos rooms alias
//! add|rm` and `fsonos rooms default`, which edit `aliases.toml` in the data
//! directory.
//!
//! An edit touches only its own entry (an alias in the `[aliases]` table, or
//! the `room` of `[defaults]` or of a client's `[defaults.clients."<id>"]`):
//! added at the table's end, or replaced or removed where it stands, so the
//! owner's comments, ordering and layout survive. The result is checked with
//! the same parser the daemon uses before it replaces the file.

use fsonos_api::Failure;
use fsonos_core::rooms::{Aliases, normalize_room};
use std::path::Path;

/// `text` with alias `name` naming `rooms`: its entry replaced where it
/// stands, or added at the end of the `[aliases]` table (created when there
/// is none).
#[must_use]
pub fn set_alias(text: &str, name: &str, rooms: &[String]) -> String {
    let entry = format!("{} = {}", toml_key(name), toml_value(rooms));
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some((_, end)) = section(&lines) {
        if let Some((at, len)) = find_entry(&lines, name) {
            lines.splice(at..at + len, [entry]);
        } else {
            // After the table's last entry, before any trailing blanks.
            let mut at = end;
            while at > 0 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.insert(at, entry);
        }
    } else {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("[aliases]".to_string());
        lines.push(entry);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `text` without alias `name`, or `None` when it names no such alias.
#[must_use]
pub fn remove_alias(text: &str, name: &str) -> Option<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let (at, len) = find_entry(&lines, name)?;
    lines.drain(at..at + len);
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out)
}

/// `text` with `room` as the room `here` means, for everyone or for
/// `client`'s requests: the `room` line of that defaults table replaced where
/// it stands, or added (with the table, when there is none).
#[must_use]
pub fn set_default_room(text: &str, client: Option<&str>, room: &str) -> String {
    let header = defaults_table(client);
    let entry = format!("room = {}", toml::Value::String(room.to_string()));
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some((start, end)) = table(&lines, &header) {
        if let Some(at) = room_line(&lines, start, end) {
            lines[at] = entry;
        } else {
            let mut at = end;
            while at > start + 1 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.insert(at, entry);
        }
    } else {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(entry);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `text` without that default room, or `None` when none is set there. A
/// client's table goes with it (it holds nothing else); `[defaults]` stays.
#[must_use]
pub fn clear_default_room(text: &str, client: Option<&str>) -> Option<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let (start, end) = table(&lines, &defaults_table(client))?;
    let at = room_line(&lines, start, end)?;
    lines.remove(at);
    if client.is_some() {
        lines.remove(start);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out)
}

/// `[defaults]`, or `client`'s `[defaults.clients."<id>"]`.
fn defaults_table(client: Option<&str>) -> String {
    client.map_or_else(
        || "[defaults]".to_owned(),
        |c| format!("[defaults.clients.{}]", toml_key(c)),
    )
}

/// The `room = ...` line in the table spanning `start..end`.
fn room_line(lines: &[String], start: usize, end: usize) -> Option<usize> {
    (start + 1..end).find(|&at| {
        let line = lines[at].trim();
        !line.starts_with('#')
            && line
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == "room")
    })
}

/// The `[aliases]` table: its header line and the line its body ends before.
fn section(lines: &[String]) -> Option<(usize, usize)> {
    table(lines, "[aliases]")
}

/// The table `header` (as written): its header line and the line its body
/// ends before.
fn table(lines: &[String], header: &str) -> Option<(usize, usize)> {
    let at = lines.iter().position(|l| l.trim() == header)?;
    let end = lines[at + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |i| at + 1 + i);
    Some((at, end))
}

/// Where alias `name`'s entry starts in the `[aliases]` table, and how many
/// lines it spans (an array may continue over several).
fn find_entry(lines: &[String], name: &str) -> Option<(usize, usize)> {
    let (header, end) = section(lines)?;
    let wanted = normalize_room(name);
    for at in header + 1..end {
        let line = lines[at].trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if line.starts_with('#') || normalize_room(unquote(key.trim())) != wanted {
            continue;
        }
        let value = value.trim();
        let mut len = 1;
        if value.starts_with('[') && !value.contains(']') {
            while at + len < end && !lines[at + len - 1].contains(']') {
                len += 1;
            }
        }
        return Some((at, len));
    }
    None
}

fn unquote(key: &str) -> &str {
    key.strip_prefix('"')
        .and_then(|k| k.strip_suffix('"'))
        .unwrap_or(key)
}

/// A bare key when TOML allows one, else a quoted one.
fn toml_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        name.to_string()
    } else {
        toml::Value::String(name.to_string()).to_string()
    }
}

fn toml_value(rooms: &[String]) -> String {
    let quoted: Vec<String> = rooms
        .iter()
        .map(|r| toml::Value::String(r.clone()).to_string())
        .collect();
    match quoted.as_slice() {
        [one] => one.clone(),
        many => format!("[{}]", many.join(", ")),
    }
}

/// Apply `edit` to `aliases.toml` at `path` (empty when it does not exist),
/// check the result parses, and replace the file atomically.
pub fn edit_file(
    path: &Path,
    edit: impl FnOnce(&str) -> Result<String, Failure>,
) -> Result<(), Failure> {
    let before = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Failure::invalid(format!(
                "cannot read {}: {e}",
                path.display()
            )));
        }
    };
    let after = edit(&before)?;
    Aliases::parse(&after).map_err(|e| {
        Failure::invalid(e.to_string())
            .with_hint("Fix aliases.toml by hand first; this command edits only valid files.")
    })?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &after)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| Failure::invalid(format!("cannot write {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNERS: &str = "\
# The owner's own words.
[aliases]
kitchen = \"Kitchen Counter\"   # the long name
downstairs = [
  \"Kitchen Counter\",
  \"Living Room\",
]

[defaults]
room = \"Office\"
";

    #[test]
    fn adding_keeps_the_rest_of_the_file_as_it_was() {
        let out = set_alias(OWNERS, "den", &["Ada\u{2019}s Studio".into()]);
        assert!(out.starts_with("# The owner's own words.\n[aliases]\n"));
        assert!(out.contains("kitchen = \"Kitchen Counter\"   # the long name\n"));
        let den = out.find("den = \"Ada\u{2019}s Studio\"").unwrap();
        assert!(den < out.find("[defaults]").unwrap(), "inside [aliases]");
        assert!(out.ends_with("[defaults]\nroom = \"Office\"\n"));
        Aliases::parse(&out).unwrap();
    }

    #[test]
    fn replacing_and_removing_touch_one_entry() {
        let out = set_alias(OWNERS, "DOWNSTAIRS", &["Den".into(), "Office".into()]);
        assert!(out.contains("DOWNSTAIRS = [\"Den\", \"Office\"]\n"));
        assert!(!out.contains("\"Living Room\""), "the old array is gone");
        assert!(out.contains("# the long name"));
        let gone = remove_alias(&out, "downstairs").unwrap();
        assert!(!gone.contains("Den"));
        assert!(gone.contains("kitchen = "));
        assert_eq!(remove_alias(&gone, "nowhere"), None);
        Aliases::parse(&gone).unwrap();
    }

    #[test]
    fn the_default_room_is_set_where_it_stands_or_in_a_new_table() {
        let office = Aliases::parse(OWNERS).unwrap();
        assert_eq!(office.default_room(None), Some("Office"));

        let den = set_default_room(OWNERS, None, "Den");
        assert!(den.ends_with("[defaults]\nroom = \"Den\"\n"), "{den}");
        assert!(den.contains("# the long name"), "the rest stays");
        let alice = set_default_room(&den, Some("alice@example.com"), "Bedroom");
        assert!(
            alice.ends_with("\n[defaults.clients.\"alice@example.com\"]\nroom = \"Bedroom\"\n"),
            "{alice}"
        );
        let parsed = Aliases::parse(&alice).unwrap();
        assert_eq!(parsed.default_room(None), Some("Den"));
        assert_eq!(
            parsed.default_room(Some("alice@example.com")),
            Some("Bedroom")
        );
        assert_eq!(parsed.default_room(Some("bob@example.com")), Some("Den"));

        let cleared = clear_default_room(&alice, Some("alice@example.com")).unwrap();
        let parsed = Aliases::parse(&cleared).unwrap();
        assert_eq!(parsed.default_room(Some("alice@example.com")), Some("Den"));
        assert_eq!(
            clear_default_room(&cleared, Some("alice@example.com")),
            None
        );
        assert_eq!(
            set_default_room("", None, "Office"),
            "[defaults]\nroom = \"Office\"\n"
        );
    }

    #[test]
    fn a_file_without_aliases_gets_a_table() {
        let out = set_alias("[defaults]\nroom = \"Office\"\n", "up", &["Office".into()]);
        assert_eq!(
            out,
            "[defaults]\nroom = \"Office\"\n\n[aliases]\nup = \"Office\"\n"
        );
        assert_eq!(
            set_alias("", "my room", &["Office".into()]),
            "[aliases]\n\"my room\" = \"Office\"\n"
        );
    }
}
