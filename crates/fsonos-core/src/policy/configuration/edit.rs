//! Preserve authored policy text, using the TOML parser's value spans. A full
//! semantic comparison proves that a narrow edit did not change another rule.

use super::{ConfigurationError, McpProfile, Policy, PolicyEdit, check_size, invalid};
use crate::rooms::normalize_room;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::Range;
use toml::{Spanned, Table, Value};

type Fields = BTreeMap<String, Spanned<Value>>;

#[derive(Deserialize, Default)]
struct Layout {
    defaults: Option<Spanned<Fields>>,
    quiet_hours: Option<Spanned<Fields>>,
    #[serde(default)]
    rooms: BTreeMap<String, Spanned<Fields>>,
    #[serde(default)]
    clients: BTreeMap<String, Spanned<Fields>>,
}

impl Layout {
    fn table(&self, path: &[String]) -> Option<&Spanned<Fields>> {
        match path {
            [section] if section == "defaults" => self.defaults.as_ref(),
            [section] if section == "quiet_hours" => self.quiet_hours.as_ref(),
            [section, name] if section == "rooms" => self.rooms.get(name),
            [section, name] if section == "clients" => self.clients.get(name),
            _ => None,
        }
    }

    fn spans(&self) -> Vec<Range<usize>> {
        self.defaults
            .iter()
            .chain(self.quiet_hours.iter())
            .chain(self.rooms.values())
            .chain(self.clients.values())
            .flat_map(|table| table.get_ref().values().map(Spanned::span))
            .collect()
    }
}

struct Plan {
    path: Vec<String>,
    fields: Option<Table>,
}

