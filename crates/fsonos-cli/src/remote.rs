//! Through the daemon: when `fsonos serve` runs, a command asks its HTTP API,
//! which holds warm, event-driven state, instead of surveying the LAN.
//!
//! `fsonos serve` writes `<data-dir>/daemon.json`, readable only by its user:
//! the loopback address of its HTTP API and the token that makes the CLI's
//! requests the CLI's under the house policy
//! (`fsonos_api::identity::CLI_TOKEN`). A command finds the daemon through
//! that file (or `FSONOS_HTTP_ADDR` when set) and checks it with
//! `GET /health` within [`PROBE`]. With no file, or no answer, it runs
//! directly, so the CLI never waits on a daemon that is not there.
//! `--direct` never asks it.
//!
//! Through the daemon a command sends the same request body the HTTP API
//! documents and prints the same DTO the direct path prints, so `--json`
//! output is the same either way. Commands with no route here (discover,
//! doctor, the DJ, scenes, ...) run directly.

use fsonos_api::{
    ActionDto, ApiError, ErrorCode, Failure, FavoriteDto, GroupRequest, MoveRequest, MuteRequest,
    OutcomeDto, PartyRequest, PlayFavoriteRequest, PlayRequest, RoomDto, UndoDto, ZoneDto,
    ZoneRequest, ZoneStateDto,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use crate::config::GlobalArgs;
use crate::{Command, Switch, direct, emit};

/// The daemon's address file in the data directory.
pub const DAEMON_FILE: &str = "daemon.json";

/// How long the `GET /health` probe may take before the command runs
/// directly.
pub const PROBE: Duration = Duration::from_millis(150);

/// How long a command through the daemon may take (an announcement waits
/// for its clip to play).
const CALL: Duration = Duration::from_secs(120);

/// What `fsonos serve` leaves in the data directory for the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonFile {
    /// The loopback HTTP API.
    pub http: SocketAddr,
    /// The CLI's token (see the module docs).
    pub cli_token: String,
    pub pid: u32,
}

/// Write `daemon.json` (owner-only), replacing any earlier one whole.
pub fn write_daemon_file(dir: &Path, file: &DaemonFile) -> io::Result<()> {
    let path = dir.join(DAEMON_FILE);
    let partial = dir.join(format!("{DAEMON_FILE}.partial"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&partial)?;
    out.write_all(
        serde_json::to_string(file)
            .map_err(io::Error::other)?
            .as_bytes(),
    )?;
    out.sync_all()?;
    std::fs::rename(&partial, &path)
}

/// Remove `daemon.json` if it is the one `pid` wrote.
pub fn remove_daemon_file(dir: &Path, pid: u32) {
    if read_daemon_file(dir).is_some_and(|f| f.pid == pid) {
        let _ = std::fs::remove_file(dir.join(DAEMON_FILE));
    }
}

fn read_daemon_file(dir: &Path) -> Option<DaemonFile> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(DAEMON_FILE)).ok()?).ok()
}

/// A running daemon that answered the probe.
#[derive(Debug, Clone)]
pub struct Daemon {
    addr: SocketAddr,
    token: Option<String>,
}

impl Daemon {
    /// The daemon to send commands to: `None` to run directly (`--direct`,
    /// no daemon known, or none answering), or, when the caller `require`s
    /// one, why there is none.
    pub fn find(global: &GlobalArgs, require: bool) -> Result<Option<Self>, Failure> {
        if global.direct {
            return Ok(None);
        }
        let file = global.data_dir().as_deref().and_then(read_daemon_file);
        // `FSONOS_HTTP_ADDR` is also serve's bind address: a wildcard or port 0
        // there says where to listen, not where to connect.
        let explicit = std::env::var("FSONOS_HTTP_ADDR")
            .ok()
            .and_then(|a| a.trim().parse::<SocketAddr>().ok())
            .filter(|a| a.port() != 0 && !a.ip().is_unspecified());
        let Some(addr) = explicit.or(file.as_ref().map(|f| f.http)) else {
            return refuse(
                require,
                "no daemon is known: `fsonos serve` writes daemon.json when it starts",
            );
        };
        let token = file.filter(|f| f.http == addr).map(|f| f.cli_token);
        if token.is_none() && !require {
            // Without the token its answers would be a local process's, not
            // the CLI's: run directly so the policy reads the same.
            return Ok(None);
        }
        let daemon = Self { addr, token };
        match daemon.healthy() {
            Ok(()) => Ok(Some(daemon)),
            Err(why) => refuse(require, &format!("no daemon answered at {addr}: {why}")),
        }
    }

