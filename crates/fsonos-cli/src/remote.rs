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
//! `--daemon` requires the daemon; `--direct` never asks it. From another
//! machine, `--daemon-url` (`FSONOS_DAEMON_URL`) names the daemon by URL,
//! Tailscale Serve's HTTPS one included: every command goes there, as the
//! caller the daemon sees (a tailnet login behind Serve). A command
//! confined to a simulator (`--routes`) asks only a daemon running under the
//! same routes file, and one without it never asks a simulator's daemon.
//! The DJ's reads (`fsonos dj status|why|moods`, see `crate::dj_cmd`), its
//! feedback and preferences (`fsonos dj like|dislike|prefs`) and
//! `fsonos scene apply` find it the same way; `fsonos events` streams
//! from it ([`Daemon::open_stream`]) and has no direct mode.
//!
//! Through the daemon a command sends the same request body the HTTP API
//! documents and prints the same DTO the direct path prints, so `--json`
//! output is the same either way; `fsonos sleep` through the daemon gets
//! its fade, `fsonos play --search` searches the daemon's library and
//! favorites, and `fsonos dj start|skip|stop|steer` reach the DJ that keeps
//! the group's queue going. Scenes, schedules and `fsonos policy show` read
//! and change the daemon's, so they reach its store from another machine too.
//! Commands with no route here (doctor, `policy check`, ...) run directly.

use fsonos_api::surface::announce::{AnnounceDto, AnnounceRequest};
use fsonos_api::surface::players::PlayerDto;
use fsonos_api::surface::queue::{QueueAddedDto, QueueDto};
use fsonos_api::surface::schedules::{FadePauseRequest, SleepTimerDto};
use fsonos_api::{
    ActionDto, ApiError, ErrorCode, Failure, FavoriteDto, GroupRequest, HitDto, MoveRequest,
    MuteRequest, OutcomeDto, PartyRequest, PlayDto, PlayFavoriteRequest, PlayRequest, RoomDto,
    SearchPlayRequest, SearchRequest, UndoDto, ZoneDto, ZoneRequest, ZoneStateDto,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::GlobalArgs;
use crate::dj_cmd::DjAction;
use crate::{Command, Switch, announce_cmd, direct, emit, schedule_cmd};

/// The daemon's address file in the data directory.
pub const DAEMON_FILE: &str = "daemon.json";

/// How long the `GET /health` probe may take before the command runs
/// directly.
pub const PROBE: Duration = Duration::from_millis(150);

/// How long a command through the daemon may take.
const CALL: Duration = Duration::from_secs(120);

/// How long a daemon named by URL may take to answer its probe: a TLS
/// handshake across the tailnet, not a loopback connect.
const URL_PROBE: Duration = Duration::from_secs(10);

/// Synthesis and playback can each take minutes; allow the daemon's
/// twenty-minute request deadline to report its outcome first.
const ANNOUNCE_CALL: Duration =
    Duration::from_secs(fsonos_api::surface::announce::ANNOUNCE_TIMEOUT_SECS + 30);

/// What `fsonos serve` leaves in the data directory for the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonFile {
    /// The loopback HTTP API.
    pub http: SocketAddr,
    /// The CLI's token (see the module docs).
    pub cli_token: String,
    pub pid: u32,
    /// The routes file it runs under (a simulator's, see `fsonos sim`), if
    /// any: a command asks it only when it runs under the same file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<PathBuf>,
    /// The listener settings it runs with (none from an older daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serve: Option<ServeSettings>,
}

/// The settings `fsonos serve` runs with that a command run elsewhere does
/// not see in its own environment: `fsonos doctor` checks a running daemon
/// by these, not by its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeSettings {
    /// `--http` / `FSONOS_HTTP_ADDR`, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<SocketAddr>,
    /// `--mcp-http` / `FSONOS_MCP_HTTP_ADDR`, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_http: Option<SocketAddr>,
    /// `--tailscale-serve` / `FSONOS_TAILSCALE_SERVE`.
    #[serde(default)]
    pub tailscale_serve: bool,
    /// `--allow-unsafe-bind`.
    #[serde(default)]
    pub allow_unsafe_bind: bool,
    /// `--tailscale off` / `FSONOS_TAILSCALE=off`: no tailnet.
    #[serde(default)]
    pub tailscale_off: bool,
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

/// The data directory's daemon lock: `fsonos serve` holds it for its life,
/// and the OS lets go of it when the daemon exits, however it exits.
pub const DAEMON_LOCK: &str = "daemon.lock";

/// Whether this process may serve a data directory.
#[derive(Debug)]
pub enum DirLock {
    /// The lock, held for as long as the file stays open.
    Held(std::fs::File),
    /// Another daemon serves it; its daemon.json, once it has written one.
    Busy(Option<DaemonFile>),
}

