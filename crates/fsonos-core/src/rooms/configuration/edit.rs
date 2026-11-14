//! Source-preserving changes with parser-provided value spans. Unrecognized
//! removal/insertion layouts fail closed; complete before/after semantic
//! comparison prevents a textual edit from changing an unrelated setting.

use super::{AliasEdit, Aliases, ConfigurationError, check_size, invalid};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ops::Range;
use toml::{Spanned, Value};

#[derive(Deserialize, Default)]
struct Layout {
    #[serde(default)]
    aliases: BTreeMap<String, Spanned<Value>>,
    #[serde(default)]
    defaults: Defaults,
}

#[derive(Deserialize, Default)]
struct Defaults {
    room: Option<Spanned<String>>,
    #[serde(default)]
    clients: BTreeMap<String, Client>,
}

#[derive(Deserialize)]
struct Client {
    room: Spanned<String>,
}

#[derive(PartialEq, Eq)]
struct Fields {
    aliases: BTreeMap<String, Vec<String>>,
    global: Option<String>,
    clients: BTreeMap<String, String>,
}

impl From<&Aliases> for Fields {
    fn from(aliases: &Aliases) -> Self {
        Self {
            aliases: aliases
                .aliases()
                .iter()
                .map(|alias| (alias.name.clone(), alias.rooms.clone()))
                .collect(),
            global: aliases.global_default_room().map(str::to_owned),
            clients: aliases
                .client_defaults()
                .map(|(client, room)| (client.to_owned(), room.to_owned()))
                .collect(),
        }
    }
}

enum Target {
    Alias(String),
    Global,
    Client(String),
}

impl Target {
    fn table(&self) -> Vec<String> {
        match self {
            Self::Alias(_) => vec!["aliases".into()],
            Self::Global => vec!["defaults".into()],
            Self::Client(client) => vec!["defaults".into(), "clients".into(), client.clone()],
        }
    }

    fn key(&self) -> &str {
        match self {
            Self::Alias(name) => name,
            Self::Global | Self::Client(_) => "room",
        }
    }

    fn span(&self, layout: &Layout) -> Option<Range<usize>> {
        match self {
            Self::Alias(name) => layout.aliases.get(name).map(Spanned::span),
            Self::Global => layout.defaults.room.as_ref().map(Spanned::span),
            Self::Client(client) => layout.defaults.clients.get(client).map(|d| d.room.span()),
        }
    }
}

struct Plan {
    target: Target,
    value: Option<Value>,
}

fn plan(aliases: &Aliases, change: &AliasEdit, expected: &mut Fields) -> Option<Plan> {
    let (target, value) = match change {
        AliasEdit::SetAlias { name, rooms } => {
            let existing = aliases.get(name);
            if existing.is_some_and(|alias| alias.rooms == *rooms) {
                return None;
            }
            // Name matching follows the resolver, retaining the owner's
            // original spelling when updating an existing identity.
            let name = existing.map_or_else(|| name.clone(), |alias| alias.name.clone());
            expected.aliases.insert(name.clone(), rooms.clone());
            let value = match rooms.as_slice() {
                [room] => Value::String(room.clone()),
                many => Value::Array(many.iter().cloned().map(Value::String).collect()),
            };
            (Target::Alias(name), Some(value))
        }
        AliasEdit::RemoveAlias { name } => {
            let alias = aliases.get(name)?;
            expected.aliases.remove(&alias.name);
            (Target::Alias(alias.name.clone()), None)
        }
        AliasEdit::SetDefault { client, room } => {
            let target = if let Some(client) = client {
                if expected.clients.get(client) == Some(room) {
                    return None;
                }
                expected.clients.insert(client.clone(), room.clone());
                Target::Client(client.clone())
            } else {
                if expected.global.as_ref() == Some(room) {
                    return None;
                }
                expected.global = Some(room.clone());
                Target::Global
            };
            (target, Some(Value::String(room.clone())))
        }
        AliasEdit::ClearDefault { client } => {
            if let Some(client) = client {
                expected.clients.remove(client)?;
                (Target::Client(client.clone()), None)
            } else {
                expected.global.take()?;
                (Target::Global, None)
            }
        }
    };
    Some(Plan { target, value })
}

/// Apply one edit to a valid document without changing unrelated source.
/// A semantic no-op returns the exact original bytes, including comments,
/// line endings and whether the document ends with a newline.
pub fn edit(text: &str, change: &AliasEdit) -> Result<String, ConfigurationError> {
    change.validate()?;
    check_size(text.as_bytes())?;
    let before = Aliases::parse(text)?;
    let mut expected = Fields::from(&before);
    let Some(plan) = plan(&before, change, &mut expected) else {
        return Ok(text.to_owned());
    };
    let layout: Layout = toml::from_str(text).map_err(|_| unsupported())?;
    let after = match (plan.target.span(&layout), &plan.value) {
        (Some(span), Some(value)) => replace(text, span, &value.to_string())?,
        (None, Some(value)) => insert(text, &layout, &plan.target, value),
        (Some(span), None) => remove(text, &layout, &plan.target, span)?,
        (None, None) => return Err(unsupported()),
    };
    check_size(after.as_bytes())?;
    let parsed = Aliases::parse(&after).map_err(|_| unsupported())?;
    if Fields::from(&parsed) != expected {
        return Err(unsupported());
    }
    Ok(after)
}