    fn healthy(&self) -> Result<(), String> {
        let (status, body) = self
            .exchange("GET", "/health", None, PROBE)
            .map_err(|e| e.to_string())?;
        let ok = status == 200
            && serde_json::from_str::<serde_json::Value>(&body).is_ok_and(|v| v["status"] == "ok");
        if ok {
            Ok(())
        } else {
            Err(format!("GET /health answered {status}"))
        }
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.call("GET", path, None)
    }

    fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, Failure> {
        let body = serde_json::to_string(body)
            .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))?;
        self.call("POST", path, Some(&body))
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<T, Failure> {
        let (status, text) = self.exchange(method, path, body, CALL).map_err(|e| {
            Failure::new(
                ErrorCode::NotReady,
                format!(
                    "the daemon at {} did not answer {method} {path}: {e}",
                    self.addr
                ),
            )
            .with_hint("Retry, or run the command with --direct.")
        })?;
        if (200..300).contains(&status) {
            return serde_json::from_str(&text).map_err(|e| {
                Failure::new(
                    ErrorCode::Internal,
                    format!("the daemon's answer to {method} {path} is unreadable: {e}"),
                )
            });
        }
        Err(match serde_json::from_str::<ApiError>(&text) {
            Ok(api) => {
                let mut failure = Failure::new(api.code, api.detail)
                    .with_hint(api.hint)
                    .with_suggestions(api.suggestions);
                failure.upnp_code = api.upnp_code;
                failure
            }
            Err(_) => Failure::new(
                ErrorCode::Internal,
                format!("the daemon answered {method} {path} with {status}: {text}"),
            ),
        })
    }

    /// One HTTP/1.1 exchange over a fresh loopback connection: the status
    /// and the body.
    fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        timeout: Duration,
    ) -> io::Result<(u16, String)> {
        let mut stream = TcpStream::connect_timeout(&self.addr, timeout.min(PROBE))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let body = body.unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\nContent-Length: {}\r\n",
            self.addr,
            body.len()
        );
        if method != "GET" {
            request.push_str("Content-Type: application/json\r\n");
        }
        if let Some(token) = &self.token {
            let _ = write!(request, "{}: {token}\r\n", fsonos_api::identity::CLI_TOKEN);
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_response(&raw)
    }
}

/// `Ok(None)` (run directly), or the failure a caller requiring the daemon
/// gets.
fn refuse(require: bool, why: &str) -> Result<Option<Daemon>, Failure> {
    if require {
        Err(Failure::new(ErrorCode::NotReady, why.to_string())
            .with_hint("Start the daemon (fsonos serve)."))
    } else {
        Ok(None)
    }
}

