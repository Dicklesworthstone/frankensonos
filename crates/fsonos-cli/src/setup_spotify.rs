//! The Spotify sign-in step of `fsonos setup`: the owner's PKCE login through
//! fsonos-spotify's `Session` (scope `user-library-read`), then the first
//! library sync into the store, which the DJ plays from.
//!
//! The redirect URI (by default http://127.0.0.1:8099/auth/spotify/callback)
//! is on the daemon's HTTP port. When a daemon runs, the sign-in goes
//! through it (`POST /auth/spotify/begin`): it holds the flow, and the
//! browser comes back to its own callback route. With no daemon, setup
//! listens on the redirect address itself, and only when nothing else does,
//! so there are never two listeners on one port. Either way the owner can
//! paste the address the browser lands on, which is how the sign-in works
//! over SSH or with no browser on this host.
//! The flow's `state` is always checked. The refresh token goes only to the
//! local, git-ignored token cache in the data directory.
//!
//! Re-running is cheap: a cached sign-in with a library already in the store
//! passes without reading anything.

use asupersync::Cx;
use asupersync::http::Client;
use fsonos_core::doctor::Status;
use fsonos_core::store::{SqliteStore, Store as _};
use fsonos_spotify::cache::{LibrarySync, sync_library};
use fsonos_spotify::client::{SpotifyConfig, TokenCache};
use fsonos_spotify::session::Session;
use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::config::{GlobalArgs, ServeArgs};
use crate::remote::Daemon;
use crate::setup_cmd::{Step, StepOutcome};
use fsonos_api::surface::spotify_auth::{SignInDto, SignInStartDto};

/// How long the sign-in waits for the browser (or a paste).
const SIGN_IN_WAIT: Duration = Duration::from_mins(10);

/// Lines typed at the terminal, read by one thread for every prompt setup
/// makes, so a prompt never loses its answer to an earlier reader.
pub struct Prompt {
    lines: Mutex<Receiver<String>>,
}

impl Prompt {
    /// Start reading stdin.
    #[must_use]
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            lines: Mutex::new(rx),
        }
    }

    /// The next line within `wait`, trimmed; `None` at the end of input or
    /// on timeout.
    pub fn line(&self, wait: Duration) -> Option<String> {
        let lines = self.lines.lock().ok()?;
        lines.recv_timeout(wait).ok().map(|l| l.trim().to_owned())
    }

    /// The next line, if one is already waiting.
    fn ready(&self) -> Option<String> {
        let lines = self.lines.lock().ok()?;
        lines.try_recv().ok().map(|l| l.trim().to_owned())
    }

    /// Ask a yes/no question; `true` for yes.
    pub fn ask(&self, question: &str) -> bool {
        print!("{question} [y/N] ");
        let _ = std::io::stdout().flush();
        self.line(Duration::MAX)
            .is_some_and(|a| matches!(a.as_str(), "y" | "Y" | "yes"))
    }
}

/// How the sign-in went, before it is a step's verdict.
#[derive(Debug)]
pub enum SignIn {
    /// No Spotify app client id is configured.
    NoClientId,
    /// Not signed in, and no terminal to sign in on.
    NeedsTerminal,
    /// Signed in, with this many tracks already in the store.
    AlreadyIn {
        tracks: usize,
    },
    /// Signed in now (or before), and the library just read into the store.
    Synced(LibrarySync),
    /// Signed in through the daemon, but this run has no client id to read
    /// the library with.
    DaemonOnly,
    Failed(String),
}

/// The step's verdict for `result`.
#[must_use]
pub fn verdict(result: SignIn, redirect_uri: &str) -> StepOutcome {
    let (status, summary, remedy) = match result {
        SignIn::NoClientId => (
            Status::Warn,
            "no Spotify app client id is configured".to_owned(),
            Some(format!(
                "Create an app at https://developer.spotify.com/dashboard with the redirect URI \
                 {redirect_uri}, set FSONOS_SPOTIFY_CLIENT_ID (or pass --spotify-client-id), \
                 and run fsonos setup again."
            )),
        ),
        SignIn::NeedsTerminal => (
            Status::Warn,
            "not signed in to Spotify".to_owned(),
            Some(
                "Run fsonos setup on a terminal (over SSH works: it prints the address to open \
                 and takes the one your browser lands on)."
                    .to_owned(),
            ),
        ),
        SignIn::AlreadyIn { tracks } => (
            Status::Pass,
            format!("signed in; the store holds {tracks} library track(s)"),
            None,
        ),
        SignIn::Synced(sync) => (
            Status::Pass,
            format!(
                "signed in; read {} library track(s), {} of them classical (the DJ's pool)",
                sync.tracks, sync.classical
            ),
            None,
        ),
        SignIn::DaemonOnly => (
            Status::Warn,
            "signed in through the daemon, but setup has no app client id to read the library with"
                .to_owned(),
            Some(
                "Set FSONOS_SPOTIFY_CLIENT_ID here too (the daemon's), and run fsonos setup again."
                    .to_owned(),
            ),
        ),
        SignIn::Failed(why) => (
            Status::Fail,
            format!("the Spotify sign-in failed: {why}"),
            Some(
                "Check the client id and that the app lists the redirect URI exactly, then try \
                 again."
                    .to_owned(),
            ),
        ),
    };
    let mut outcome = StepOutcome::new(Step::SpotifyLogin, status, summary);
    outcome.remedies = remedy.into_iter().collect();
    outcome
}

