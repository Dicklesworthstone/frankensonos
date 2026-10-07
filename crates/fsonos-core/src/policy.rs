//! House policy: which tools each client may use, and how loud it may make
//! the house.
//!
//! Several agents can drive the speakers from anywhere on the tailnet; people
//! hand the house to agents only if they can bound what the agents do (no
//! volume 80 at 2 a.m.). The policy is read from `policy.toml` in the data
//! directory — a missing file means the built-in defaults — and is pure:
//!
//! * [`Policy::authorize`] gates every tool call, reads included.
//! * [`Policy::evaluate`] bounds a volume change by the room caps, the
//!   quiet-hours cap and the per-step limit, given the current volumes.
//!
//! Enforcement is central (every surface asks the same [`Policy`]), so the
//! HTTP API, MCP server and CLI agree.
//!
//! ```toml
//! [defaults]
//! max_volume = 70        # per room
//! max_step = 20          # largest single increase
//! fade_secs = 0
//!
//! [quiet_hours]          # local wall-clock time; may wrap midnight
//! start = "22:00"
//! end = "07:00"
//! max_volume = 25
//!
//! [rooms."Bedroom"]
//! max_volume = 40
//!
//! [clients."alice@example.com"]
//! allow = ["list_zones", "get_zone_state", "play", "set_volume"]
//!
//! [clients.cli]
//! capped = true          # the CLI is uncapped by default
//! ```
//!
//! Clients are keyed by [`Client::key`]: `cli`, `mcp-stdio`,
//! `loopback-http`, a tailnet principal (login, `tag:<name>` or node name),
//! or `unknown`.

use crate::rooms::normalize_room;
use chrono::{DateTime, FixedOffset, NaiveTime};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The policy file's name inside the data directory.
pub const FILE_NAME: &str = "policy.toml";

/// Who is asking: the surface and, over the tailnet, the principal. Agents
/// are never "local" — the agent-safety rules are about them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Client {
    /// A person at the terminal.
    Cli,
    /// A local agent speaking MCP over stdio.
    McpStdio,
    /// A local process over HTTP or MCP-over-HTTP on loopback.
    LoopbackHttp,
    /// A resolved tailnet principal: login, `tag:<name>`, or node name.
    Tailnet(String),
    /// A caller that could not be identified.
    Unknown,
}

impl Client {
    /// The `[clients."<key>"]` table this client is configured under.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Cli => "cli",
            Self::McpStdio => "mcp-stdio",
            Self::LoopbackHttp => "loopback-http",
            Self::Tailnet(principal) => principal,
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// A requested volume change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeChange {
    /// Set the level (0–100).
    Set(u8),
    /// Raise (positive) or lower (negative) by this much.
    Adjust(i8),
}

/// A room's current volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomLevel {
    pub room: String,
    pub volume: u8,
}

/// A volume change and the current level of every room it lands on: one
/// room, or each member of a group (a group change moves the members
/// proportionally, as Sonos group volume does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeIntent {
    pub change: VolumeChange,
    pub rooms: Vec<RoomLevel>,
}

/// A tool call to authorize.
#[derive(Debug, Clone, Copy)]
pub struct ToolCall<'a> {
    pub name: &'a str,
    /// The tool only reads state.
    pub read_only: bool,
}

/// The policy's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allowed, but only as `allowed`.
    Clamp {
        requested: VolumeChange,
        allowed: VolumeChange,
        reason: String,
    },
    Deny {
        reason: String,
    },
}

/// Limits that apply to every room unless the room overrides them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_volume: u8,
    /// The largest single increase.
    pub max_step: u8,
    /// Ramp length for volume changes the daemon makes on its own.
    pub fade_secs: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_volume: 70,
            max_step: 20,
            fade_secs: 0,
        }
    }
}

/// A nightly window with a lower cap, in local wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub max_volume: u8,
}

