//! Keeping the DJ's library fresh: a refresh reads the owner's Spotify
//! library, taste signals and artist genres into the library cache
//! (fsonos-spotify's `sync_library`), the way `fsonos setup` first did.
//!
//! `fsonos serve` refreshes when it starts, if its last refresh is more
//! than a day old (or it never made one, as after an upgrade whose cache
//! lacks the newer columns), and checks again every [`CHECK`]: a day after
//! a success, or at the next check after a failure (a rate limit resumes
//! where it stopped). Signed out, it waits quietly; `fsonos doctor` says so.
//! Those days and checks run on the daemon's clock, the one its schedules
//! use (a test moves it with serve's `--clock-file`).
//! `fsonos dj sync` (`POST /dj/sync`, the `dj_sync` tool) asks for one now;
//! `fsonos dj sync --status` (`GET /dj/sync`, `dj_sync_status`) only says
//! where they stand.
//!
//! A refresh opens its own connection to the store, so the DJ and the
//! other surfaces never wait on its Spotify reads, and the DJ rebuilds its
//! pool at its next pick once a refresh has landed. The last success is
//! kept in [`STAMP`] in the data directory.

use asupersync::Cx;
use asupersync::http::Client;
use clap::Args;
use fsonos_api::surface::dj_sync::{LibrarySyncDto, SyncedDto};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::store::SqliteStore;
use fsonos_spotify::SpotifyError;
use fsonos_spotify::cache::sync_library_detailed;
use fsonos_spotify::client::{SpotifyConfig, TokenCache};
use fsonos_spotify::session::Session;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use crate::config::{GlobalArgs, ServeArgs};
use crate::remote::Daemon;

/// The last successful refresh, in the data directory.
pub const STAMP: &str = "library-sync.json";

/// How old the last refresh may get before the daemon refreshes again.
pub const MAX_AGE: Duration = Duration::from_hours(24);

/// How often the daemon checks whether a refresh is due.
pub const CHECK: Duration = Duration::from_mins(15);

/// Room for the Spotify client's and the store's futures.
const STACK: usize = 8 * 1024 * 1024;

/// The library refresh for one data directory; see the module docs.
pub struct LibrarySync {
    config: SpotifyConfig,
    data_dir: PathBuf,
    state: Mutex<Running>,
    /// A refresh landed since the DJ last rebuilt its pool.
    fresh: AtomicBool,
    /// When a refresh happens and is due (the system clock by default).
    clock: Box<dyn Clock>,
}

#[derive(Default)]
struct Running {
    running: bool,
    error: Option<String>,
}

impl LibrarySync {
    /// A refresh through the Spotify app `config`, with the token cache and
    /// the store in `data_dir`; `None` when the config cannot work (the
    /// reason is logged).
    #[must_use]
    pub fn new(config: SpotifyConfig, data_dir: PathBuf) -> Option<Self> {
        if let Err(e) = config.validate() {
            tracing::warn!("Spotify library refresh off: {e}");
            return None;
        }
        Some(Self {
            config,
            data_dir,
            state: Mutex::new(Running::default()),
            fresh: AtomicBool::new(false),
            clock: Box::new(SystemClock),
        })
    }

    /// The same refresh on `clock`.
    #[must_use]
    pub fn with_clock(self, clock: Box<dyn Clock>) -> Self {
        Self { clock, ..self }
    }