/// Sign in (on a terminal, through `prompt`) and sync the library.
pub fn sign_in(global: &GlobalArgs, serve: &ServeArgs, prompt: Option<&Prompt>) -> SignIn {
    // A daemon that runs the sign-in holds the flow; the token it caches in
    // the shared data directory is the one read with below.
    if let Some((daemon, status)) = Daemon::find(global, false)
        .ok()
        .flatten()
        .and_then(|d| d.get::<SignInDto>("/auth/spotify").ok().map(|s| (d, s)))
    {
        if !status.signed_in {
            let Some(prompt) = prompt else {
                return SignIn::NeedsTerminal;
            };
            if let Err(why) = through_daemon(&daemon, prompt) {
                return SignIn::Failed(why);
            }
        }
        if serve.spotify_client_id.is_none() {
            return SignIn::DaemonOnly;
        }
    }
    let Some(client_id) = serve.spotify_client_id.clone() else {
        return SignIn::NoClientId;
    };
    let data_dir = match crate::daemon::data_dir(global) {
        Ok(dir) => dir,
        Err(f) => return SignIn::Failed(f.detail),
    };
    let config = SpotifyConfig {
        client_id,
        redirect_uri: serve.spotify_redirect_uri.clone(),
    };
    let runtime = match crate::runtime() {
        Ok(rt) => rt,
        Err(e) => return SignIn::Failed(format!("{e:#}")),
    };
    runtime.block_on(async move {
        let Some(cx) = Cx::current() else {
            return SignIn::Failed("no async context".to_owned());
        };
        let redirect = config.redirect_uri.clone();
        let mut session = match Session::open(
            config,
            TokenCache::in_data_dir(&data_dir),
            Client::default_for_runtime(&cx),
        ) {
            Ok(session) => session,
            Err(e) => return SignIn::Failed(e.to_string()),
        };
        let mut store = match SqliteStore::open(&data_dir.join(crate::daemon::DB_FILE)) {
            Ok(store) => store,
            Err(e) => return SignIn::Failed(format!("the store: {e}")),
        };
        if session.is_authorized() {
            let tracks = store.library().map_or(0, |l| l.len());
            if tracks > 0 {
                return SignIn::AlreadyIn { tracks };
            }
        } else {
            let Some(prompt) = prompt else {
                return SignIn::NeedsTerminal;
            };
            let pending = match session.begin_authorization() {
                Ok(pending) => pending,
                Err(e) => return SignIn::Failed(e.to_string()),
            };
            let landed = match redirect_landing(&redirect, pending.url(), prompt) {
                Ok(landed) => landed,
                Err(why) => return SignIn::Failed(why),
            };
            if let Err(e) = session.complete_authorization(&cx, &pending, &landed).await {
                return SignIn::Failed(e.to_string());
            }
        }
        match sync_library(&mut session, &cx, &mut store).await {
            Ok(sync) => SignIn::Synced(sync),
            Err(e) => SignIn::Failed(format!("signed in, but the library read failed: {e}")),
        }
    })
}