impl QuietHours {
    /// Is `t` inside the window? The window may wrap midnight; `end` is
    /// exclusive.
    #[must_use]
    pub fn contains(&self, t: NaiveTime) -> bool {
        if self.start < self.end {
            self.start <= t && t < self.end
        } else {
            t >= self.start || t < self.end
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RoomLimits {
    max_volume: Option<u8>,
    max_step: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ClientRules {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
    capped: Option<bool>,
}

/// The house policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub defaults: Limits,
    pub quiet_hours: Option<QuietHours>,
    /// Keyed by [`normalize_room`].
    rooms: HashMap<String, RoomLimits>,
    /// Keyed by lowercased [`Client::key`].
    clients: HashMap<String, ClientRules>,
}

/// A policy file that cannot be used, with where the problem is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError {
    pub path: Option<PathBuf>,
    /// 1-based line and column, when the problem has a location.
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}", path.display())?,
            None => f.write_str(FILE_NAME)?,
        }
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(f, " line {line}, column {column}")?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for PolicyError {}

impl Policy {
    /// Load `policy.toml` from `data_dir`; a missing file is the defaults.
    pub fn load(data_dir: &Path) -> Result<Self, PolicyError> {
        let path = data_dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml(&text).map_err(|e| PolicyError {
                path: Some(path),
                ..e
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PolicyError {
                path: Some(path),
                line: None,
                column: None,
                message: format!("cannot read the policy file: {e}"),
            }),
        }
    }

    /// Parse a policy document.
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        let raw: RawPolicy = toml::from_str(text).map_err(|e| {
            let (line, column) = e.span().map(|span| line_col(text, span.start)).unzip();
            PolicyError {
                path: None,
                line,
                column,
                message: e.message().trim().to_owned(),
            }
        })?;
        let mut rooms = HashMap::new();
        for (name, room) in raw.rooms {
            if rooms.insert(normalize_room(&name), room.into()).is_some() {
                return Err(PolicyError {
                    path: None,
                    line: None,
                    column: None,
                    message: format!(
                        "[rooms] names {name:?} twice (names match case-insensitively)"
                    ),
                });
            }
        }
        let mut clients = HashMap::new();
        for (key, client) in raw.clients {
            if clients.insert(key.to_lowercase(), client.into()).is_some() {
                return Err(PolicyError {
                    path: None,
                    line: None,
                    column: None,
                    message: format!(
                        "[clients] names {key:?} twice (keys match case-insensitively)"
                    ),
                });
            }
        }
        let fallback = Limits::default();
        Ok(Self {
            defaults: Limits {
                max_volume: raw.defaults.max_volume.map_or(fallback.max_volume, |v| v.0),
                max_step: raw.defaults.max_step.map_or(fallback.max_step, |v| v.0),
                fade_secs: raw.defaults.fade_secs.unwrap_or(fallback.fade_secs),
            },
            quiet_hours: raw.quiet_hours.map(|q| q.0),
            rooms,
            clients,
        })
    }

    fn client_rules(&self, client: &Client) -> Option<&ClientRules> {
        self.clients.get(&client.key().to_lowercase())
    }

    /// Do volume caps apply to `client`? Everyone but the CLI by default.
    #[must_use]
    pub fn is_capped(&self, client: &Client) -> bool {
        self.client_rules(client)
            .and_then(|r| r.capped)
            .unwrap_or(*client != Client::Cli)
    }

    /// Is `now` inside quiet hours?
    #[must_use]
    pub fn quiet_hours_active(&self, now: DateTime<FixedOffset>) -> bool {
        self.quiet_hours.is_some_and(|q| q.contains(now.time()))
    }

    /// The highest level `room` may be set to at `now`, and why.
    fn cap(&self, room: &str, now: DateTime<FixedOffset>) -> (u8, String) {
        let room_max = self
            .rooms
            .get(&normalize_room(room))
            .and_then(|r| r.max_volume)
            .unwrap_or(self.defaults.max_volume);
        match self.quiet_hours {
            Some(q) if q.contains(now.time()) && q.max_volume < room_max => (
                q.max_volume,
                format!(
                    "{room} is capped at {} during quiet hours ({}–{})",
                    q.max_volume,
                    q.start.format("%H:%M"),
                    q.end.format("%H:%M")
                ),
            ),
            _ => (room_max, format!("{room} is capped at {room_max}")),
        }
    }