/// Take `dir`'s daemon lock for this process, or say which daemon holds it.
/// A lock, not a probe of daemon.json: two daemons starting together cannot
/// both pass, and a file a dead daemon left behind does not count.
pub fn lock_data_dir(dir: &Path) -> io::Result<DirLock> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(DAEMON_LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(DirLock::Held(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(DirLock::Busy(read_daemon_file(dir))),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Remove `daemon.json` if it is the one `pid` wrote.
pub fn remove_daemon_file(dir: &Path, pid: u32) {
    if read_daemon_file(dir).is_some_and(|f| f.pid == pid) {
        let _ = std::fs::remove_file(dir.join(DAEMON_FILE));
    }
}

pub(crate) fn read_daemon_file(dir: &Path) -> Option<DaemonFile> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(DAEMON_FILE)).ok()?).ok()
}

/// Where a daemon answers: its loopback socket (from daemon.json or
/// `FSONOS_HTTP_ADDR`), or a URL (`--daemon-url`).
#[derive(Debug, Clone)]
enum Endpoint {
    Socket(SocketAddr),
    Url(String),
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(addr) => write!(f, "{addr}"),
            Self::Url(url) => f.write_str(url),
        }
    }
}

/// A running daemon that answered the probe.
#[derive(Debug, Clone)]
pub struct Daemon {
    addr: Endpoint,
    token: Option<String>,
    /// The local client the calls come from when not the CLI
    /// ([`Daemon::for_stdio_mcp`]).
    claim: Option<&'static str>,
}

impl Daemon {
    /// The daemon to send commands to: `None` to run directly (`--direct`,
    /// no daemon known, or none answering), or, when the caller `require`s
    /// one, why there is none.
    pub fn find(global: &GlobalArgs, require: bool) -> Result<Option<Self>, Failure> {
        if global.direct {
            return Ok(None);
        }
        if let Some(url) = &global.daemon_url {
            return Self::at_url(url).map(Some);
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
        let vouched = file.filter(|f| f.http == addr);
        // A routes file confines a command to the simulator it names: such a
        // command asks only a daemon running under the same file, and one
        // without it never asks a simulator's daemon.
        let daemon_routes = vouched.as_ref().and_then(|f| f.routes.as_deref());
        if !same_routes(daemon_routes, global.routes.as_deref()) {
            if !require {
                return Ok(None);
            }
            let under = |r: Option<&Path>| {
                r.map_or_else(
                    || "no routes file".to_string(),
                    |r| format!("routes file {}", r.display()),
                )
            };
            return Err(Failure::new(
                ErrorCode::NotReady,
                format!(
                    "the daemon at {addr} runs under {}, this command under {}",
                    under(daemon_routes),
                    under(global.routes.as_deref())
                ),
            )
            .with_hint("Start fsonos serve with the same --routes, or drop --daemon."));
        }
        let token = vouched.map(|f| f.cli_token);
        if token.is_none() && !require {
            // Without the token its answers would be a local process's, not
            // the CLI's: run directly so the policy reads the same.
            return Ok(None);
        }
        let daemon = Self {
            addr: Endpoint::Socket(addr),
            token,
            claim: None,
        };
        match daemon.healthy() {
            Ok(()) => Ok(Some(daemon)),
            Err(why) => refuse(require, &format!("no daemon answered at {addr}: {why}")),
        }
    }

    /// The daemon at `url` (`--daemon-url`), which must answer: a remote
    /// caller never runs directly against the LAN it is not on.
    fn at_url(url: &str) -> Result<Self, Failure> {
        let url = url.trim().trim_end_matches('/');
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(Failure::invalid(format!(
                "--daemon-url wants an http:// or https:// URL, got {url:?}"
            ))
            .with_hint("e.g. https://<mac>.<tailnet>.ts.net behind Tailscale Serve."));
        }
        let daemon = Self {
            addr: Endpoint::Url(url.to_owned()),
            token: None,
            claim: None,
        };
        daemon.healthy().map_err(|why| {
            Failure::new(
                ErrorCode::NotReady,
                format!("no daemon answered at {url}: {why}"),
            )
            .with_hint(
                "Check the URL, that fsonos serve runs there, and that this machine is on \
                 the tailnet.",
            )
        })?;
        Ok(daemon)
    }

    /// Call as the stdio MCP server (`fsonos mcp`) rather than the CLI, so
    /// the house policy's rules for that client apply on the daemon too.
    #[must_use]
    pub fn for_stdio_mcp(mut self) -> Self {
        self.claim = Some("mcp-stdio");
        self
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

    /// `GET path`: the answer as `T`, or the daemon's failure.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.call("GET", path, None, CALL)
    }