fn table<const N: usize>(fields: [(&str, Value); N]) -> Table {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn integer(value: u8) -> Value {
    Value::Integer(i64::from(value))
}
fn strings(values: &[String]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

fn identity(before: &Value, section: &str, name: &str) -> String {
    before
        .get(section)
        .and_then(Value::as_table)
        .and_then(|entries| {
            entries.keys().find(|key| {
                if section == "rooms" {
                    normalize_room(key) == normalize_room(name)
                } else {
                    key.to_lowercase() == name.to_lowercase()
                }
            })
        })
        .cloned()
        .unwrap_or_else(|| name.to_owned())
}

fn plan(before: &Value, change: &PolicyEdit) -> Plan {
    let (path, fields) = match change {
        PolicyEdit::SetDefaults {
            max_volume,
            max_step,
            fade_secs,
        } => (
            vec!["defaults".into()],
            Some(table([
                ("max_volume", integer(*max_volume)),
                ("max_step", integer(*max_step)),
                ("fade_secs", Value::Integer(i64::from(*fade_secs))),
            ])),
        ),
        PolicyEdit::SetQuietHours {
            start,
            end,
            max_volume,
        } => (
            vec!["quiet_hours".into()],
            Some(table([
                ("start", Value::String(start.clone())),
                ("end", Value::String(end.clone())),
                ("max_volume", integer(*max_volume)),
            ])),
        ),
        PolicyEdit::ClearQuietHours => (vec!["quiet_hours".into()], None),
        PolicyEdit::SetRoom {
            room,
            max_volume,
            max_step,
        } => {
            let mut fields = Table::new();
            if let Some(value) = max_volume {
                fields.insert("max_volume".into(), integer(*value));
            }
            if let Some(value) = max_step {
                fields.insert("max_step".into(), integer(*value));
            }
            (
                vec!["rooms".into(), identity(before, "rooms", room)],
                Some(fields),
            )
        }
        PolicyEdit::RemoveRoom { room } => {
            (vec!["rooms".into(), identity(before, "rooms", room)], None)
        }
        PolicyEdit::SetClient {
            client,
            allow,
            deny,
            grant,
            capped,
            manage_others,
            mcp_profile,
        } => {
            let mut fields = Table::new();
            if let Some(value) = allow {
                fields.insert("allow".into(), strings(value));
            }
            if !deny.is_empty() {
                fields.insert("deny".into(), strings(deny));
            }
            if !grant.is_empty() {
                fields.insert("grant".into(), strings(grant));
            }
            if let Some(value) = capped {
                fields.insert("capped".into(), Value::Boolean(*value));
            }
            if *manage_others {
                fields.insert("manage_others".into(), Value::Boolean(true));
            }
            if let Some(value) = mcp_profile {
                let name = match value {
                    McpProfile::Essentials => "essentials",
                    McpProfile::Full => "full",
                };
                fields.insert("mcp_profile".into(), Value::String(name.into()));
            }
            (
                vec!["clients".into(), identity(before, "clients", client)],
                Some(fields),
            )
        }
        PolicyEdit::RemoveClient { client } => (
            vec!["clients".into(), identity(before, "clients", client)],
            None,
        ),
    };
    Plan { path, fields }
}

fn expected(before: &Value, plan: &Plan) -> Result<Value, ConfigurationError> {
    let mut result = before.clone();
    let mut parent = result.as_table_mut().ok_or_else(unsupported)?;
    for section in &plan.path[..plan.path.len() - 1] {
        if !parent.contains_key(section) && plan.fields.is_none() {
            return Ok(result);
        }
        parent = parent
            .entry(section.clone())
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(unsupported)?;
    }
    let key = plan.path.last().ok_or_else(unsupported)?;
    if let Some(fields) = &plan.fields {
        parent.insert(key.clone(), Value::Table(fields.clone()));
    } else {
        parent.remove(key);
    }
    Ok(result)
}

/// Edit one complete section or rule. Effective no-ops retain every original
/// byte, including comments and the absence of an optional configuration file.
pub fn edit(text: &str, change: &PolicyEdit) -> Result<String, ConfigurationError> {
    change.validate()?;
    check_size(text.as_bytes())?;
    let before = Policy::from_toml(text)?;
    let raw: Value = toml::from_str(text).map_err(|_| unsupported())?;
    let plan = plan(&raw, change);
    let expected = expected(&raw, &plan)?;
    let canonical = toml::to_string(&expected).map_err(|_| unsupported())?;
    let wanted = Policy::from_toml(&canonical)?;
    if before.view() == wanted.view() {
        return Ok(text.to_owned());
    }
    let layout: Layout = toml::from_str(text).map_err(|_| unsupported())?;
    let after = if let Some(existing) = layout.table(&plan.path) {
        revise(text, &layout, &plan, existing)?
    } else if let Some(fields) = &plan.fields {
        append(text, &plan.path, fields)
    } else {
        return Err(unsupported());
    };
    check_size(after.as_bytes())?;
    let actual: Value = toml::from_str(&after).map_err(|_| unsupported())?;
    if without_empty_containers(actual) != without_empty_containers(expected)
        || Policy::from_toml(&after)?.view() != wanted.view()
    {
        return Err(unsupported());
    }
    Ok(after)
}

/// Removing the last child may remove an implicit parent table; an explicitly
/// authored empty parent can remain. Neither form carries policy settings.
fn without_empty_containers(mut value: Value) -> Value {
    if let Some(table) = value.as_table_mut() {
        for name in ["rooms", "clients"] {
            if table
                .get(name)
                .and_then(Value::as_table)
                .is_some_and(Table::is_empty)
            {
                table.remove(name);
            }
        }
    }
    value
}

fn unsupported() -> ConfigurationError {
    invalid(
        "policy.toml uses a layout this edit cannot safely change; update that entry in the file directly",
    )
}

struct Header {
    path: Vec<String>,
    start: usize,
    end: usize,
}

fn headers(text: &str, layout: &Layout) -> Vec<Header> {
    let spans = layout.spans();
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
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
    {
        name.to_owned()
    } else {
        Value::String(name.to_owned()).to_string()
    }
}

fn entries(fields: &Table, newline: &str) -> String {
    let mut result = String::new();
    for (name, value) in fields {
        write!(result, "{} = {value}{newline}", key(name))
            .expect("writing into String cannot fail");
    }
    result
}

fn append(text: &str, path: &[String], fields: &Table) -> String {
    let newline = newline(text);
    let separator = if text.is_empty() {
        String::new()
    } else if text.ends_with('\n') {
        newline.to_owned()
    } else {
        newline.repeat(2)
    };
    let path = path
        .iter()
        .map(|name| key(name))
        .collect::<Vec<_>>()
        .join(".");
    format!(
        "{text}{separator}[{path}]{newline}{}",
        entries(fields, newline)
    )
}

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

fn revise(
    text: &str,
    layout: &Layout,
    plan: &Plan,
    existing: &Spanned<Fields>,
) -> Result<String, ConfigurationError> {
    let headers = headers(text, layout);
    let mut edits = Vec::new();
    if let Some(index) = headers.iter().position(|header| header.path == plan.path) {
        let header = &headers[index];
        let end = headers.get(index + 1).map_or(text.len(), |next| next.start);
        for (name, value) in existing.get_ref() {
            let span = value.span();
            if span.start < header.end || span.end > end {
                return Err(unsupported());
            }
            if let Some(next) = plan.fields.as_ref().and_then(|fields| fields.get(name)) {
                if value.get_ref() != next {
                    edits.push((span, next.to_string()));
                }
            } else {
                edits.push((assignment(text, span, name)?, String::new()));
            }
        }
        if let Some(fields) = &plan.fields {
            let added: Table = fields
                .iter()
                .filter(|(name, _)| !existing.get_ref().contains_key(*name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            if !added.is_empty() {
                let separator = if text[..header.end].ends_with('\n') {
                    ""
                } else {
                    newline(text)
                };
                edits.push((
                    header.end..header.end,
                    format!("{separator}{}", entries(&added, newline(text))),
                ));
            }
        } else {
            edits.push((header.start..header.end, String::new()));
        }
    } else {
        let span = existing.span();
        let source = text.get(span.clone()).ok_or_else(unsupported)?;
        if !source.trim().starts_with('{') || !source.trim().ends_with('}') {
            return Err(unsupported());
        }
        if let Some(fields) = &plan.fields {
            edits.push((span, Value::Table(fields.clone()).to_string()));
        } else {
            edits.push((
                assignment(text, span, plan.path.last().ok_or_else(unsupported)?)?,
                String::new(),
            ));
        }
    }
    edits.sort_by_key(|(range, _)| (range.start, range.end));
    if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
        return Err(unsupported());
    }
    let mut after = text.to_owned();
    for (range, value) in edits.into_iter().rev() {
        if after.get(range.clone()).is_none() {
            return Err(unsupported());
        }
        after.replace_range(range, &value);
    }
    Ok(after)
}