    fn step(&self, room: &str) -> u8 {
        self.rooms
            .get(&normalize_room(room))
            .and_then(|r| r.max_step)
            .unwrap_or(self.defaults.max_step)
    }

    /// May `client` call `tool`? Reads go through here too: surfaces call it
    /// for every tool before dispatch.
    #[must_use]
    pub fn authorize(&self, tool: ToolCall<'_>, client: &Client) -> Decision {
        if let Some(rules) = self.client_rules(client) {
            if rules.deny.iter().any(|t| t == tool.name) {
                return Decision::Deny {
                    reason: format!("policy denies {} to {client}", tool.name),
                };
            }
            if let Some(allow) = &rules.allow {
                return if allow.iter().any(|t| t == tool.name) {
                    Decision::Allow
                } else {
                    Decision::Deny {
                        reason: format!(
                            "{client} may only use: {}",
                            if allow.is_empty() {
                                "no tools".to_owned()
                            } else {
                                allow.join(", ")
                            }
                        ),
                    }
                };
            }
        }
        if *client == Client::Unknown && !tool.read_only {
            return Decision::Deny {
                reason: format!(
                    "unidentified callers may only use read-only tools, not {}; give this \
                     caller a [clients] entry in {FILE_NAME}",
                    tool.name
                ),
            };
        }
        Decision::Allow
    }

    /// Bound a volume change. Increases are limited by each room's cap
    /// (lower during quiet hours) and by `max_step`; decreases are always
    /// allowed, but never left above a cap. Raising a room already at its cap
    /// is denied rather than silently lowered. Uncapped clients are allowed
    /// anything.
    #[must_use]
    pub fn evaluate(
        &self,
        intent: &VolumeIntent,
        client: &Client,
        now: DateTime<FixedOffset>,
    ) -> Decision {
        let rooms = &intent.rooms;
        if rooms.is_empty() || !self.is_capped(client) {
            return Decision::Allow;
        }
        let current: Vec<u8> = rooms.iter().map(|r| r.volume.min(100)).collect();
        let caps: Vec<(u8, String)> = rooms.iter().map(|r| self.cap(&r.room, now)).collect();
        let step = rooms.iter().map(|r| self.step(&r.room)).min().unwrap_or(0);
        let level = group_level(&current);
        let target = match intent.change {
            VolumeChange::Set(v) => v.min(100),
            VolumeChange::Adjust(d) => clamp_level(i16::from(level) + i16::from(d)),
        };
        // The first room pushed over its cap if the group moves to `to`.
        let over_cap = |to: u8| {
            members_at(&current, level, to)
                .zip(&caps)
                .position(|(member, (cap, _))| member > *cap)
        };
        let allowed = |to: u8| match intent.change {
            VolumeChange::Set(_) => VolumeChange::Set(to),
            VolumeChange::Adjust(_) => {
                VolumeChange::Adjust(i8::try_from(i16::from(to) - i16::from(level)).unwrap_or(0))
            }
        };
        let clamp = |to: u8, reason: String| Decision::Clamp {
            requested: intent.change,
            allowed: allowed(to),
            reason,
        };

        if target <= level {
            return match over_cap(target) {
                None => Decision::Allow,
                Some(room) => {
                    let to = (0..target)
                        .rev()
                        .find(|&l| over_cap(l).is_none())
                        .unwrap_or(0);
                    clamp(to, caps[room].1.clone())
                }
            };
        }
        let ceiling = target.min(level.saturating_add(step));
        match (level + 1..=ceiling).rev().find(|&l| over_cap(l).is_none()) {
            Some(to) if to == target => Decision::Allow,
            Some(to) if to == ceiling => clamp(
                to,
                format!("volume rises at most {step} at a time (max_step)"),
            ),
            Some(to) => {
                let room = over_cap(to + 1).unwrap_or(0);
                clamp(to, caps[room].1.clone())
            }
            None => {
                let room = over_cap(level + 1).unwrap_or(0);
                Decision::Deny {
                    reason: format!("{}; it is already at {}", caps[room].1, rooms[room].volume),
                }
            }
        }
    }
}