/// Sign in through `daemon`: it starts the flow and the browser comes back
/// to it, or the owner pastes the address the browser landed on.
fn through_daemon(daemon: &Daemon, prompt: &Prompt) -> Result<(), String> {
    let started: SignInStartDto = daemon
        .post("/auth/spotify/begin", &serde_json::json!({}))
        .map_err(|f| f.detail)?;
    println!(
        "Open this address in a browser and allow access:\n  {}\nThe daemon waits for the \
         browser to come back to {} (or paste the address it lands on here, then Enter):",
        started.url, started.redirect_uri
    );
    let _ = std::io::stdout().flush();
    let deadline = Instant::now() + SIGN_IN_WAIT;
    while Instant::now() < deadline {
        if let Some(line) = prompt.ready()
            && !line.is_empty()
        {
            let done: SignInDto = daemon
                .post(
                    "/auth/spotify/complete",
                    &serde_json::json!({ "redirect": line }),
                )
                .map_err(|f| f.detail)?;
            if done.signed_in {
                return Ok(());
            }
        }
        if daemon
            .get::<SignInDto>("/auth/spotify")
            .is_ok_and(|s| s.signed_in)
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err("the browser did not come back within 10 minutes".to_owned())
}

/// The address the browser lands on after the owner allows access: caught
/// on the redirect address when setup may listen there, or pasted.
fn redirect_landing(redirect_uri: &str, url: &str, prompt: &Prompt) -> Result<String, String> {
    println!("Open this address in a browser and allow access:\n  {url}");
    let listener = redirect_addr(redirect_uri)
        .filter(|addr| TcpStream::connect_timeout(addr, Duration::from_millis(300)).is_err())
        .and_then(|addr| TcpListener::bind(addr).ok());
    match &listener {
        Some(_) => println!(
            "Waiting for the browser to come back to {redirect_uri} (or paste the address it \
             lands on here, then Enter):"
        ),
        None => println!(
            "Then paste the address your browser lands on (it starts with {redirect_uri}) here, \
             then Enter:"
        ),
    }
    let _ = std::io::stdout().flush();
    if let Some(listener) = &listener {
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("the redirect listener: {e}"))?;
    }
    let deadline = Instant::now() + SIGN_IN_WAIT;
    while Instant::now() < deadline {
        if let Some(line) = prompt.ready() {
            if line.is_empty() {
                continue;
            }
            return Ok(line);
        }
        if let Some(target) = listener
            .as_ref()
            .and_then(|l| l.accept().ok())
            .and_then(|(stream, _)| answer_redirect(stream))
        {
            return Ok(target);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err("no redirect arrived within 10 minutes".to_owned())
}

/// Read the browser's redirect, answer it, and return its request target
/// (`/auth/spotify/callback?code=…&state=…`); `None` for anything else.
fn answer_redirect(mut stream: TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = [0_u8; 8192];
    let n = stream.read(&mut buf).ok()?;
    let target = request_target(&buf[..n])?;
    let body = "<!doctype html><title>FrankenSonos</title><p>Signed in to FrankenSonos. You \
                can close this tab and go back to the terminal.</p>";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    Some(target)
}

/// The target of a `GET` that carries the flow's `state`.
#[must_use]
pub fn request_target(raw: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(raw).ok()?.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    target.contains("state=").then(|| target.to_owned())
}

/// The loopback address setup may listen on for `uri`
/// (`http://127.0.0.1:<port>/…` or `http://[::1]:<port>/…`); `None` for
/// HTTPS, another host, or no port.
#[must_use]
pub fn redirect_addr(uri: &str) -> Option<SocketAddr> {
    let authority = uri.strip_prefix("http://")?.split('/').next()?;
    authority
        .parse::<SocketAddr>()
        .ok()
        .filter(|addr| addr.ip().is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_listens_only_on_a_loopback_redirect_with_a_port() {
        assert_eq!(
            redirect_addr("http://127.0.0.1:8099/auth/spotify/callback"),
            Some("127.0.0.1:8099".parse().unwrap())
        );
        assert_eq!(
            redirect_addr("http://[::1]:8099/auth/spotify/callback"),
            Some("[::1]:8099".parse().unwrap())
        );
        for other in [
            "https://example.invalid/auth/spotify/callback",
            "http://127.0.0.1/auth/spotify/callback",
            "http://192.0.2.10:8099/auth/spotify/callback",
        ] {
            assert_eq!(redirect_addr(other), None, "{other}");
        }
    }

    #[test]
    fn only_a_redirect_with_state_is_taken() {
        let redirect =
            b"GET /auth/spotify/callback?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(
            request_target(redirect).as_deref(),
            Some("/auth/spotify/callback?code=abc&state=xyz")
        );
        assert_eq!(request_target(b"GET /favicon.ico HTTP/1.1\r\n\r\n"), None);
        assert_eq!(
            request_target(b"POST /auth/spotify/callback?state=x HTTP/1.1\r\n\r\n"),
            None
        );
        assert_eq!(request_target(b"\xff\xfe"), None);
    }

    #[test]
    fn each_sign_in_result_is_a_verdict_with_its_fix() {
        let uri = "http://127.0.0.1:8099/auth/spotify/callback";
        let none = verdict(SignIn::NoClientId, uri);
        assert_eq!(none.status, Status::Warn);
        assert!(
            none.remedies[0].contains("FSONOS_SPOTIFY_CLIENT_ID") && none.remedies[0].contains(uri)
        );
        assert_eq!(verdict(SignIn::NeedsTerminal, uri).status, Status::Warn);
        let synced = verdict(
            SignIn::Synced(LibrarySync {
                tracks: 120,
                classical: 80,
                retired: 0,
            }),
            uri,
        );
        assert_eq!(synced.status, Status::Pass);
        assert!(
            synced
                .summary
                .contains("120 library track(s), 80 of them classical")
        );
        assert!(
            verdict(SignIn::AlreadyIn { tracks: 7 }, uri)
                .remedies
                .is_empty()
        );
        let daemon_only = verdict(SignIn::DaemonOnly, uri);
        assert_eq!(daemon_only.status, Status::Warn);
        assert!(daemon_only.remedies[0].contains("FSONOS_SPOTIFY_CLIENT_ID"));
        let failed = verdict(SignIn::Failed("invalid_grant".into()), uri);
        assert_eq!(failed.status, Status::Fail);
        assert!(failed.summary.contains("invalid_grant"));
    }
}
