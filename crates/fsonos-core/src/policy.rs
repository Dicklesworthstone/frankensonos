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
//! HTTP API, MCP server and CLI agree. A running daemon re-reads the file
//! when it changes; an edit that no longer parses is logged and the last good
//! policy stays in force.
//!
//! ```toml
//! [defaults]
//! max_volume = 70        # per room
//! max_step = 20          # largest single increase
//! fade_secs = 0         # scheduled volume changes ramp over this long
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
//! `loopback-http`, `bridge-compat`, a tailnet principal (login,
//! `tag:<name>` or node name), or `unknown`.
//!
//! An `allow` or `deny` entry names a tool or a class of tools ([`CLASSES`]:
//! `volume`, `playback`, `grouping`, `queue`, `sound`, `house-config`,
//! `announce`, `dj`, `auth`, `undo`, and `read` for every read). A tool that
//! always carries out another's operation is denied with it: denying
//! `set_volume` denies `fade_volume` too ([`CARRIES`]). Composite calls
//! check each operation they would make: a scene applied by a client denied
//! `set_volume` leaves the volumes as they are, and a scheduled volume change
//! is refused when it fires.
//!
//! Starting a Spotify sign-in (`spotify_sign_in`) is the owner's: whoever
//! signs in chooses whose library and taste the DJ follows. Only the CLI and
//! the owner's own devices (the login of the daemon host's tailnet node,
//! [`Policy::with_owner`]) may start one, unless a client's `grant` names it
//! or its class (`auth`), or its `allow` list does ([`OWNER_ONLY`]). A
//! `grant` adds to what a client may use, where `allow` limits it to the
//! tools named:
//!
//! ```toml
//! [clients."alice@example.com"]
//! grant = ["auth"]
//! ```

use crate::rooms::normalize_room;
use chrono::{DateTime, FixedOffset, NaiveTime};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The policy file's name inside the data directory.
pub const FILE_NAME: &str = "policy.toml";

/// What tools do, by class. An `allow` or `deny` entry may name a class
/// instead of a tool, and then stands for every tool in it
/// (`deny = ["volume"]`); every read is in [`READ_CLASS`] too.
pub const CLASSES: [(&str, &[&str]); 10] = [
    ("volume", &["set_volume", "fade_volume", "mute"]),
    (
        "playback",
        &[
            "play",
            "pause",
            "resume",
            "next",
            "previous",
            "seek",
            "play_favorite",
            "play_queue",
            "set_play_mode",
            "set_sleep_timer",
        ],
    ),
    (
        "grouping",
        &["group", "ungroup", "group_all", "move_playback"],
    ),
    (
        "queue",
        &["add_to_queue", "remove_from_queue", "move_in_queue"],
    ),
    ("sound", &["set_eq"]),
    (
        "house-config",
        &[
            "save_scene",
            "delete_scene",
            "add_schedule",
            "remove_schedule",
            "pause_schedule",
            "resume_schedule",
        ],
    ),
    ("announce", &["announce"]),
    (
        "dj",
        &[
            "dj_start",
            "dj_stop",
            "dj_skip",
            "dj_steer",
            "dj_feedback",
            "dj_prefer",
            "dj_sync",
        ],
    ),
    ("auth", &["spotify_sign_in", "spotify_sign_in_complete"]),
    ("undo", &["undo_last", "undo_checked"]),
];

/// The class every read is in.
pub const READ_CLASS: &str = "read";

/// Tools that always carry out another tool's operation, which an entry
/// denying that tool (or its class) denies too: a fade is a run of volume
/// sets, and a fade-out is a fade and then a pause.
pub const CARRIES: [(&str, &str); 5] = [
    ("fade_volume", "set_volume"),
    ("fade_and_pause", "pause"),
    ("fade_and_pause", "fade_volume"),
    ("fade_and_pause", "set_volume"),
    ("spotify_sign_in_complete", "spotify_sign_in"),
];

/// Tools that finish what another starts: an `allow` or `grant` entry naming
/// the starter names its finish too, so a client that may start a Spotify
/// sign-in may also finish it. (A `deny` covers it through [`CARRIES`].)
const FINISHES: [(&str, &str); 1] = [("spotify_sign_in_complete", "spotify_sign_in")];

/// Tools only the owner may use (the CLI, or the daemon host's own tailnet
/// login, [`Policy::with_owner`]), unless a client's `grant` (or its `allow`
/// list) names them or their class: starting a Spotify sign-in
/// chooses whose library and taste the DJ follows. Finishing one
/// (`spotify_sign_in_complete`, the browser's way back) is not among them:
/// it needs the waiting sign-in's state, which only its starter saw.
pub const OWNER_ONLY: [&str; 1] = ["spotify_sign_in"];