/// Sonos group volume: the rounded mean of the members.
fn group_level(members: &[u8]) -> u8 {
    let n = u32::try_from(members.len()).unwrap_or(u32::MAX).max(1);
    let sum: u32 = members.iter().map(|&v| u32::from(v)).sum();
    u8::try_from((2 * sum + n) / (2 * n)).unwrap_or(100)
}

/// Member levels after moving the group from `from` to `to`: proportional
/// to each member's share (equal when the group is silent).
fn members_at(current: &[u8], from: u8, to: u8) -> impl Iterator<Item = u8> + '_ {
    current.iter().map(move |&c| {
        if current.len() == 1 || from == 0 {
            to
        } else {
            let scaled =
                (2 * u32::from(c) * u32::from(to) + u32::from(from)) / (2 * u32::from(from));
            u8::try_from(scaled.min(100)).unwrap_or(100)
        }
    })
}

fn clamp_level(v: i16) -> u8 {
    u8::try_from(v.clamp(0, 100)).unwrap_or(0)
}

/// 1-based line and character column of byte offset `at` in `text`.
fn line_col(text: &str, at: usize) -> (usize, usize) {
    let before = &text[..at.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before, |nl| &before[nl + 1..])
        .chars()
        .count()
        + 1;
    (line, column)
}

// ── The TOML schema. Validation happens while deserializing, so the toml
// crate attaches the offending value's location to every error. ──────────

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default)]
    quiet_hours: Option<RawQuietHours>,
    #[serde(default)]
    rooms: HashMap<String, RawRoom>,
    #[serde(default)]
    clients: HashMap<String, RawClient>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    max_volume: Option<Volume>,
    max_step: Option<Volume>,
    fade_secs: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoom {
    max_volume: Option<Volume>,
    max_step: Option<Volume>,
}

impl From<RawRoom> for RoomLimits {
    fn from(raw: RawRoom) -> Self {
        Self {
            max_volume: raw.max_volume.map(|v| v.0),
            max_step: raw.max_step.map(|v| v.0),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClient {
    allow: Option<Vec<String>>,
    #[serde(default)]
    deny: Vec<String>,
    capped: Option<bool>,
}

impl From<RawClient> for ClientRules {
    fn from(raw: RawClient) -> Self {
        Self {
            allow: raw.allow,
            deny: raw.deny,
            capped: raw.capped,
        }
    }
}

/// A volume level, 0–100.
struct Volume(u8);

impl<'de> Deserialize<'de> for Volume {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = i64::deserialize(d)?;
        u8::try_from(n)
            .ok()
            .filter(|v| *v <= 100)
            .map(Volume)
            .ok_or_else(|| D::Error::custom(format!("volume must be between 0 and 100, not {n}")))
    }
}

/// A wall-clock time written `"HH:MM"`.
struct ClockTime(NaiveTime);

impl<'de> Deserialize<'de> for ClockTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        NaiveTime::parse_from_str(&s, "%H:%M")
            .map(ClockTime)
            .map_err(|_| D::Error::custom(format!("expected a time like \"22:00\", not {s:?}")))
    }
}

struct RawQuietHours(QuietHours);