    /// Now on its clock, in Unix seconds.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.clock.now().timestamp()
    }

    /// Seconds on its clock's count that never steps back (see
    /// [`Clock::elapsed_secs`]): the host's monotonic time, or a test's.
    fn elapsed_secs(&self) -> i64 {
        self.clock.elapsed_secs()
    }

    /// The daemon's, on its `clock`: when `serve` names a Spotify app (it
    /// says so when not).
    #[must_use]
    pub fn for_serve(
        serve: &ServeArgs,
        data_dir: &Path,
        clock: Box<dyn Clock>,
    ) -> Option<Arc<Self>> {
        let Some(client_id) = serve.spotify_client_id.clone() else {
            tracing::info!(
                "the DJ's library is not refreshed: no Spotify app client id \
                 (FSONOS_SPOTIFY_CLIENT_ID)"
            );
            return None;
        };
        let config = SpotifyConfig {
            client_id,
            redirect_uri: serve.spotify_redirect_uri.clone(),
        };
        Self::new(config, data_dir.to_owned()).map(|sync| Arc::new(sync.with_clock(clock)))
    }

    fn running(&self) -> std::sync::MutexGuard<'_, Running> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where it stands.
    #[must_use]
    pub fn status(&self) -> LibrarySyncDto {
        let (running, error) = {
            let state = self.running();
            (state.running, state.error.clone())
        };
        LibrarySyncDto::new(running, self.last(), error)
    }

    /// Claim the one refresh slot. Admission happens before a worker is
    /// spawned, so concurrent callers cannot both schedule work.
    fn claim(&self) -> bool {
        let mut state = self.running();
        if state.running {
            return false;
        }
        state.running = true;
        state.error = None;
        true
    }

    /// Start a refresh on a thread of its own, unless one is running;
    /// where it stands.
    pub fn start(self: &Arc<Self>) -> Result<LibrarySyncDto, Failure> {
        if !self.signed_in() {
            return Err(signed_out());
        }
        if !self.claim() {
            return Ok(self.status());
        }
        let sync = Arc::clone(self);
        if let Err(e) = thread::Builder::new()
            .name("fsonos-library-sync".into())
            .stack_size(STACK)
            .spawn(move || {
                if let Err(f) = sync.run_claimed() {
                    tracing::warn!("library refresh: {}", f.detail);
                }
            })
        {
            let failure = Failure::new(ErrorCode::Internal, format!("start the refresh: {e}"));
            let mut state = self.running();
            state.running = false;
            state.error = Some(failure.detail.clone());
            return Err(failure);
        }
        Ok(self.status())
    }

    /// Refresh now, on this thread (which must not be in a runtime);
    /// what it found. A second caller while one runs is told to wait.
    pub fn run(&self) -> Result<SyncedDto, Failure> {
        if !self.claim() {
            return Err(
                Failure::new(ErrorCode::NotReady, "a library refresh is already running")
                    .with_hint("fsonos dj sync (GET /dj/sync) shows when it is done."),
            );
        }
        self.run_claimed()
    }

    /// Perform a refresh whose slot was already claimed.
    fn run_claimed(&self) -> Result<SyncedDto, Failure> {
        let result = self.read();
        let mut state = self.running();
        state.running = false;
        state.error = result.as_ref().err().map(|f| f.detail.clone());
        drop(state);
        if let Ok(synced) = &result {
            self.fresh.store(true, Ordering::Release);
            if let Err(e) = write_stamp(&self.data_dir, synced) {
                tracing::warn!("record the library refresh: {e}");
            }
        }
        result
    }

    /// Read Spotify into a store of its own.
    fn read(&self) -> Result<SyncedDto, Failure> {
        let (config, dir) = (self.config.clone(), self.data_dir.clone());
        let synced = thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || {
                let runtime = crate::runtime().map_err(|e| SpotifyError::Http(format!("{e:#}")))?;
                runtime.block_on(async move {
                    let cx = Cx::current()
                        .ok_or_else(|| SpotifyError::Http("no async context".to_owned()))?;
                    let mut session = Session::open(
                        config,
                        TokenCache::in_data_dir(&dir),
                        Client::default_for_runtime(&cx),
                    )?;
                    if !session.is_authorized() {
                        return Err(SpotifyError::Auth("not signed in to Spotify".into()));
                    }
                    let mut store = SqliteStore::open(&dir.join(crate::daemon::DB_FILE))?;
                    sync_library_detailed(&mut session, &cx, &mut store).await
                })
            })
            .map_err(|e| Failure::new(ErrorCode::Internal, format!("start the refresh: {e}")))?
            .join()
            .map_err(|_| Failure::new(ErrorCode::Internal, "the library refresh panicked"))?
            .map_err(failure)?;
        let pending = synced
            .genres
            .deferred
            .saturating_add(synced.genres.failed.len());
        Ok(SyncedDto {
            at: self.now(),
            tracks: synced.library.tracks,
            candidates: synced.library.candidates,
            classical: synced.library.classical,
            retired: synced.library.retired,
            genre_artists_pending: pending,
            notes: synced.notes,
        })
    }

    /// Whether a Spotify sign-in is cached (no network).
    #[must_use]
    pub fn signed_in(&self) -> bool {
        TokenCache::in_data_dir(&self.data_dir)
            .load()
            .is_ok_and(|token| token.is_some())
    }

    /// The last successful refresh, from [`STAMP`].
    #[must_use]
    pub fn last(&self) -> Option<SyncedDto> {
        last_synced(&self.data_dir)
    }

    /// Whether a refresh is due at `now` (Unix seconds): never made, or
    /// older than [`MAX_AGE`].
    #[must_use]
    pub fn due(&self, now: i64) -> bool {
        let max = i64::try_from(MAX_AGE.as_secs()).unwrap_or(i64::MAX);
        self.last()
            .is_none_or(|last| last.genre_artists_pending > 0 || now.saturating_sub(last.at) >= max)
    }

    /// Whether a refresh landed since the last call (the DJ then rebuilds
    /// its pool).
    pub fn take_fresh(&self) -> bool {
        self.fresh.swap(false, Ordering::AcqRel)
    }
}