/// The class `tool` is in ([`CLASSES`]), if any.
#[must_use]
pub fn class_of(tool: &str) -> Option<&'static str> {
    CLASSES
        .iter()
        .find(|(_, tools)| tools.contains(&tool))
        .map(|(class, _)| *class)
}

/// Whether the `allow` or `deny` entry `entry` names `tool`: the tool
/// itself, its class, or the class of reads for a read.
fn names(entry: &str, tool: ToolCall<'_>) -> bool {
    entry == tool.name
        || class_of(tool.name) == Some(entry)
        || (tool.read_only && entry == READ_CLASS)
        || FINISHES
            .iter()
            .any(|(finish, start)| *finish == tool.name && *start == entry)
}

/// What the deny entry `entry` denies of `tool`, for the reason given: the
/// tool itself, its class (or a read), or an operation it carries out.
fn denied_by(entry: &str, tool: ToolCall<'_>) -> Option<String> {
    if entry == tool.name {
        return Some(entry.to_owned());
    }
    if class_of(tool.name) == Some(entry) {
        return Some(format!("{entry} ({} is a {entry} tool)", tool.name));
    }
    if tool.read_only && entry == READ_CLASS {
        return Some(format!("{entry} ({} is a read)", tool.name));
    }
    CARRIES
        .iter()
        .filter(|(composite, _)| *composite == tool.name)
        .find(|(_, op)| *op == entry || class_of(op) == Some(entry))
        .map(|(_, op)| format!("{entry} ({} carries out {op})", tool.name))
}

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
    /// A local agent on the legacy bridge's API (`fsonos serve
    /// --bridge-compat`), held to the house policy like any agent.
    BridgeCompat,
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
            Self::BridgeCompat => "bridge-compat",
            Self::Unknown => "unknown",
        }
    }
}

/// The keys no caller may go by: the daemon's own clients ([`Client::key`])
/// and the system actors that act under a key of their own (the DJ; also
/// schedules, sleep timers, the self-test and setup). A tailnet name that
/// took one would get that client's rules, a schedule it made would run as
/// that client, and its rows in the action log would read as that client's.
pub const RESERVED_KEYS: [&str; 10] = [
    "cli",
    "mcp-stdio",
    "loopback-http",
    "bridge-compat",
    "unknown",
    "dj",
    "schedule",
    "sleep",
    "selftest",
    "setup",
];

/// Whether `name` is one of [`RESERVED_KEYS`], ignoring case and the
/// whitespace around it.
#[must_use]
pub fn reserved_key(name: &str) -> bool {
    RESERVED_KEYS
        .iter()
        .any(|key| key.eq_ignore_ascii_case(name.trim()))
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RoomLimits {
    /// The room as the file names it.
    name: String,
    max_volume: Option<u8>,
    max_step: Option<u8>,
}

/// Which MCP tools a client is offered in `tools/list` ([`Policy::mcp_profile`]):
/// the curated essentials, or every tool. Policy still governs tool *use*
/// independently; this only shapes what an agent sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpProfile {
    /// The dozen-odd tools most agent tasks need.
    Essentials,
    /// Every tool.
    Full,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ClientRules {
    allow: Option<Vec<String>>,
    deny: Vec<String>,
    /// Tools (or classes) this client may use besides: the [`OWNER_ONLY`]
    /// ones need it, and with an `allow` list it adds to what that names.
    grant: Vec<String>,
    capped: Option<bool>,
    /// It may change or delete scenes and schedules others saved
    /// ([`Policy::may_change`]).
    manage_others: bool,
    /// Which MCP tools it is offered ([`Policy::mcp_profile`]).
    mcp_profile: Option<McpProfile>,
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
    /// The login of the daemon host's tailnet node: the owner's own devices
    /// reach the daemon as it ([`Self::with_owner`]). Never from the file.
    owner: Option<String>,
}

/// The effective policy, for showing: see [`Policy::view`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyView {
    pub defaults: Limits,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHoursView>,
    /// Rooms with limits of their own, by name.
    pub rooms: Vec<RoomView>,
    /// Clients with rules of their own, by key (lowercase).
    pub clients: Vec<ClientView>,
}

/// Quiet hours in local wall-clock time, `HH:MM`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHoursView {
    pub start: String,
    pub end: String,
    pub max_volume: u8,
}

/// A room's own limits; an absent one is the default's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomView {
    pub room: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_volume: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_step: Option<u8>,
}