impl<'de> Deserialize<'de> for RawQuietHours {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            start: ClockTime,
            end: ClockTime,
            max_volume: Volume,
        }
        let f = Fields::deserialize(d)?;
        if f.start.0 == f.end.0 {
            return Err(D::Error::custom(
                "quiet_hours start and end are the same time; remove [quiet_hours] to disable it",
            ));
        }
        Ok(Self(QuietHours {
            start: f.start.0,
            end: f.end.0,
            max_volume: f.max_volume.0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hhmm: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(&format!("2026-10-07T{hhmm}:00-04:00")).unwrap()
    }

    const NOON: &str = "12:00";

    fn room(name: &str, volume: u8) -> RoomLevel {
        RoomLevel {
            room: name.into(),
            volume,
        }
    }

    fn intent(change: VolumeChange, rooms: &[(&str, u8)]) -> VolumeIntent {
        VolumeIntent {
            change,
            rooms: rooms.iter().map(|&(n, v)| room(n, v)).collect(),
        }
    }

    fn agent() -> Client {
        Client::Tailnet("tag:assistant".into())
    }

    fn allowed(d: &Decision) -> Option<VolumeChange> {
        match d {
            Decision::Clamp { allowed, .. } => Some(*allowed),
            _ => None,
        }
    }

    const QUIET: &str = r#"
[quiet_hours]
start = "22:00"
end = "07:00"
max_volume = 25
"#;

    #[test]
    fn defaults_cap_agents_and_leave_the_cli_alone() {
        let p = Policy::default();
        let set = |v, cur| intent(VolumeChange::Set(v), &[("Kitchen", cur)]);
        assert_eq!(
            p.evaluate(&set(65, 50), &agent(), at(NOON)),
            Decision::Allow
        );
        // Step-bound: 30 → at most 50.
        let d = p.evaluate(&set(90, 30), &agent(), at(NOON));
        assert_eq!(allowed(&d), Some(VolumeChange::Set(50)));
        assert!(matches!(&d, Decision::Clamp { reason, .. } if reason.contains("max_step")));
        // Cap-bound: 60 → at most 70.
        let d = p.evaluate(&set(80, 60), &agent(), at(NOON));
        assert_eq!(allowed(&d), Some(VolumeChange::Set(70)));
        assert!(
            matches!(&d, Decision::Clamp { reason, .. } if reason == "Kitchen is capped at 70")
        );
        // The person at the terminal is uncapped unless the file says so.
        assert_eq!(
            p.evaluate(&set(100, 10), &Client::Cli, at(NOON)),
            Decision::Allow
        );
        let capped = Policy::from_toml("[clients.cli]\ncapped = true\n").unwrap();
        assert_eq!(
            allowed(&capped.evaluate(&set(100, 60), &Client::Cli, at(NOON))),
            Some(VolumeChange::Set(70))
        );
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let p = Policy::from_toml(QUIET).unwrap();
        for (time, cap) in [
            ("21:59", 70),
            ("22:00", 25),
            ("23:30", 25),
            ("00:00", 25),
            ("06:59", 25),
            ("07:00", 70),
        ] {
            assert_eq!(p.quiet_hours_active(at(time)), cap == 25, "{time}");
            let d = p.evaluate(
                &intent(VolumeChange::Set(90), &[("Den", 10)]),
                &agent(),
                at(time),
            );
            let expect = VolumeChange::Set(cap.min(30));
            assert_eq!(allowed(&d), Some(expect), "{time}");
        }
        // A window that does not wrap.
        let day = Policy::from_toml(
            "[quiet_hours]\nstart = \"13:00\"\nend = \"15:00\"\nmax_volume = 10\n",
        )
        .unwrap();
        assert!(day.quiet_hours_active(at("14:00")));
        assert!(!day.quiet_hours_active(at("15:00")));
        assert!(!day.quiet_hours_active(at("12:59")));
    }

    #[test]
    fn room_overrides_match_names_loosely() {
        let p = Policy::from_toml(
            "[rooms.\"Bedroom\"]\nmax_volume = 40\n\n[rooms.\"Ada's Studio\"]\nmax_step = 5\n",
        )
        .unwrap();
        let d = p.evaluate(
            &intent(VolumeChange::Set(60), &[("Bedroom", 30)]),
            &agent(),
            at(NOON),
        );
        assert_eq!(allowed(&d), Some(VolumeChange::Set(40)));
        assert!(
            matches!(&d, Decision::Clamp { reason, .. } if reason == "Bedroom is capped at 40")
        );
        // Curly apostrophe and case still find the override.
        let d = p.evaluate(
            &intent(VolumeChange::Adjust(10), &[("ada\u{2019}s studio", 20)]),
            &agent(),
            at(NOON),
        );
        assert_eq!(allowed(&d), Some(VolumeChange::Adjust(5)));
        // Other rooms keep the defaults.
        assert_eq!(
            p.evaluate(
                &intent(VolumeChange::Set(60), &[("Kitchen", 50)]),
                &agent(),
                at(NOON)
            ),
            Decision::Allow
        );
    }

    #[test]
    fn deltas_are_clamped_and_decreases_allowed() {
        let p = Policy::default();
        let adj = |d, cur| intent(VolumeChange::Adjust(d), &[("Kitchen", cur)]);
        assert_eq!(
            allowed(&p.evaluate(&adj(30, 40), &agent(), at(NOON))),
            Some(VolumeChange::Adjust(20))
        );
        assert_eq!(
            allowed(&p.evaluate(&adj(10, 65), &agent(), at(NOON))),
            Some(VolumeChange::Adjust(5))
        );
        assert_eq!(
            p.evaluate(&adj(-50, 40), &agent(), at(NOON)),
            Decision::Allow
        );
        assert_eq!(
            p.evaluate(&adj(-128, 3), &agent(), at(NOON)),
            Decision::Allow
        );
    }

    #[test]
    fn raising_a_room_already_over_its_cap_is_denied() {
        let p = Policy::from_toml(QUIET).unwrap();
        let d = p.evaluate(
            &intent(VolumeChange::Adjust(5), &[("Den", 40)]),
            &agent(),
            at("23:00"),
        );
        assert_eq!(
            d,
            Decision::Deny {
                reason: "Den is capped at 25 during quiet hours (22:00–07:00); it is already at 40"
                    .into()
            }
        );
        // Turning it down is fine, but not to a level still over the cap.
        let d = p.evaluate(
            &intent(VolumeChange::Set(30), &[("Den", 40)]),
            &agent(),
            at("23:00"),
        );
        assert_eq!(allowed(&d), Some(VolumeChange::Set(25)));
        assert_eq!(
            p.evaluate(
                &intent(VolumeChange::Set(20), &[("Den", 40)]),
                &agent(),
                at("23:00")
            ),
            Decision::Allow
        );
    }

    #[test]
    fn group_changes_keep_every_member_under_its_cap() {
        let p = Policy::from_toml("[rooms.Bedroom]\nmax_volume = 40\n").unwrap();
        let members = [("Kitchen", 40), ("Bedroom", 30)]; // group level 35
        // Proportional move to 50 would put Bedroom at 43; 47 keeps it at 40.
        let d = p.evaluate(&intent(VolumeChange::Set(50), &members), &agent(), at(NOON));
        assert_eq!(allowed(&d), Some(VolumeChange::Set(47)));
        assert!(
            matches!(&d, Decision::Clamp { reason, .. } if reason == "Bedroom is capped at 40")
        );
        assert_eq!(
            p.evaluate(
                &intent(VolumeChange::Adjust(10), &members),
                &agent(),
                at(NOON)
            ),
            Decision::Allow
        );
        // A silent group moves together.
        let d = p.evaluate(
            &intent(VolumeChange::Set(60), &[("Kitchen", 0), ("Bedroom", 0)]),
            &agent(),
            at(NOON),
        );
        assert_eq!(allowed(&d), Some(VolumeChange::Set(20)));
    }

    #[test]
    fn tool_allow_and_deny_lists() {
        let p = Policy::from_toml(
            r#"
[clients."Alice@Example.com"]
allow = ["list_zones", "play"]

[clients.mcp-stdio]
deny = ["group"]

[clients."tag:kiosk"]
allow = []
"#,
        )
        .unwrap();
        let call = |name, read_only| ToolCall { name, read_only };
        let alice = Client::Tailnet("alice@example.com".into());
        assert_eq!(
            p.authorize(call("list_zones", true), &alice),
            Decision::Allow
        );
        assert_eq!(p.authorize(call("play", false), &alice), Decision::Allow);
        assert_eq!(
            p.authorize(call("set_volume", false), &alice),
            Decision::Deny {
                reason: "alice@example.com may only use: list_zones, play".into()
            }
        );
        assert!(matches!(
            p.authorize(call("group", false), &Client::McpStdio),
            Decision::Deny { .. }
        ));
        assert_eq!(
            p.authorize(call("play", false), &Client::McpStdio),
            Decision::Allow
        );
        assert!(matches!(
            p.authorize(call("list_zones", true), &Client::Tailnet("tag:kiosk".into())),
            Decision::Deny { reason } if reason.ends_with("no tools")
        ));
    }

    #[test]
    fn unknown_clients_default_to_read_only() {
        let p = Policy::default();
        let read = ToolCall {
            name: "list_zones",
            read_only: true,
        };
        let write = ToolCall {
            name: "set_volume",
            read_only: false,
        };
        assert_eq!(p.authorize(read, &Client::Unknown), Decision::Allow);
        assert!(matches!(
            p.authorize(write, &Client::Unknown),
            Decision::Deny { .. }
        ));
        // Identified callers get every tool by default (still volume-capped).
        assert_eq!(p.authorize(write, &agent()), Decision::Allow);
        assert_eq!(p.authorize(write, &Client::LoopbackHttp), Decision::Allow);
        // An explicit allow list for unknown callers widens the default.
        let open = Policy::from_toml("[clients.unknown]\nallow = [\"set_volume\"]\n").unwrap();
        assert_eq!(open.authorize(write, &Client::Unknown), Decision::Allow);
    }

    #[test]
    fn parses_the_documented_example() {
        let p = Policy::from_toml(
            r#"
[defaults]
max_volume = 60
max_step = 10
fade_secs = 3

[quiet_hours]
start = "22:30"
end = "06:45"
max_volume = 20

[rooms."Bedroom"]
max_volume = 40
"#,
        )
        .unwrap();
        assert_eq!(
            p.defaults,
            Limits {
                max_volume: 60,
                max_step: 10,
                fade_secs: 3
            }
        );
        let q = p.quiet_hours.unwrap();
        assert_eq!(q.start, NaiveTime::from_hms_opt(22, 30, 0).unwrap());
        assert_eq!(q.max_volume, 20);
        assert_eq!(Policy::from_toml("").unwrap(), Policy::default());
    }

    fn err(text: &str) -> PolicyError {
        Policy::from_toml(text).unwrap_err()
    }

    #[test]
    fn errors_name_the_line() {
        let e = err("[defaults]\nmax_step = 10\nmax_volume = 150\n");
        assert_eq!((e.line, e.column), (Some(3), Some(14)));
        assert!(e.message.contains("between 0 and 100, not 150"), "{e}");
        assert_eq!(
            e.to_string(),
            format!("policy.toml line 3, column 14: {}", e.message)
        );

        let e = err("[quiet_hours]\nstart = \"25:00\"\nend = \"07:00\"\nmax_volume = 20\n");
        assert_eq!(e.line, Some(2));
        assert!(e.message.contains("\"25:00\""), "{e}");

        let e = err("[defaults]\nmax_volum = 50\n");
        assert_eq!(e.line, Some(2));
        assert!(e.message.contains("max_volum"), "{e}");

        let e = err("\n\n[quiet_hours]\nstart = \"07:00\"\nend = \"07:00\"\nmax_volume = 1\n");
        assert!(matches!(e.line, Some(3..=6)), "{e}");
        assert!(e.message.contains("same time"), "{e}");

        let e = err("[rooms.\"Den\"\nmax_volume = 3\n");
        assert_eq!(e.line, Some(1));

        let e = err("[rooms.Den]\nmax_volume = 30\n[rooms.den]\nmax_volume = 40\n");
        assert!(e.message.contains("twice"), "{e}");
    }

    #[test]
    fn load_reads_the_data_dir_and_defaults_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(Policy::load(dir.path()).unwrap(), Policy::default());

        std::fs::write(&path, "[defaults]\nmax_volume = 55\n").unwrap();
        assert_eq!(Policy::load(dir.path()).unwrap().defaults.max_volume, 55);

        std::fs::write(&path, "[defaults]\nmax_volume = 555\n").unwrap();
        let e = Policy::load(dir.path()).unwrap_err();
        assert_eq!(e.path.as_deref(), Some(path.as_path()));
        assert!(
            e.to_string()
                .starts_with(&format!("{} line 2", path.display())),
            "{e}"
        );
    }
}