/// The daemon's schedule: refresh when due and signed in, checking again
/// [`CHECK`] after each check ends, timed on `sync`'s clock (looked at every
/// half second; a wall-clock step does not move it), on a thread of its own
/// until `stop`. Nothing without `sync`.
pub fn watch(sync: Option<Arc<LibrarySync>>, stop: Arc<AtomicBool>) {
    let Some(sync) = sync else {
        return;
    };
    let every = i64::try_from(CHECK.as_secs()).unwrap_or(i64::MAX);
    let spawned = thread::Builder::new()
        .name("fsonos-library-watch".into())
        .stack_size(STACK)
        .spawn(move || {
            let mut next = i64::MIN;
            while !stop.load(Ordering::Acquire) {
                if sync.elapsed_secs() >= next {
                    if sync.signed_in() && sync.due(sync.now()) {
                        match sync.run() {
                            Ok(synced) => eprintln!("fsonos serve: {}", synced.text()),
                            Err(f) => tracing::warn!("library refresh: {}", f.detail),
                        }
                    }
                    // From the end of a refresh, however long it took (a
                    // rate limit can hold one for an hour).
                    next = sync.elapsed_secs().saturating_add(every);
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("not refreshing the library: {e}");
    }
}

/// The last successful refresh `data_dir`'s [`STAMP`] holds.
fn last_synced(data_dir: &Path) -> Option<SyncedDto> {
    let text = std::fs::read_to_string(data_dir.join(STAMP)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_stamp(data_dir: &Path, synced: &SyncedDto) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(synced).map_err(std::io::Error::other)?;
    let tmp = data_dir.join(format!("{STAMP}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, data_dir.join(STAMP))
}

fn signed_out() -> Failure {
    Failure::new(
        ErrorCode::SpotifyAuthRequired,
        "the daemon is not signed in to Spotify",
    )
    .with_hint("Sign in once with fsonos setup; the daemon then refreshes the library daily.")
}

/// A refresh failure, coded: signed out is the owner's to fix; a rate limit
/// or an outage passes, and the next check resumes.
fn failure(err: SpotifyError) -> Failure {
    match err {
        SpotifyError::Auth(_) => signed_out(),
        SpotifyError::Api { status: 429, .. } => Failure::new(
            ErrorCode::Internal,
            "Spotify is rate-limiting the library read; the next refresh resumes",
        ),
        other => Failure::new(ErrorCode::Internal, format!("the library refresh: {other}")),
    }
}

/// `fsonos dj sync`.
#[derive(Args, Debug, Clone)]
pub struct SyncArgs {
    /// Only say where the library refresh stands (when the last one finished
    /// and what it found, whether one runs now); start none.
    #[arg(long, conflicts_with = "no_wait")]
    pub status: bool,
    /// Return once the refresh has started, rather than when it is done.
    #[arg(long)]
    pub no_wait: bool,
    /// Without a daemon: the Spotify app client id to refresh with.
    #[arg(long, env = "FSONOS_SPOTIFY_CLIENT_ID")]
    pub spotify_client_id: Option<String>,
    /// Without a daemon: the redirect URI registered with that app.
    #[arg(
        long,
        env = "FSONOS_SPOTIFY_REDIRECT_URI",
        default_value = "http://127.0.0.1:8099/auth/spotify/callback"
    )]
    pub spotify_redirect_uri: String,
}

/// How often `fsonos dj sync` asks the daemon whether it is done.
const POLL: Duration = Duration::from_secs(2);

/// `fsonos dj sync`: through the daemon when one answers (it refreshes in
/// the background; this waits unless `--no-wait`), else right here.
pub fn run(global: &GlobalArgs, args: &SyncArgs) -> anyhow::Result<()> {
    if args.status {
        let state: LibrarySyncDto = match Daemon::find(global, global.daemon)? {
            Some(daemon) => daemon.get("/dj/sync")?,
            // Without a daemon nothing refreshes in the background.
            None => {
                LibrarySyncDto::new(false, last_synced(&crate::daemon::data_dir(global)?), None)
            }
        };
        return crate::emit(global.json, &state, |s| format!("{}\n", s.done));
    }
    if let Some(daemon) = Daemon::find(global, global.daemon)? {
        let mut state: LibrarySyncDto = daemon.post("/dj/sync", &serde_json::json!({}))?;
        while state.running && !args.no_wait {
            thread::sleep(POLL);
            state = daemon.get("/dj/sync")?;
        }
        if let Some(error) = state.error.clone().filter(|_| !args.no_wait) {
            return Err(Failure::new(ErrorCode::Internal, error).into());
        }
        return crate::emit(global.json, &state, |s| format!("{}\n", s.done));
    }
    let Some(client_id) = args.spotify_client_id.clone() else {
        return Err(Failure::invalid(
            "no daemon answers, and no Spotify app client id is set (FSONOS_SPOTIFY_CLIENT_ID)",
        )
        .with_hint(
            "Run it with fsonos serve up (it refreshes daily on its own), or set \
                 FSONOS_SPOTIFY_CLIENT_ID.",
        )
        .into());
    };
    let config = SpotifyConfig {
        client_id,
        redirect_uri: args.spotify_redirect_uri.clone(),
    };
    let sync = LibrarySync::new(config, crate::daemon::data_dir(global)?)
        .ok_or_else(|| Failure::invalid("the Spotify app client id is not a valid id"))?;
    if !sync.signed_in() {
        return Err(signed_out().into());
    }
    let synced = sync.run()?;
    let state = LibrarySyncDto::new(false, Some(synced), None);
    crate::emit(global.json, &state, |s| format!("{}\n", s.done))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(name: &str) -> LibrarySync {
        let dir = std::env::temp_dir().join(format!(
            "fsonos-library-sync-{}-{name}-{}",
            std::process::id(),
            SystemClock.now().timestamp()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        LibrarySync::new(
            SpotifyConfig {
                client_id: "0123456789abcdef0123456789abcdef".into(),
                redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
            },
            dir,
        )
        .expect("a valid config")
    }

    fn synced(at: i64) -> SyncedDto {
        SyncedDto {
            at,
            tracks: 3,
            candidates: 2,
            classical: 1,
            retired: 0,
            genre_artists_pending: 0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn due_when_never_made_or_a_day_old() {
        let sync = sync("due");
        let day = i64::try_from(MAX_AGE.as_secs()).unwrap();
        assert!(sync.due(1_000_000), "never refreshed");
        write_stamp(&sync.data_dir, &synced(1_000_000)).unwrap();
        assert_eq!(sync.last(), Some(synced(1_000_000)));
        assert!(!sync.due(1_000_000 + day - 1));
        assert!(sync.due(1_000_000 + day));

        let mut pending = synced(1_000_000 + day);
        pending.genre_artists_pending = 3;
        write_stamp(&sync.data_dir, &pending).unwrap();
        assert!(
            sync.due(1_000_001 + day),
            "unfinished genre enrichment retries at the next daemon check"
        );

        std::fs::write(
            sync.data_dir.join(STAMP),
            r#"{"at":1000000,"tracks":3,"candidates":2,"classical":1,"retired":0}"#,
        )
        .unwrap();
        assert!(!sync.due(1_000_000 + day - 1));

        // A stamp that cannot be read is a refresh never made.
        std::fs::write(sync.data_dir.join(STAMP), "not json").unwrap();
        assert!(sync.due(1_000_000));
    }

    #[test]
    fn signed_out_refreshes_nothing() {
        let sync = Arc::new(sync("signed-out"));
        assert!(!sync.signed_in());
        let refused = sync.start().unwrap_err();
        assert_eq!(refused.code, ErrorCode::SpotifyAuthRequired);
        // Run directly, the sign-in check fails before any request.
        let failed = sync.run().unwrap_err();
        assert_eq!(
            failed.code,
            ErrorCode::SpotifyAuthRequired,
            "{}",
            failed.detail
        );
        let status = sync.status();
        assert!(!status.running);
        assert!(status.done.contains("failed"), "{}", status.done);
        assert!(!sync.take_fresh(), "nothing landed");
        assert_eq!(sync.last(), None);
    }

    #[test]
    fn only_one_concurrent_caller_claims_the_refresh_slot() {
        let sync = Arc::new(sync("claim"));
        let start = Arc::new(std::sync::Barrier::new(16));
        // All 16 must be running before any is joined: the barrier waits
        // for every one of them.
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let sync = Arc::clone(&sync);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    usize::from(sync.claim())
                })
            })
            .collect();
        let claimed: usize = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum();
        assert_eq!(claimed, 1);
        assert!(sync.running().running);
    }

    #[test]
    fn a_bad_client_id_turns_the_refresh_off() {
        let config = SpotifyConfig {
            client_id: "not an id!".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        };
        assert!(LibrarySync::new(config, std::env::temp_dir()).is_none());
    }

    /// A DJ with no Spotify app has no library to refresh, and says how
    /// to give it one.
    #[test]
    fn without_an_app_the_dj_cannot_refresh() {
        use fsonos_api::dj::DjEngine;
        let dj = crate::dj::SpotifyDj::new(None);
        for answer in [dj.sync_library(), dj.library_sync()] {
            let f = answer.unwrap_err();
            assert_eq!(f.code, ErrorCode::NotImplemented, "{}", f.detail);
            assert!(f.hint.contains("FSONOS_SPOTIFY_CLIENT_ID"), "{}", f.hint);
        }
    }

    #[test]
    fn a_rate_limit_is_said_plainly() {
        let f = failure(SpotifyError::Api {
            status: 429,
            body: String::new(),
        });
        assert!(f.detail.contains("rate-limiting"), "{}", f.detail);
        assert_eq!(
            failure(SpotifyError::Auth("expired".into())).code,
            ErrorCode::SpotifyAuthRequired
        );
    }
}