/// Status and body of a raw HTTP/1.1 response (`Content-Length` or chunked).
fn parse_response(raw: &[u8]) -> io::Result<(u16, String)> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no end of headers"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut rest = &raw[split + 4..];
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| bad("no status line"))?;
    let chunked = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.trim().eq_ignore_ascii_case("chunked")
        })
    });
    if !chunked {
        return Ok((status, String::from_utf8_lossy(rest).into_owned()));
    }
    let mut body = Vec::new();
    loop {
        let end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| bad("truncated chunk"))?;
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&rest[..end])
                .split(';')
                .next()
                .unwrap_or("")
                .trim(),
            16,
        )
        .map_err(|_| bad("bad chunk size"))?;
        rest = &rest[end + 2..];
        if size == 0 {
            break;
        }
        let chunk = rest.get(..size).ok_or_else(|| bad("truncated chunk"))?;
        body.extend_from_slice(chunk);
        rest = rest.get(size + 2..).unwrap_or_default();
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// Percent-encode a path segment or query value.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Whether `command` has a route through the daemon.
fn routable(command: &Command) -> bool {
    matches!(
        command,
        Command::Zones
            | Command::Status { .. }
            | Command::Rooms { alias: None }
            | Command::Favorites { .. }
            | Command::Log { .. }
            | Command::Undo { .. }
            | Command::Play { search: None, .. }
            | Command::Pause { .. }
            | Command::Resume { .. }
            | Command::Next { .. }
            | Command::Previous { .. }
            | Command::Volume { .. }
            | Command::Mute { .. }
            | Command::Group { .. }
            | Command::Ungroup { .. }
            | Command::Move { .. }
            | Command::Party { .. }
    )
}

/// Carry out `command` through the daemon when one runs and the command has
/// a route; `Ok(false)` to run it directly.
pub fn run(global: &GlobalArgs, command: &Command) -> anyhow::Result<bool> {
    if !routable(command) {
        return Ok(false);
    }
    let Some(daemon) = Daemon::find(global, false)? else {
        return Ok(false);
    };
    let json = global.json;
    let done = |o: &OutcomeDto| format!("{}\n", o.done);
    match command {
        Command::Zones => {
            let zones: Vec<ZoneDto> = daemon.get("/zones")?;
            emit(json, &zones, |z| direct::zones_text(z))?;
        }
        Command::Status { zone } => {
            let state: ZoneStateDto = daemon.get(&format!("/zones/{}/state", pct(zone)))?;
            emit(json, &state, direct::status_text)?;
        }
        Command::Rooms { alias: None } => {
            let rooms: Vec<RoomDto> = daemon.get("/rooms")?;
            emit(json, &rooms, |r| direct::rooms_text(r))?;
        }
        Command::Favorites { zone } => {
            let favorites: Vec<FavoriteDto> =
                daemon.get(&format!("/favorites?zone={}", pct(zone)))?;
            emit(json, &favorites, |f| direct::favorites_text(f))?;
        }
        Command::Log {
            client,
            since,
            limit,
        } => {
            let mut query = format!("/actions?limit={limit}");
            if let Some(client) = client {
                let _ = write!(query, "&client={}", pct(client));
            }
            if let Some(since) = since.as_deref().map(crate::seconds_ago).transpose()? {
                let _ = write!(query, "&since={since}");
            }
            let actions: Vec<ActionDto> = daemon.get(&query)?;
            emit(json, &actions, |a| direct::actions_text(a))?;
        }
        Command::Undo { mine } => {
            let undone: UndoDto = daemon.post("/undo", &serde_json::json!({ "own_only": mine }))?;
            emit(json, &undone, |u: &UndoDto| format!("{}\n", u.summary))?;
        }
        Command::Play {
            zone,
            favorite: Some(favorite),
            ..
        } => {
            let req = PlayFavoriteRequest {
                zone: zone.clone(),
                favorite: favorite.clone(),
            };
            let outcome: OutcomeDto = daemon.post("/play/favorite", &req)?;
            emit(json, &outcome, done)?;
        }
        command => {
            let (path, body) = control_request(&daemon, command)?;
            let outcome: OutcomeDto = daemon.post(path, &body)?;
            emit(json, &outcome, done)?;
        }
    }
    Ok(true)
}