fn unsupported() -> ConfigurationError {
    invalid(
        "aliases.toml uses a layout this edit cannot safely change; update that entry in the file directly",
    )
}

fn replace(text: &str, span: Range<usize>, value: &str) -> Result<String, ConfigurationError> {
    if text.get(span.clone()).is_none() {
        return Err(unsupported());
    }
    let mut after = text.to_owned();
    after.replace_range(span, value);
    Ok(after)
}

struct Header {
    path: Vec<String>,
    start: usize,
    end: usize,
}

/// Inspect only header lines outside parsed values. A multiline string may
/// itself contain `[aliases]` or a table-like room name.
fn headers(text: &str, layout: &Layout) -> Vec<Header> {
    let spans: Vec<_> = layout
        .aliases
        .values()
        .map(Spanned::span)
        .chain(layout.defaults.room.iter().map(Spanned::span))
        .chain(
            layout
                .defaults
                .clients
                .values()
                .map(|client| client.room.span()),
        )
        .collect();
    let mut offset = 0;
    let mut result = Vec::new();
    for line in text.split_inclusive('\n') {
        if !spans.iter().any(|span| span.contains(&offset))
            && line.trim_start().starts_with('[')
            && let Some(path) = header_path(line)
        {
            result.push(Header {
                path,
                start: offset,
                end: offset + line.len(),
            });
        }
        offset += line.len();
    }
    result
}

fn header_path(line: &str) -> Option<Vec<String>> {
    let mut value: Value = toml::from_str(line).ok()?;
    let mut path = Vec::new();
    loop {
        let Value::Table(table) = value else {
            return None;
        };
        if table.is_empty() {
            return (!path.is_empty()).then_some(path);
        }
        if table.len() != 1 {
            return None;
        }
        let (key, child) = table.into_iter().next()?;
        path.push(key);
        value = child;
    }
}

fn newline(text: &str) -> &'static str {
    if text
        .find('\n')
        .is_some_and(|at| at > 0 && text.as_bytes()[at - 1] == b'\r')
    {
        "\r\n"
    } else {
        "\n"
    }
}

fn key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        name.to_owned()
    } else {
        Value::String(name.to_owned()).to_string()
    }
}

fn insert(text: &str, layout: &Layout, target: &Target, value: &Value) -> String {
    let headers = headers(text, layout);
    let table = target.table();
    let newline = newline(text);
    let entry = format!("{} = {value}{newline}", key(target.key()));
    if let Some(index) = headers.iter().position(|header| header.path == table) {
        let at = headers
            .get(index + 1)
            .map_or(text.len(), |header| header.start);
        let separator = if at > 0 && !text[..at].ends_with('\n') {
            newline
        } else {
            ""
        };
        return format!("{}{separator}{entry}{}", &text[..at], &text[at..]);
    }
    let separator = if text.is_empty() {
        String::new()
    } else if text.ends_with('\n') {
        newline.to_owned()
    } else {
        newline.repeat(2)
    };
    let header = table
        .iter()
        .map(|name| key(name))
        .collect::<Vec<_>>()
        .join(".");
    format!("{text}{separator}[{header}]{newline}{entry}")
}

/// A removal must occupy one standalone assignment. Inline tables and dotted
/// keys are left intact when their safe removal cannot be proved.
fn assignment(
    text: &str,
    value: Range<usize>,
    key: &str,
) -> Result<Range<usize>, ConfigurationError> {
    if text.get(value.clone()).is_none() {
        return Err(unsupported());
    }
    let start = text[..value.start].rfind('\n').map_or(0, |at| at + 1);
    let prefix = format!("{}0", &text[start..value.start]);
    let parsed: Value = toml::from_str(&prefix).map_err(|_| unsupported())?;
    let table = parsed.as_table().ok_or_else(unsupported)?;
    if table.len() != 1 || table.get(key) != Some(&Value::Integer(0)) {
        return Err(unsupported());
    }
    let end = text[value.end..]
        .find('\n')
        .map_or(text.len(), |at| value.end + at + 1);
    let suffix = text[value.end..end].trim();
    if !suffix.is_empty() && !suffix.starts_with('#') {
        return Err(unsupported());
    }
    Ok(start..end)
}

fn remove(
    text: &str,
    layout: &Layout,
    target: &Target,
    value: Range<usize>,
) -> Result<String, ConfigurationError> {
    let assignment = assignment(text, value, target.key())?;
    if !matches!(target, Target::Client(_)) {
        return replace(text, assignment, "");
    }
    let headers = headers(text, layout);
    let table = target.table();
    let index = headers
        .iter()
        .position(|header| header.path == table)
        .ok_or_else(unsupported)?;
    let header = &headers[index];
    let end = headers.get(index + 1).map_or(text.len(), |next| next.start);
    if assignment.start < header.end || assignment.end > end {
        return Err(unsupported());
    }
    // An explicit client table cannot remain without its required room.
    // Retain its comments, removing only the header and room assignment.
    let after = replace(text, assignment, "")?;
    replace(&after, header.start..header.end, "")
}