    /// `DELETE path`: the answer as `T`, or the daemon's failure.
    pub fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.call("DELETE", path, None, CALL)
    }

    /// `PUT path` with no body: the answer as `T`, or the daemon's failure.
    pub fn put<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.call("PUT", path, None, CALL)
    }

    /// `PUT path` with `body` as JSON.
    pub fn put_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, Failure> {
        let body = serde_json::to_string(body)
            .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))?;
        self.call("PUT", path, Some(&body), CALL)
    }

    pub fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, Failure> {
        self.post_with_timeout(path, body, CALL)
    }

    /// The announcement response arrives after synthesis, playback, and restore.
    pub fn announce(&self, req: &AnnounceRequest) -> Result<AnnounceDto, Failure> {
        self.post_with_timeout("/announce", req, ANNOUNCE_CALL)
    }

    /// `POST /announce` with an announcement's body as JSON (the stdio MCP
    /// server's), waiting as long as [`Self::announce`] does.
    pub fn announce_json(&self, body: &serde_json::Value) -> Result<serde_json::Value, Failure> {
        self.post_with_timeout("/announce", body, ANNOUNCE_CALL)
    }

    /// `method path` with `body`, as the stdio MCP server forwards a tool's
    /// call. A method this client does not send is refused, never sent as
    /// another one.
    pub fn forward(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, Failure> {
        let empty = serde_json::json!({});
        match (method, body) {
            ("GET", _) => self.get(path),
            ("PUT", None) => self.put(path),
            ("PUT", Some(body)) => self.put_json(path, body),
            ("DELETE", _) => self.delete(path),
            // An announcement answers once it has played and the music is back.
            ("POST", _) if path == "/announce" => self.announce_json(body.unwrap_or(&empty)),
            ("POST", _) => self.post(path, body.unwrap_or(&empty)),
            _ => Err(Failure::new(
                ErrorCode::Internal,
                format!("the daemon client does not send {method} {path}"),
            )),
        }
    }

    fn post_with_timeout<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<T, Failure> {
        let body = serde_json::to_string(body)
            .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))?;
        self.call("POST", path, Some(&body), timeout)
    }

    fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        timeout: Duration,
    ) -> Result<T, Failure> {
        let response = self.exchange(method, path, body, timeout);
        let (status, text) = response.map_err(|e| {
            // A dropped response does not mean playback stopped. Mark its
            // unknown outcome non-retryable; never replay it in direct mode.
            let (code, hint) = if method == "POST" && path == "/announce" {
                (
                    ErrorCode::Internal,
                    "The announcement may still be running. Check playback before sending it again.",
                )
            } else {
                (ErrorCode::NotReady, "Retry, or run the command with --direct.")
            };
            let detail = format!(
                "the daemon at {} did not answer {method} {path}: {e}",
                self.addr
            );
            Failure::new(code, detail).with_hint(hint)
        })?;
        if (200..300).contains(&status) {
            return serde_json::from_str(&text).map_err(|e| {
                Failure::new(
                    ErrorCode::Internal,
                    format!("the daemon's answer to {method} {path} is unreadable: {e}"),
                )
            });
        }
        Err(refused(method, path, status, &text))
    }

    /// `GET path` held open, as server-sent events are: the body as it
    /// arrives once the daemon answered 200, and whether it comes chunked.
    /// A read waits at most `quiet`.
    pub fn open_stream(
        &self,
        path: &str,
        quiet: Duration,
    ) -> Result<(BufReader<TcpStream>, bool), Failure> {
        let lost = |e: io::Error| {
            Failure::new(
                ErrorCode::NotReady,
                format!("the daemon at {} did not answer GET {path}: {e}", self.addr),
            )
            .with_hint("Check that fsonos serve runs.")
        };
        let Endpoint::Socket(addr) = &self.addr else {
            return Err(Failure::new(
                ErrorCode::NotImplemented,
                format!(
                    "the event stream comes only from a daemon on this machine, not {}",
                    self.addr
                ),
            )
            .with_hint("Run fsonos events on the daemon's host, or read GET /events there."));
        };
        let mut stream = TcpStream::connect_timeout(addr, PROBE).map_err(lost)?;
        stream.set_read_timeout(Some(quiet)).map_err(lost)?;
        stream.set_write_timeout(Some(CALL)).map_err(lost)?;
        let request = self.request("GET", path, "", "text/event-stream");
        stream.write_all(request.as_bytes()).map_err(lost)?;
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if reader.read_until(b'\n', &mut head).map_err(lost)? == 0 {
                return Err(lost(io::ErrorKind::UnexpectedEof.into()));
            }
        }
        let (status, chunked) = parse_head(&String::from_utf8_lossy(&head))
            .ok_or_else(|| lost(io::ErrorKind::InvalidData.into()))?;
        if status != 200 {
            reader.read_to_end(&mut head).map_err(lost)?;
            let (status, text) = parse_response(&head).map_err(lost)?;
            return Err(refused("GET", path, status, &text));
        }
        Ok((reader, chunked))
    }

    /// One HTTP/1.1 exchange over a fresh loopback connection (or through
    /// the HTTP client, to a daemon named by URL): the status and the body.
    fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        timeout: Duration,
    ) -> io::Result<(u16, String)> {
        let addr = match &self.addr {
            Endpoint::Socket(addr) => addr,
            Endpoint::Url(url) => {
                return exchange_url(url, method, path, body, timeout.max(URL_PROBE));
            }
        };
        let mut stream = TcpStream::connect_timeout(addr, timeout.min(PROBE))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let request = self.request(method, path, body.unwrap_or_default(), "application/json");
        stream.write_all(request.as_bytes())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_response(&raw)
    }

    /// The HTTP/1.1 request: its head, as the CLI (or the stdio MCP
    /// server), then `body`.
    fn request(&self, method: &str, path: &str, body: &str, accept: &str) -> String {
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {accept}\r\nContent-Length: {}\r\n",
            self.addr,
            body.len()
        );
        if method != "GET" {
            request.push_str("Content-Type: application/json\r\n");
        }
        if let Some(token) = &self.token {
            let _ = write!(request, "{}: {token}\r\n", fsonos_api::identity::CLI_TOKEN);
            if let Some(claim) = self.claim {
                let _ = write!(
                    request,
                    "{}: {claim}\r\n",
                    fsonos_api::identity::LOCAL_CLIENT
                );
            }
        }
        request.push_str("\r\n");
        request.push_str(body);
        request
    }
}