/// The route and body a control subcommand sends, as `plan_for` plans it
/// directly.
fn control_request(
    daemon: &Daemon,
    command: &Command,
) -> Result<(&'static str, serde_json::Value), Failure> {
    let value = |v: Result<serde_json::Value, serde_json::Error>| {
        v.map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))
    };
    let zone = |zone: &String| value(serde_json::to_value(ZoneRequest { zone: zone.clone() }));
    Ok(match command {
        Command::Play {
            zone,
            source_uri,
            title,
            ..
        } => {
            let source_uri = source_uri.clone().ok_or_else(|| {
                Failure::invalid("nothing to play")
                    .with_hint("Give a source URI, or --favorite <name> (see fsonos favorites).")
            })?;
            (
                "/play",
                value(serde_json::to_value(PlayRequest {
                    zone: zone.clone(),
                    source_uri,
                    title: title.clone(),
                }))?,
            )
        }
        Command::Pause { zone: z } => ("/pause", zone(z)?),
        Command::Resume { zone: z } => ("/resume", zone(z)?),
        Command::Next { zone: z } => ("/next", zone(z)?),
        Command::Previous { zone: z } => ("/previous", zone(z)?),
        Command::Ungroup { zone: z } => ("/ungroup", zone(z)?),
        Command::Volume { zone, level, group } => (
            "/volume",
            value(serde_json::to_value(crate::volume_request(
                zone, level, *group,
            )?))?,
        ),
        Command::Mute { zone, state } => (
            "/mute",
            value(serde_json::to_value(MuteRequest {
                zone: zone.clone(),
                mute: *state == Switch::On,
            }))?,
        ),
        Command::Group { zone, to } => (
            "/group",
            value(serde_json::to_value(GroupRequest {
                zone: zone.clone(),
                to: to.clone(),
            }))?,
        ),
        Command::Move { zone, to, copy } => (
            "/move",
            value(serde_json::to_value(MoveRequest {
                zone: zone.clone(),
                to: to.clone(),
                copy: *copy,
            }))?,
        ),
        Command::Party { target } => {
            // A household label names the household, as `party_request` decides.
            let zones: Vec<ZoneDto> = daemon.get("/zones")?;
            let req = match target.as_deref() {
                Some(t)
                    if zones
                        .iter()
                        .any(|z| z.household.eq_ignore_ascii_case(t.trim())) =>
                {
                    PartyRequest {
                        zone: None,
                        household: Some(t.to_owned()),
                    }
                }
                Some(t) => PartyRequest {
                    zone: Some(t.to_owned()),
                    household: None,
                },
                None => PartyRequest::default(),
            };
            ("/party", value(serde_json::to_value(req))?)
        }
        _ => {
            return Err(Failure::new(ErrorCode::Internal, "not a control command"));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_parse_with_a_length_or_in_chunks() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(parse_response(plain).unwrap(), (200, "[]".to_string()));
        let chunked =
            b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;x=y\r\n\":1}\r\n0\r\n\r\n";
        assert_eq!(
            parse_response(chunked).unwrap(),
            (404, "{\"a\":1}".to_string())
        );
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(pct("Living Room"), "Living%20Room");
        assert_eq!(pct("Kitchen@S1/x"), "Kitchen%40S1%2Fx");
        assert_eq!(pct("Wohnzimmer-ü"), "Wohnzimmer-%C3%BC");
    }

    #[test]
    fn the_daemon_file_round_trips_owner_only_and_is_removed_only_by_its_writer() {
        let dir = std::env::temp_dir().join(format!("fsonos-daemon-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = DaemonFile {
            http: "127.0.0.1:8099".parse().unwrap(),
            cli_token: "0123456789abcdef0123456789abcdef".into(),
            pid: 42,
        };
        write_daemon_file(&dir, &file).unwrap();
        assert_eq!(read_daemon_file(&dir), Some(file));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(DAEMON_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        remove_daemon_file(&dir, 7);
        assert!(
            read_daemon_file(&dir).is_some(),
            "another daemon's file stays"
        );
        remove_daemon_file(&dir, 42);
        assert!(read_daemon_file(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_daemon_is_noticed_within_the_probe() {
        // A port nothing listens on: the probe fails fast, never hangs.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = free.local_addr().unwrap();
        drop(free);
        let daemon = Daemon { addr, token: None };
        let started = std::time::Instant::now();
        assert!(daemon.healthy().is_err());
        assert!(started.elapsed() < PROBE * 4, "{:?}", started.elapsed());
    }
}