/// A client's own rules: `allow` (only these tools) when given, `deny`
/// always, `grant` (tools it may use too: the [`OWNER_ONLY`] ones need it,
/// and it adds to an `allow` list), whether volume caps apply when set, and
/// whether it may change scenes and schedules others saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientView {
    pub client: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capped: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manage_others: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_profile: Option<McpProfile>,
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
            let limits = RoomLimits {
                name: name.clone(),
                ..room.into()
            };
            if rooms.insert(normalize_room(&name), limits).is_some() {
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
            owner: None,
        })
    }

    /// This policy with `login` as the owner's: the login of the daemon
    /// host's tailnet node, so the owner's own devices (the Apple and web
    /// remotes through Tailscale) may use the [`OWNER_ONLY`] tools as the
    /// CLI does. `None` (off the tailnet, a tagged node) leaves the CLI alone.
    #[must_use]
    pub fn with_owner(mut self, login: Option<String>) -> Self {
        self.owner = login.filter(|l| !l.trim().is_empty());
        self
    }

    /// The owner's tailnet login, when known ([`Self::with_owner`]).
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Whether `client` is the owner: the CLI, or the owner's login.
    fn is_owner(&self, client: &Client) -> bool {
        match client {
            Client::Cli => true,
            Client::Tailnet(login) => self
                .owner
                .as_deref()
                .is_some_and(|owner| owner.eq_ignore_ascii_case(login)),
            _ => false,
        }
    }

    /// The policy as people read it (`fsonos policy show`, `GET /policy`):
    /// rooms by name and clients by key, each sorted.
    #[must_use]
    pub fn view(&self) -> PolicyView {
        let mut rooms: Vec<RoomView> = self
            .rooms
            .values()
            .map(|r| RoomView {
                room: r.name.clone(),
                max_volume: r.max_volume,
                max_step: r.max_step,
            })
            .collect();
        rooms.sort_by_key(|r| normalize_room(&r.room));
        let mut clients: Vec<ClientView> = self
            .clients
            .iter()
            .map(|(key, c)| ClientView {
                client: key.clone(),
                allow: c.allow.clone(),
                deny: c.deny.clone(),
                grant: c.grant.clone(),
                capped: c.capped,
                manage_others: c.manage_others,
                mcp_profile: c.mcp_profile,
            })
            .collect();
        clients.sort_by(|a, b| a.client.cmp(&b.client));
        PolicyView {
            defaults: self.defaults,
            quiet_hours: self.quiet_hours.map(|q| QuietHoursView {
                start: q.start.format("%H:%M").to_string(),
                end: q.end.format("%H:%M").to_string(),
                max_volume: q.max_volume,
            }),
            rooms,
            clients,
        }
    }

    fn client_rules(&self, client: &Client) -> Option<&ClientRules> {
        self.clients.get(&client.key().to_lowercase())
    }

    /// Which MCP tools `client` is offered in `tools/list`: its configured
    /// [`McpProfile`], or `None` (the server's default). It shapes only what
    /// the agent sees; [`Self::authorize`] still governs every tool's use.
    #[must_use]
    pub fn mcp_profile(&self, client: &Client) -> Option<McpProfile> {
        self.client_rules(client).and_then(|r| r.mcp_profile)
    }

    /// May `client` change or delete a scene or schedule that `saver` (a
    /// [`Client::key`]) saved: save over it, delete it, pause, resume or
    /// remove it? Its saver may, and so may the owner (the CLI or the
    /// owner's tailnet login) and a client whose rules set
    /// `manage_others = true`. One saved before savers were kept (`None`)
    /// is the owner's. [`Self::authorize`] decides first whether `client`
    /// may use the tool at all.
    #[must_use]
    pub fn may_change(&self, client: &Client, saver: Option<&str>) -> Decision {
        if saver.is_some_and(|saver| saver.eq_ignore_ascii_case(client.key()))
            || self.is_owner(client)
            || self.client_rules(client).is_some_and(|r| r.manage_others)
        {
            return Decision::Allow;
        }
        let whose = saver.map_or_else(
            || "the owner's (saved before fsonos kept who saved what)".to_owned(),
            |saver| format!("{saver}'s"),
        );
        Decision::Deny {
            reason: format!(
                "it is {whose}; only its saver or the owner may change it, unless {FILE_NAME} \
                 sets manage_others = true under [clients.\"{}\"]",
                client.key()
            ),
        }
    }

    /// Denials another tool gets around: a client denied a tool, but not
    /// the rest of its class, can do the same thing through those. One line
    /// each, naming the class that would cover them.
    #[must_use]
    pub fn open_doors(&self) -> Vec<String> {
        let mut keys: Vec<&String> = self.clients.keys().collect();
        keys.sort();
        let mut out = Vec::new();
        for key in keys {
            let deny = &self.clients[key].deny;
            for entry in deny {
                let Some(class) = class_of(entry) else {
                    continue;
                };
                if deny.iter().any(|e| e == class) {
                    continue;
                }
                let carried = |tool: &str| CARRIES.contains(&(tool, entry.as_str()));
                let open: Vec<&str> = CLASSES
                    .iter()
                    .filter(|(c, _)| *c == class)
                    .flat_map(|(_, tools)| tools.iter().copied())
                    .filter(|&t| t != entry.as_str() && !deny.iter().any(|e| e == t) && !carried(t))
                    .collect();
                if !open.is_empty() {
                    out.push(format!(
                        "{key}: {entry} is denied, but these {class} tools are not: {}; \
                         deny \"{class}\" to cover them all",
                        open.join(", ")
                    ));
                }
            }
        }
        out
    }

    /// Do volume caps apply to `client`? Everyone but the CLI by default.
    #[must_use]
    pub fn is_capped(&self, client: &Client) -> bool {
        self.client_rules(client)
            .and_then(|r| r.capped)
            .unwrap_or(*client != Client::Cli)
    }

    /// The highest level `client` may set `room` to at `now`: its room cap
    /// (or the quiet-hours cap when lower), or 100 when `client` is uncapped.
    /// For levels that are set outright, such as an announcement's; there is
    /// no step limit here.
    #[must_use]
    pub fn max_volume(&self, room: &str, client: &Client, now: DateTime<FixedOffset>) -> u8 {
        self.cap_for(room, client, now).map_or(100, |(cap, _)| cap)
    }

    /// The level `client` is held to in `room` at `now` (its room cap, or the
    /// quiet-hours cap when lower), and why; `None` when `client` is uncapped.
    #[must_use]
    pub fn cap_for(
        &self,
        room: &str,
        client: &Client,
        now: DateTime<FixedOffset>,
    ) -> Option<(u8, String)> {
        self.is_capped(client).then(|| self.cap(room, now))
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
            if let Some(what) = rules.deny.iter().find_map(|entry| denied_by(entry, tool)) {
                return Decision::Deny {
                    reason: format!("policy denies {what} to {client}"),
                };
            }
            if let Some(allow) = &rules.allow {
                let named: Vec<&str> = allow
                    .iter()
                    .chain(&rules.grant)
                    .map(String::as_str)
                    .collect();
                return if named.iter().any(|entry| names(entry, tool)) {
                    Decision::Allow
                } else {
                    Decision::Deny {
                        reason: format!(
                            "{client} may only use: {}",
                            if named.is_empty() {
                                "no tools".to_owned()
                            } else {
                                named.join(", ")
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
        if !tool.read_only
            && !self.is_owner(client)
            && OWNER_ONLY.contains(&tool.name)
            && !self
                .client_rules(client)
                .is_some_and(|rules| rules.grant.iter().any(|entry| names(entry, tool)))
        {
            return Decision::Deny {
                reason: format!(
                    "{} is the owner's: only the CLI or the owner's own tailnet login may use \
                     it, unless {FILE_NAME} grants it to {client} (grant = [\"auth\"] under \
                     [clients.\"{}\"])",
                    tool.name,
                    client.key()
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
        let steps: Vec<u8> = rooms.iter().map(|r| self.step(&r.room)).collect();
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
        // A group increase is proportional: a louder member can rise by
        // more than the group's mean. Bound the actual increase in every
        // room, including overrides on quieter or silent members.
        let over_step = |to: u8| {
            members_at(&current, level, to)
                .zip(current.iter().zip(&steps))
                .position(|(member, (before, step))| member.saturating_sub(*before) > *step)
        };
        let step_reason = |room: usize| {
            format!(
                "{} volume rises at most {} at a time (max_step)",
                rooms[room].room, steps[room]
            )
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
        match (level + 1..=target)
            .rev()
            .find(|&l| over_cap(l).is_none() && over_step(l).is_none())
        {
            Some(to) if to == target => Decision::Allow,
            Some(to) => {
                let reason = over_cap(to + 1).map_or_else(
                    || step_reason(over_step(to + 1).unwrap_or(0)),
                    |room| caps[room].1.clone(),
                );
                clamp(to, reason)
            }
            None => {
                let reason = over_cap(level + 1).map_or_else(
                    || step_reason(over_step(level + 1).unwrap_or(0)),
                    |room| format!("{}; it is already at {}", caps[room].1, rooms[room].volume),
                );
                Decision::Deny { reason }
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
            name: String::new(),
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
    #[serde(default)]
    grant: Vec<String>,
    capped: Option<bool>,
    #[serde(default)]
    manage_others: bool,
    mcp_profile: Option<McpProfile>,
}

impl From<RawClient> for ClientRules {
    fn from(raw: RawClient) -> Self {
        Self {
            allow: raw.allow,
            deny: raw.deny,
            grant: raw.grant,
            capped: raw.capped,
            manage_others: raw.manage_others,
            mcp_profile: raw.mcp_profile,
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
mod view_tests {
    use super::*;

    #[test]
    fn the_view_shows_every_rule_sorted() {
        let policy = Policy::from_toml(
            r#"
[defaults]
max_volume = 60
max_step = 10

[quiet_hours]
start = "22:00"
end = "07:30"
max_volume = 25

[rooms."Living Room"]
max_step = 5

[rooms."Bedroom"]
max_volume = 40

[clients."tag:agent"]
deny = ["party"]
capped = true

[clients."Alice@example.com"]
allow = ["list_zones", "play"]
"#,
        )
        .unwrap();
        let view = policy.view();
        assert_eq!(
            view.defaults,
            Limits {
                max_volume: 60,
                max_step: 10,
                fade_secs: 0
            }
        );
        assert_eq!(
            view.quiet_hours,
            Some(QuietHoursView {
                start: "22:00".into(),
                end: "07:30".into(),
                max_volume: 25
            })
        );
        assert_eq!(
            view.rooms,
            [
                RoomView {
                    room: "Bedroom".into(),
                    max_volume: Some(40),
                    max_step: None
                },
                RoomView {
                    room: "Living Room".into(),
                    max_volume: None,
                    max_step: Some(5)
                },
            ]
        );
        let clients: Vec<&str> = view.clients.iter().map(|c| c.client.as_str()).collect();
        assert_eq!(clients, ["alice@example.com", "tag:agent"]);
        assert_eq!(
            view.clients[0].allow.as_deref(),
            Some(&["list_zones".to_string(), "play".to_string()][..])
        );
        assert_eq!(
            (view.clients[1].deny.len(), view.clients[1].capped),
            (1, Some(true))
        );

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(
            json["rooms"][0],
            serde_json::json!({"room": "Bedroom", "max_volume": 40})
        );
        assert!(Policy::default().view().quiet_hours.is_none());
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
    fn max_volume_is_the_room_or_quiet_hours_cap_for_capped_clients() {
        let p = Policy::from_toml(&format!("[rooms.Bedroom]\nmax_volume = 40\n{QUIET}")).unwrap();
        assert_eq!(p.max_volume("Bedroom", &agent(), at(NOON)), 40);
        assert_eq!(p.max_volume("bedroom", &agent(), at(NOON)), 40);
        assert_eq!(p.max_volume("Kitchen", &agent(), at(NOON)), 70);
        assert_eq!(p.max_volume("Kitchen", &agent(), at("23:30")), 25);
        assert_eq!(p.max_volume("Bedroom", &agent(), at("06:59")), 25);
        assert_eq!(p.max_volume("Kitchen", &Client::Cli, at("23:30")), 100);
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
    fn group_increases_respect_each_members_step_after_proportional_scaling() {
        let policy = Policy::default();
        let members = [("Kitchen", 20), ("Bedroom", 40)];
        // The group starts at 30. A mean increase of 20 would raise Bedroom
        // by 27 to 67, even though both rooms would remain below their caps.
        // A mean of 45 instead leaves the rooms at 30 and 60: +10 and +20.
        for (requested, expected) in [
            (VolumeChange::Set(50), VolumeChange::Set(45)),
            (VolumeChange::Adjust(20), VolumeChange::Adjust(15)),
        ] {
            let decision = policy.evaluate(&intent(requested, &members), &agent(), at(NOON));
            assert_eq!(allowed(&decision), Some(expected));
            assert!(matches!(decision, Decision::Clamp { reason, .. }
                    if reason == "Bedroom volume rises at most 20 at a time (max_step)"));
            assert_eq!(
                policy.evaluate(&intent(expected, &members), &agent(), at(NOON)),
                Decision::Allow
            );
        }
    }

    #[test]
    fn a_quieter_rooms_step_limits_its_own_rise_instead_of_the_group_mean() {
        let policy = Policy::from_toml("[rooms.Kitchen]\nmax_step = 2\n").unwrap();
        let members = [("Kitchen", 10), ("Bedroom", 30)];
        // At a group mean of 24 the rooms become 12 and 36. Kitchen's limit
        // is satisfied even though the mean rises by more than two.
        let decision =
            policy.evaluate(&intent(VolumeChange::Set(25), &members), &agent(), at(NOON));
        assert_eq!(allowed(&decision), Some(VolumeChange::Set(24)));
        assert!(matches!(decision, Decision::Clamp { reason, .. } if reason.contains("Kitchen")));
        assert_eq!(
            policy.evaluate(
                &intent(VolumeChange::Adjust(4), &members),
                &agent(),
                at(NOON)
            ),
            Decision::Allow
        );
    }

    #[test]
    fn zero_step_rooms_can_stay_silent_while_other_group_members_rise() {
        let policy = Policy::from_toml("[rooms.Kitchen]\nmax_step = 0\n").unwrap();
        // Proportional scaling leaves a silent member silent, so its zero
        // allowance must not prevent a permitted increase in another room.
        assert_eq!(
            policy.evaluate(
                &intent(VolumeChange::Set(30), &[("Kitchen", 0), ("Bedroom", 40)]),
                &agent(),
                at(NOON)
            ),
            Decision::Allow
        );
        // An entirely silent group moves equally: Kitchen would rise too.
        let decision = policy.evaluate(
            &intent(VolumeChange::Adjust(1), &[("Kitchen", 0), ("Bedroom", 0)]),
            &agent(),
            at(NOON),
        );
        assert!(matches!(decision, Decision::Deny { reason }
            if reason == "Kitchen volume rises at most 0 at a time (max_step)"));
    }

    #[test]
    fn member_steps_do_not_restrict_decreases_or_uncapped_cli_changes() {
        let policy = Policy::from_toml("[defaults]\nmax_step = 0\n").unwrap();
        let members = [("Kitchen", 20), ("Bedroom", 80)];
        assert_eq!(
            policy.evaluate(
                &intent(VolumeChange::Adjust(-10), &members),
                &agent(),
                at(NOON)
            ),
            Decision::Allow
        );
        assert_eq!(
            policy.evaluate(
                &intent(VolumeChange::Set(100), &members),
                &Client::Cli,
                at(NOON)
            ),
            Decision::Allow
        );
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

    /// A scene or schedule is its saver's to change (p7vq.4): the owner's
    /// too, and a client's with manage_others; one with no saver is the
    /// owner's.
    #[test]
    fn what_a_client_saved_is_its_own_to_change() {
        let p = Policy::from_toml("[clients.\"tag:Keeper\"]\nmanage_others = true\n")
            .unwrap()
            .with_owner(Some("owner@example.com".into()));
        let tailnet = |login: &str| Client::Tailnet(login.into());
        let allowed =
            |client: &Client, saver: Option<&str>| p.may_change(client, saver) == Decision::Allow;
        assert!(allowed(&tailnet("tag:agent"), Some("tag:agent")));
        assert!(allowed(
            &tailnet("Alice@Example.com"),
            Some("alice@example.com")
        ));
        assert!(allowed(&Client::Cli, Some("tag:agent")));
        assert!(allowed(&tailnet("owner@example.com"), Some("tag:agent")));
        assert!(allowed(&tailnet("tag:keeper"), Some("cli")));
        assert!(allowed(&Client::Cli, None));
        for (client, saver) in [
            (tailnet("tag:other"), Some("tag:agent")),
            (Client::McpStdio, Some("cli")),
            (Client::LoopbackHttp, Some("mcp-stdio")),
            (tailnet("tag:agent"), None),
        ] {
            let Decision::Deny { reason } = p.may_change(&client, saver) else {
                panic!("{client} may change {saver:?}'s");
            };
            assert!(
                reason.contains(saver.unwrap_or("the owner's"))
                    && reason.contains(&format!("[clients.\"{}\"]", client.key())),
                "{reason}"
            );
        }
        assert!(p.view().clients[0].manage_others);
    }

    /// A grant adds to an allow list too: starting and finishing a sign-in,
    /// and nothing else the list leaves out.
    #[test]
    fn a_grant_adds_to_an_allow_list() {
        let panel = Policy::from_toml(
            "[clients.\"tag:panel\"]\nallow = [\"read\", \"playback\"]\ngrant = [\"auth\"]\n",
        )
        .unwrap();
        let call = |name, read_only| ToolCall { name, read_only };
        let tag_panel = Client::Tailnet("tag:panel".into());
        for tool in ["spotify_sign_in", "spotify_sign_in_complete", "pause"] {
            assert_eq!(
                panel.authorize(call(tool, false), &tag_panel),
                Decision::Allow,
                "{tool}"
            );
        }
        // The refusal names what the grant adds, too.
        assert!(matches!(
            panel.authorize(call("set_volume", false), &tag_panel),
            Decision::Deny { reason } if reason.ends_with("may only use: read, playback, auth")
        ));
    }

    #[test]
    fn starting_a_sign_in_is_the_owners_unless_granted() {
        let p = Policy::from_toml(
            r#"
[clients."alice@example.com"]
grant = ["auth"]

[clients."tag:kiosk"]
allow = ["read", "spotify_sign_in"]

[clients.mcp-stdio]
grant = ["spotify_sign_in"]
deny = ["auth"]
"#,
        )
        .unwrap();
        let call = |name, read_only| ToolCall { name, read_only };
        let start = call("spotify_sign_in", false);
        let status = call("spotify_sign_in", true);
        let finish = call("spotify_sign_in_complete", false);
        // The CLI may; another caller is told the key that would let it.
        assert_eq!(p.authorize(start, &Client::Cli), Decision::Allow);
        // So may the owner's own login (the daemon host's tailnet node), in
        // any case, and no other login.
        let owned = Policy::default().with_owner(Some("owner@example.com".into()));
        assert_eq!(owned.owner(), Some("owner@example.com"));
        assert_eq!(
            owned.authorize(start, &Client::Tailnet("Owner@Example.com".into())),
            Decision::Allow
        );
        assert!(matches!(
            owned.authorize(start, &Client::Tailnet("bob@example.com".into())),
            Decision::Deny { .. }
        ));
        assert!(matches!(
            owned.authorize(start, &Client::LoopbackHttp),
            Decision::Deny { .. }
        ));
        assert_eq!(Policy::default().with_owner(Some(" ".into())).owner(), None);
        assert_eq!(
            Policy::default().authorize(start, &Client::Cli),
            Decision::Allow
        );
        for other in [
            Client::Tailnet("bob@example.com".into()),
            Client::LoopbackHttp,
        ] {
            assert!(
                matches!(p.authorize(start, &other), Decision::Deny { reason }
                    if reason.contains(&format!("grant = [\"auth\"] under [clients.\"{}\"]", other.key()))),
                "{other}"
            );
            // Its status stays readable.
            assert_eq!(p.authorize(status, &other), Decision::Allow, "{other}");
        }
        // An unidentified caller is told what would identify it; a grant
        // could not help it.
        assert!(matches!(
            p.authorize(start, &Client::Unknown),
            Decision::Deny { reason } if reason.starts_with("unidentified callers")
        ));
        assert_eq!(p.authorize(status, &Client::Unknown), Decision::Allow);
        // Finishing a waiting sign-in is not the owner's alone: the browser
        // brings the redirect back as whoever it is.
        assert_eq!(p.authorize(finish, &Client::LoopbackHttp), Decision::Allow);
        // A grant adds to what a client may use; an allow list naming the
        // tool allows it too, and still limits the client to what it names.
        let alice = Client::Tailnet("alice@example.com".into());
        assert_eq!(p.authorize(start, &alice), Decision::Allow);
        assert_eq!(p.authorize(call("play", false), &alice), Decision::Allow);
        let kiosk = Client::Tailnet("tag:kiosk".into());
        assert_eq!(p.authorize(start, &kiosk), Decision::Allow);
        // Allowed to start one by name, it may finish it too.
        assert_eq!(p.authorize(finish, &kiosk), Decision::Allow);
        assert!(matches!(
            p.authorize(call("play", false), &kiosk),
            Decision::Deny { .. }
        ));
        // A denial beats a grant, and denying the start denies finishing.
        assert!(matches!(
            p.authorize(start, &Client::McpStdio),
            Decision::Deny { reason } if reason.contains("policy denies auth")
        ));
        let by_name = Policy::from_toml("[clients.agent]\ndeny = [\"spotify_sign_in\"]\n").unwrap();
        let agent = Client::Tailnet("agent".into());
        assert!(matches!(
            by_name.authorize(finish, &agent),
            Decision::Deny { .. }
        ));
        assert!(
            by_name.open_doors().is_empty(),
            "{:?}",
            by_name.open_doors()
        );
        assert_eq!(
            p.view()
                .clients
                .iter()
                .find(|c| c.client == "alice@example.com")
                .map(|c| c.grant.clone()),
            Some(vec!["auth".to_owned()])
        );
    }

    /// `fade_and_pause` lowers the volume step by step, then pauses: a deny
    /// of either, or of their classes, covers it (p7vq.14).
    #[test]
    fn a_fade_out_is_held_to_pause_and_volume_denies() {
        let p = Policy::from_toml(
            r#"
[clients.nopause]
deny = ["pause"]

[clients.noplayback]
deny = ["playback"]

[clients.novolume]
deny = ["set_volume"]

[clients.quiet]
deny = ["volume"]

[clients.free]
deny = ["seek"]
"#,
        )
        .unwrap();
        let fade = || ToolCall {
            name: "fade_and_pause",
            read_only: false,
        };
        for key in ["nopause", "noplayback", "novolume", "quiet"] {
            assert!(
                matches!(
                    p.authorize(fade(), &Client::Tailnet(key.into())),
                    Decision::Deny { .. }
                ),
                "{key}"
            );
        }
        assert_eq!(
            p.authorize(fade(), &Client::Tailnet("free".into())),
            Decision::Allow
        );
    }

    #[test]
    fn classes_and_carried_operations_are_denied_and_allowed_by_name() {
        let p = Policy::from_toml(
            r#"
[clients.agent]
deny = ["set_volume"]

[clients.quiet]
deny = ["volume"]

[clients.reader]
allow = ["read", "playback"]
"#,
        )
        .unwrap();
        let call = |name, read_only| ToolCall { name, read_only };
        let who = |key: &str| Client::Tailnet(key.into());
        // A tool-name deny: the tool, and a tool that carries it out.
        let agent = who("agent");
        assert_eq!(
            p.authorize(call("set_volume", false), &agent),
            Decision::Deny {
                reason: "policy denies set_volume to agent".into()
            }
        );
        assert_eq!(
            p.authorize(call("fade_volume", false), &agent),
            Decision::Deny {
                reason: "policy denies set_volume (fade_volume carries out set_volume) to agent"
                    .into()
            }
        );
        // ...but not the rest of its class: that is what the class is for.
        assert_eq!(p.authorize(call("mute", false), &agent), Decision::Allow);
        assert_eq!(p.authorize(call("play", false), &agent), Decision::Allow);
        // A class deny covers every tool in it.
        let quiet = who("quiet");
        for tool in ["set_volume", "fade_volume", "mute"] {
            assert!(
                matches!(p.authorize(call(tool, false), &quiet), Decision::Deny { reason }
                    if reason == format!("policy denies volume ({tool} is a volume tool) to quiet")),
                "{tool}"
            );
        }
        assert_eq!(p.authorize(call("pause", false), &quiet), Decision::Allow);
        // An allow list naming classes: every read, and playback.
        let reader = who("reader");
        assert_eq!(
            p.authorize(call("list_zones", true), &reader),
            Decision::Allow
        );
        assert_eq!(p.authorize(call("pause", false), &reader), Decision::Allow);
        assert!(matches!(
            p.authorize(call("set_volume", false), &reader),
            Decision::Deny { .. }
        ));
        assert_eq!(class_of("fade_volume"), Some("volume"));
        assert_eq!(class_of("list_zones"), None);
    }

    #[test]
    fn a_denial_another_tool_of_its_class_gets_around_is_named() {
        let p = Policy::from_toml(
            r#"
[clients.agent]
deny = ["set_volume", "group"]

[clients.kid]
deny = ["set_volume", "volume"]
"#,
        )
        .unwrap();
        assert_eq!(
            p.open_doors(),
            [
                "agent: set_volume is denied, but these volume tools are not: mute; deny \
                 \"volume\" to cover them all",
                "agent: group is denied, but these grouping tools are not: ungroup, group_all, \
                 move_playback; deny \"grouping\" to cover them all",
            ]
        );
        assert!(Policy::default().open_doors().is_empty());
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

    #[test]
    fn the_daemons_own_clients_and_system_actors_have_reserved_keys() {
        for client in [
            Client::Cli,
            Client::McpStdio,
            Client::LoopbackHttp,
            Client::BridgeCompat,
            Client::Unknown,
        ] {
            assert!(reserved_key(client.key()), "{client}");
        }
        for name in ["dj", "DJ", " dj ", "schedule", "sleep", "selftest", "setup"] {
            assert!(reserved_key(name), "{name:?}");
        }
        for name in ["dj-agent", "ada@example.com", "tag:agent", ""] {
            assert!(!reserved_key(name), "{name:?}");
        }
    }

    #[test]
    fn a_clients_mcp_profile_is_parsed_and_read_back() {
        let p = Policy::from_toml(
            "[clients.\"agent@example.com\"]\nmcp_profile = \"essentials\"\n\n             [clients.\"tag:full\"]\nmcp_profile = \"full\"\n",
        )
        .unwrap();
        assert_eq!(
            p.mcp_profile(&Client::Tailnet("agent@example.com".into())),
            Some(McpProfile::Essentials)
        );
        assert_eq!(
            p.mcp_profile(&Client::Tailnet("tag:full".into())),
            Some(McpProfile::Full)
        );
        // A client with no profile, and an unknown client, default to None.
        let bare = Policy::from_toml("[clients.\"x\"]\ndeny = [\"play\"]\n").unwrap();
        assert_eq!(bare.mcp_profile(&Client::Tailnet("x".into())), None);
        assert_eq!(bare.mcp_profile(&Client::McpStdio), None);
        assert!(
            p.view()
                .clients
                .iter()
                .any(|c| c.mcp_profile == Some(McpProfile::Essentials))
        );
    }

    #[test]
    fn an_unknown_mcp_profile_value_is_rejected() {
        let err = Policy::from_toml("[clients.\"x\"]\nmcp_profile = \"minimal\"\n").unwrap_err();
        assert!(
            err.message.contains("mcp_profile") || err.message.contains("minimal"),
            "{err:?}"
        );
    }
}