/// One request to the daemon at `url` through asupersync's HTTP client (TLS
/// for https, as Tailscale Serve answers): the status and the body.
fn exchange_url(
    url: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> io::Result<(u16, String)> {
    let (target, method, body) = (
        format!("{url}{path}"),
        method.to_owned(),
        body.map(str::to_owned),
    );
    let runtime = crate::runtime().map_err(|e| io::Error::other(e.to_string()))?;
    runtime.block_on(async move {
        let cx = asupersync::Cx::current().ok_or_else(|| io::Error::other("no ambient Cx"))?;
        let client = asupersync::http::Client::default_for_runtime(&cx);
        let request = match method.as_str() {
            "POST" => client.post(target),
            "PUT" => client.put(target),
            "DELETE" => client.delete(target),
            _ => client.get(target),
        };
        let mut request = request
            .header("Accept", "application/json")
            .timeout(timeout);
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body);
        }
        let response = request
            .send(&cx)
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok((
            response.status,
            String::from_utf8_lossy(&response.body).into_owned(),
        ))
    })
}

/// The daemon's refusal of `method path` (`status`, body `text`) as the
/// failure it reported.
fn refused(method: &str, path: &str, status: u16, text: &str) -> Failure {
    match serde_json::from_str::<ApiError>(text) {
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
    }
}

/// A routes file as daemon.json records it: canonical, so the same file
/// named another way matches.
pub fn canonical(routes: &Path) -> PathBuf {
    std::fs::canonicalize(routes).unwrap_or_else(|_| routes.to_path_buf())
}

/// Whether a daemon under routes file `daemon` and a command under
/// `command` are confined alike: both to the same file, or neither.
fn same_routes(daemon: Option<&Path>, command: Option<&Path>) -> bool {
    match (daemon, command) {
        (None, None) => true,
        (Some(d), Some(c)) => canonical(d) == canonical(c),
        _ => false,
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

/// A response head's status, and whether its body comes chunked.
fn parse_head(head: &str) -> Option<(u16, bool)> {
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())?;
    let chunked = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.trim().eq_ignore_ascii_case("chunked")
        })
    });
    Some((status, chunked))
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
    let (status, chunked) = parse_head(&head).ok_or_else(|| bad("no status line"))?;
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
pub fn pct(s: &str) -> String {
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
        Command::Discover
            | Command::Zones { .. }
            | Command::Search { .. }
            | Command::Status { .. }
            | Command::Rooms { alias: None }
            | Command::Favorites { .. }
            | Command::Queue { .. }
            | Command::Eq(_)
            | Command::History { .. }
            | Command::Log { .. }
            | Command::Undo { .. }
            | Command::Play { .. }
            | Command::Pause { .. }
            | Command::Resume { .. }
            | Command::Next { .. }
            | Command::Previous { .. }
            | Command::Seek { .. }
            | Command::Volume { .. }
            | Command::Mute { .. }
            | Command::Group { .. }
            | Command::Ungroup { .. }
            | Command::Move { .. }
            | Command::Party { .. }
            | Command::Sleep(_)
            | Command::Say(_)
            | Command::Chime(_)
            | Command::Announce(_)
    ) || matches!(command, Command::Dj { action } if action.daemon_request().is_some())
}

