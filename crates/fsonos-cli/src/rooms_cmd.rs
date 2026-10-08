//! `fsonos rooms`: the rooms with their aliases, and `fsonos rooms alias
//! add|rm`, which edits `aliases.toml` in the data directory.
//!
//! The edit touches only the alias's own entry in the `[aliases]` table
//! (added at the table's end, or replaced or removed where it stands), so the
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

/// The `[aliases]` table: its header line and the line its body ends before.
fn section(lines: &[String]) -> Option<(usize, usize)> {
    let header = lines.iter().position(|l| l.trim() == "[aliases]")?;
    let end = lines[header + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |i| header + 1 + i);
    Some((header, end))
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