/// Carry out `command` through the daemon when one runs and the command has
/// a route; `Ok(false)` to run it directly.
pub fn run(global: &GlobalArgs, command: &Command) -> anyhow::Result<bool> {
    if !routable(command) {
        return Ok(false);
    }
    let Some(daemon) = Daemon::find(global, global.daemon)? else {
        warn_direct(command);
        return Ok(false);
    };
    let json = global.json;
    // Its notes too: a clamped volume, a room skipped, a command healed.
    let done = crate::outcome_text;
    match command {
        Command::Search { query, zone, limit } => crate::search(
            global,
            &crate::search_request(query, zone.as_deref(), *limit),
            &daemon,
        )?,
        Command::Discover
        | Command::Zones { .. }
        | Command::Status { .. }
        | Command::Rooms { alias: None }
        | Command::Favorites { .. }
        | Command::Queue { .. }
        | Command::History { .. }
        | Command::Log { .. } => read(&daemon, global, command)?,
        Command::Undo { preview: true, .. } => {
            let next: Option<ActionDto> = daemon.get("/undo/preview")?;
            emit(json, &next, |a: &Option<ActionDto>| {
                crate::direct::undo_preview_text(a.as_ref())
            })?;
        }
        Command::Undo { id: Some(id), .. } => {
            let undone: UndoDto =
                daemon.post("/undo/checked", &serde_json::json!({ "expected_id": id }))?;
            emit(json, &undone, |u: &UndoDto| format!("{}\n", u.summary))?;
        }
        Command::Undo { mine, force, .. } => {
            // `force` goes only when asked, so a plain undo still reaches a
            // daemon that predates it.
            let mut body = serde_json::json!({ "own_only": mine });
            if *force {
                body["force"] = serde_json::Value::Bool(true);
            }
            let undone: UndoDto = daemon.post("/undo", &body)?;
            emit(json, &undone, |u: &UndoDto| format!("{}\n", u.summary))?;
        }
        Command::Play {
            zone,
            search: Some(query),
            pick,
            ..
        } => crate::play_search(global, zone.clone(), query.clone(), *pick, &daemon)?,
        Command::Sleep(args) => sleep(&daemon, json, args)?,
        Command::Eq(args) => crate::eq_cmd::through(&daemon, json, args)?,
        Command::Say(args) => {
            let announced = daemon.announce(&args.request())?;
            emit(json, &announced, announce_cmd::text)?;
        }
        Command::Chime(args) => {
            let announced = daemon.announce(&args.request())?;
            emit(json, &announced, announce_cmd::text)?;
        }
        Command::Announce(args) => {
            let announced = daemon.announce(&args.request()?)?;
            emit(json, &announced, announce_cmd::text)?;
        }
        Command::Play {
            zone,
            favorite: Some(favorite),
            next: false,
            queue: false,
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
            if let Some(req) = crate::queue_request(command) {
                let added: QueueAddedDto = daemon.post("/queue", &req)?;
                emit(json, &added, QueueAddedDto::text)?;
                return Ok(true);
            }
            let (path, body) = control_request(&daemon, command)?;
            let outcome: OutcomeDto = daemon.post(path, &body)?;
            emit(json, &outcome, done)?;
        }
    }
    Ok(true)
}

/// The house's reads through the daemon: the players, zones and rooms, a
/// room's state, favorites, queue and history, and the action log.
fn read(daemon: &Daemon, global: &GlobalArgs, command: &Command) -> anyhow::Result<()> {
    let json = global.json;
    match command {
        Command::Discover => {
            let players: Vec<PlayerDto> = daemon.get("/players")?;
            remember_rooms(global, players.iter().map(|p| p.room.clone()));
            // The daemon keeps only the players it can read, and discovers on
            // its own: nothing to report as unreachable or failed here.
            let found = direct::DiscoverDto {
                players,
                unreachable: Vec::new(),
                ssdp_error: None,
            };
            emit(json, &found, direct::discover_text)?;
        }
        Command::Zones { zone: Some(room) } => {
            let zone: ZoneDto = daemon.get(&format!("/zones/{}", pct(room)))?;
            emit(json, &zone, |z| direct::zones_text(std::slice::from_ref(z)))?;
        }
        Command::Zones { zone: None } => {
            let zones: Vec<ZoneDto> = daemon.get("/zones")?;
            remember_rooms(global, zones.iter().flat_map(|z| z.members.iter().cloned()));
            emit(json, &zones, |z| direct::zones_text(z))?;
        }
        Command::Status { zone } => {
            let state: ZoneStateDto = daemon.get(&format!("/zones/{}/state", pct(zone)))?;
            emit(json, &state, direct::status_text)?;
        }
        Command::Rooms { alias: None } => {
            let rooms: Vec<RoomDto> = daemon.get("/rooms")?;
            remember_rooms(global, rooms.iter().map(|r| r.name.clone()));
            emit(json, &rooms, |r| direct::rooms_text(r))?;
        }
        Command::Favorites { zone } => {
            let favorites: Vec<FavoriteDto> =
                daemon.get(&format!("/favorites?zone={}", pct(zone)))?;
            emit(json, &favorites, |f| direct::favorites_text(f))?;
        }
        Command::Queue {
            zone,
            offset,
            limit,
        } => {
            let queue: QueueDto = daemon.get(&format!(
                "/zones/{}/queue?offset={offset}&limit={limit}",
                pct(zone)
            ))?;
            emit(json, &queue, QueueDto::text)?;
        }
        Command::History { zone, limit } => {
            let mut path = format!("/history?limit={limit}");
            if let Some(zone) = zone {
                let _ = write!(path, "&zone={}", pct(zone));
            }
            let plays: Vec<PlayDto> = daemon.get(&path)?;
            emit(json, &plays, |p| direct::history_text(p))?;
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
        _ => unreachable!("not a read through the daemon"),
    }
    Ok(())
}

/// What a command with a route loses run directly: a DJ started from here
/// plays the works it queues, with no daemon to queue more.
fn warn_direct(command: &Command) {
    if let Command::Dj {
        action: DjAction::Start(_),
    } = command
    {
        tracing::warn!(
            "the DJ runs in fsonos serve: started without it, it plays \
             the works it queues now and no more"
        );
    }
}

/// `fsonos sleep` through the daemon: set, extend or cancel its fading
/// timer, or show the group's.
fn sleep(daemon: &Daemon, json: bool, args: &schedule_cmd::SleepArgs) -> anyhow::Result<()> {
    match schedule_cmd::sleep_request(args)? {
        None => {
            let path = args.zone.as_deref().map_or_else(
                || "/sleep".to_owned(),
                |z| format!("/sleep?zone={}", pct(z)),
            );
            let timers: Vec<SleepTimerDto> = daemon.get(&path)?;
            emit(json, &timers, |t| {
                schedule_cmd::timers_text(args.zone.as_deref(), t)
            })
        }
        Some(req) => {
            let timer: SleepTimerDto = daemon.post("/sleep", &req)?;
            emit(json, &timer, schedule_cmd::timer_text)
        }
    }
}

impl crate::SearchAndPlay for Daemon {
    fn search(&self, req: &SearchRequest) -> Result<Vec<HitDto>, Failure> {
        let mut path = format!("/library/search?q={}", pct(&req.query));
        if let Some(zone) = &req.zone {
            let _ = write!(path, "&zone={}", pct(zone));
        }
        if let Some(limit) = req.limit {
            let _ = write!(path, "&limit={limit}");
        }
        self.get(&path)
    }

    fn search_and_play(&self, req: &SearchPlayRequest) -> Result<OutcomeDto, Failure> {
        self.post("/library/play", req)
    }
}

/// Refresh the room cache completions read (see `crate::completions`).
fn remember_rooms(global: &GlobalArgs, rooms: impl IntoIterator<Item = String>) {
    if let Some(dir) = global.data_dir() {
        crate::completions::remember_rooms(&dir, rooms);
    }
}

/// The route and body of a `--fade` pause or volume, which have routes of
/// their own; `None` for anything else.
fn fade_request(command: &Command) -> Result<Option<(&'static str, serde_json::Value)>, Failure> {
    let (path, body) = match command {
        Command::Pause {
            zone,
            fade: Some(secs),
        } => (
            "/pause/fade",
            serde_json::to_value(FadePauseRequest {
                zone: zone.clone(),
                fade_secs: *secs,
            }),
        ),
        Command::Volume {
            zone,
            level,
            group,
            fade: Some(secs),
        } => (
            "/volume/fade",
            serde_json::to_value(crate::fade_volume_request(zone, level, *group, *secs)?),
        ),
        _ => return Ok(None),
    };
    let body = body.map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))?;
    Ok(Some((path, body)))
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
    if let Some(fade) = fade_request(command)? {
        return Ok(fade);
    }
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
        Command::Pause { zone: z, .. } => ("/pause", zone(z)?),
        Command::Resume { zone: z } => ("/resume", zone(z)?),
        Command::Next { zone: z } => ("/next", zone(z)?),
        Command::Previous { zone: z } => ("/previous", zone(z)?),
        Command::Seek { zone, position } => (
            "/seek",
            value(serde_json::to_value(crate::seek_request(zone, position)?))?,
        ),
        Command::Ungroup { zone: z } => ("/ungroup", zone(z)?),
        Command::Volume {
            zone, level, group, ..
        } => (
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
        Command::Dj { action } => match action.daemon_request() {
            Some((path, body)) => (path, value(body)?),
            None => return Err(Failure::new(ErrorCode::Internal, "not a DJ control")),
        },
        _ => {
            return Err(Failure::new(ErrorCode::Internal, "not a control command"));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own: the pid alone recurs across runs on a
    /// worker (and across jobs in other pid namespaces sharing its temp dir).
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("fsonos-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn every_announcement_command_has_a_daemon_route() {
        use clap::Parser;
        for args in [
            vec!["fsonos", "say", "hello"],
            vec!["fsonos", "chime", "bell"],
            vec!["fsonos", "announce", "--file", "clip.wav"],
        ] {
            assert!(routable(&crate::Cli::try_parse_from(args).unwrap().command));
        }
    }

    #[test]
    fn a_lost_announcement_response_never_suggests_replaying_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let daemon = Daemon {
            addr: Endpoint::Socket(listener.local_addr().unwrap()),
            token: None,
            claim: None,
        };
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut line = String::new();
            let mut body_len = 0;
            loop {
                use std::io::BufRead as _;
                line.clear();
                assert!(
                    reader.read_line(&mut line).unwrap() > 0,
                    "request headers ended early"
                );
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    body_len = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; body_len];
            reader.read_exact(&mut body).unwrap();
            let request: AnnounceRequest = serde_json::from_slice(&body).unwrap();
            assert_eq!(request.chime.as_deref(), Some("bell"));
            // The command was received, but no response reached its caller.
        });
        let failure = daemon
            .announce(&AnnounceRequest {
                chime: Some("bell".into()),
                ..AnnounceRequest::default()
            })
            .unwrap_err();
        worker.join().unwrap();
        assert!(!failure.retryable());
        assert!(failure.hint.contains("may still be running"));
        assert!(!failure.hint.contains("--direct"));
    }

    /// What a daemon at a fresh listener receives for `forward(method, ...)`:
    /// its request line and body. It answers nothing.
    fn forwarded(method: &str, body: Option<&serde_json::Value>) -> (String, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let daemon = Daemon {
            addr: Endpoint::Socket(listener.local_addr().unwrap()),
            token: None,
            claim: None,
        };
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();
            (
                first.trim_end().to_owned(),
                String::from_utf8(sent).unwrap(),
            )
        });
        let _ = daemon.forward(method, "/scenes/evening", body);
        worker.join().unwrap()
    }

    #[test]
    fn the_stdio_server_forwards_each_method_as_itself() {
        let (line, _) = forwarded("DELETE", None);
        assert!(line.starts_with("DELETE /scenes/evening "), "{line}");
        let levels = serde_json::json!({ "volume": 12 });
        let (line, body) = forwarded("PUT", Some(&levels));
        assert!(line.starts_with("PUT /scenes/evening "), "{line}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            levels
        );
        let (line, body) = forwarded("POST", None);
        assert!(line.starts_with("POST /scenes/evening "), "{line}");
        assert_eq!(body, "{}");

        // A method the client does not send is refused, with no request.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let daemon = Daemon {
            addr: Endpoint::Socket(listener.local_addr().unwrap()),
            token: None,
            claim: None,
        };
        let refused = daemon
            .forward("PATCH", "/scenes/evening", None)
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Internal);
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "nothing was sent");
    }

    #[test]
    fn the_stdio_server_names_itself_only_alongside_the_token() {
        let headers = |daemon: Daemon, listener: std::net::TcpListener| {
            let worker = std::thread::spawn(move || {
                use std::io::BufRead as _;
                let (stream, _) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream);
                let mut seen = Vec::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    seen.push(line.trim_end().to_lowercase());
                }
                seen
            });
            // No answer comes back; only the request matters here.
            let _ = daemon.get::<serde_json::Value>("/zones");
            worker.join().unwrap()
        };
        let bind = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listener = bind();
        let with_token = Daemon {
            addr: Endpoint::Socket(listener.local_addr().unwrap()),
            token: Some("secret".into()),
            claim: None,
        }
        .for_stdio_mcp();
        let sent = headers(with_token, listener);
        assert!(
            sent.contains(&"x-fsonos-client: mcp-stdio".to_string()),
            "{sent:?}"
        );
        // Without the token there is nothing to claim with.
        let listener = bind();
        let without = Daemon {
            addr: Endpoint::Socket(listener.local_addr().unwrap()),
            token: None,
            claim: None,
        }
        .for_stdio_mcp();
        let sent = headers(without, listener);
        assert!(
            !sent.iter().any(|h| h.starts_with("x-fsonos-client")),
            "{sent:?}"
        );
    }

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
        let dir = scratch_dir("daemon-file");
        std::fs::create_dir_all(&dir).unwrap();
        let file = DaemonFile {
            http: "127.0.0.1:8099".parse().unwrap(),
            cli_token: "0123456789abcdef0123456789abcdef".into(),
            pid: 42,
            routes: None,
            serve: None,
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
        let daemon = Daemon {
            addr: Endpoint::Socket(addr),
            token: None,
            claim: None,
        };
        let started = std::time::Instant::now();
        assert!(daemon.healthy().is_err());
        assert!(started.elapsed() < PROBE * 4, "{:?}", started.elapsed());
    }

    /// A daemon that answers `GET /health` (and nothing else), for `n`
    /// connections.
    fn healthy_daemon(n: usize) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(n) {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = r#"{"status":"ok"}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        addr
    }

    #[test]
    fn the_data_directory_lock_admits_one_daemon() {
        let dir = scratch_dir("lock");
        let DirLock::Held(held) = lock_data_dir(&dir).unwrap() else {
            panic!("the first take holds the lock");
        };
        // While it is held, another take is refused and names the daemon
        // its daemon.json records.
        let file = DaemonFile {
            http: "127.0.0.1:8099".parse().unwrap(),
            cli_token: "0123456789abcdef0123456789abcdef".into(),
            pid: 42,
            routes: None,
            serve: None,
        };
        write_daemon_file(&dir, &file).unwrap();
        assert!(
            matches!(lock_data_dir(&dir).unwrap(), DirLock::Busy(Some(f)) if f.pid == 42),
            "a second daemon is refused"
        );
        // Once the holder is gone, the next daemon takes it, whatever
        // daemon.json says.
        drop(held);
        // A child another test forks in this window shares the lock's file
        // description until it execs, so the lock can outlive the drop for
        // a moment: try again briefly, as a restarting daemon would.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let retaken = loop {
            match lock_data_dir(&dir).unwrap() {
                DirLock::Held(_) => break true,
                DirLock::Busy(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                DirLock::Busy(_) => break false,
            }
        };
        assert!(retaken, "once the holder is gone, the next daemon takes it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_simulator_command_asks_only_a_daemon_under_the_same_routes_file() {
        use clap::Parser;
        let dir = scratch_dir("confined");
        std::fs::create_dir_all(&dir).unwrap();
        let routes = dir.join("routes.toml");
        std::fs::write(&routes, "[routes]\n").unwrap();
        let (dir_s, routes_s) = (dir.to_str().unwrap(), routes.to_str().unwrap());
        let global = |args: &[&str]| {
            let mut all = vec!["fsonos", "--data-dir", dir_s];
            all.extend_from_slice(args);
            all.push("zones");
            crate::Cli::try_parse_from(all).unwrap().global
        };
        let daemon_file = |http: SocketAddr, routes: Option<&Path>| {
            let file = DaemonFile {
                http,
                cli_token: "0123456789abcdef0123456789abcdef".into(),
                pid: 42,
                routes: routes.map(Path::to_path_buf),
                serve: None,
            };
            write_daemon_file(&dir, &file).unwrap();
        };

        // The real house's daemon (no routes file).
        daemon_file(healthy_daemon(1), None);
        assert!(
            Daemon::find(&global(&[]), false).unwrap().is_some(),
            "a plain command asks it"
        );
        assert!(
            Daemon::find(&global(&["--routes", routes_s]), false)
                .unwrap()
                .is_none(),
            "a simulator command runs directly instead"
        );
        let refused = Daemon::find(&global(&["--routes", routes_s, "--daemon"]), true).unwrap_err();
        assert!(
            refused.detail.contains("runs under no routes file"),
            "{}",
            refused.detail
        );

        // A daemon of the simulator's own: only its commands ask it.
        daemon_file(healthy_daemon(1), Some(&routes));
        assert!(
            Daemon::find(&global(&["--routes", routes_s]), false)
                .unwrap()
                .is_some()
        );
        assert!(Daemon::find(&global(&[]), false).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
