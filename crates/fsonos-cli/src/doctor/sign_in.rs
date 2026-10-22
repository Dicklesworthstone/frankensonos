//! `dj.sign_in`: whether the owner's Spotify sign-in still works, so serve
//! can refresh the DJ's library. With a Spotify app configured and a grant
//! cached, it reads one liked track with the cached token, refreshing the
//! token first when it is about to expire, as serve would (the refreshed
//! token is cached for serve). It sends nothing else and never changes
//! playback.

use asupersync::Cx;
use asupersync::http::Client;
use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult};
use fsonos_spotify::SpotifyError;
use fsonos_spotify::client::{SpotifyConfig, TokenCache};
use fsonos_spotify::session::{GrantCheck, Session};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::config::ServeArgs;

pub const SIGN_IN: CheckId = CheckId("dj.sign_in");

/// The longest the check waits for Spotify.
const WAIT: Duration = Duration::from_secs(15);

/// Room for the Spotify client's futures.
const STACK: usize = 8 * 1024 * 1024;

/// Reads the grant through Spotify (tests stand in for it).
pub type ReadGrant =
    Arc<dyn Fn(SpotifyConfig, PathBuf) -> Result<GrantCheck, SpotifyError> + Send + Sync>;

/// See the module docs.
pub struct SignInCheck {
    data_dir: PathBuf,
    /// serve's Spotify app; `None` when neither `FSONOS_SPOTIFY_CLIENT_ID`
    /// nor the data directory's `spotify.toml` names one.
    config: Option<SpotifyConfig>,
    /// The redirect URI a new app must register.
    redirect_uri: String,
    read: ReadGrant,
}

impl SignInCheck {
    /// The check for `serve`'s Spotify app and `data_dir`'s token cache.
    #[must_use]
    pub fn new(serve: &ServeArgs, data_dir: &Path) -> Self {
        Self::with_read(serve, data_dir, Arc::new(read_grant))
    }

    fn with_read(serve: &ServeArgs, data_dir: &Path, read: ReadGrant) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            config: serve
                .spotify_app_id(data_dir)
                .map(|client_id| SpotifyConfig {
                    client_id,
                    redirect_uri: serve.spotify_redirect_uri.clone(),
                }),
            redirect_uri: serve.spotify_redirect_uri.clone(),
            read,
        }
    }
}

/// One liked track, through the session serve's library refresh uses.
fn read_grant(config: SpotifyConfig, data_dir: PathBuf) -> Result<GrantCheck, SpotifyError> {
    let runtime = crate::runtime().map_err(|e| SpotifyError::Http(format!("{e:#}")))?;
    runtime.block_on(async move {
        let cx = Cx::current().ok_or_else(|| SpotifyError::Http("no async context".to_owned()))?;
        let mut session = Session::open(
            config,
            TokenCache::in_data_dir(&data_dir),
            Client::default_for_runtime(&cx),
        )?;
        session.check_grant(&cx).await
    })
}

impl Check for SignInCheck {
    fn id(&self) -> CheckId {
        SIGN_IN
    }

    fn title(&self) -> &'static str {
        "Spotify sign-in"
    }

    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let signed_in = match TokenCache::in_data_dir(&self.data_dir).load() {
            Ok(token) => token.is_some(),
            Err(e) => {
                return CheckResult::warn(
                    "could not read the Spotify token cache",
                    "Check the data directory, or re-run `fsonos setup`.",
                )
                .with_detail(e.to_string());
            }
        };
        let Some(config) = self.config.clone() else {
            return if signed_in {
                CheckResult::warn(
                    "a Spotify sign-in is cached, but no Spotify app is set: serve can't \
                     refresh the DJ's library with it",
                    "Set FSONOS_SPOTIFY_CLIENT_ID to the app you signed in with, for serve \
                     and doctor alike (docs/DEPLOY.md).",
                )
            } else {
                // Without Spotify the DJ has nothing to play: a warning, not
                // a skip, so a house whose DJ cannot start never reads healthy.
                CheckResult::warn(
                    "Spotify isn't set up: the DJ has no library to play from",
                    format!(
                        "Create an app at {} with the redirect URI {}, then run `fsonos \
                         setup`: it saves the app's client id and signs you in.",
                        crate::spotify_app::DASHBOARD,
                        self.redirect_uri
                    ),
                )
            };
        };
        if !signed_in {
            return CheckResult::warn(
                "not signed in to Spotify: the DJ has no library to play from",
                "Run `fsonos setup` to sign in.",
            );
        }
        let wait = ctx.remaining().min(WAIT);
        let (tx, rx) = mpsc::channel();
        let (read, dir) = (Arc::clone(&self.read), self.data_dir.clone());
        let started = thread::Builder::new()
            .name("fsonos-doctor-spotify".into())
            .stack_size(STACK)
            .spawn(move || {
                let _ = tx.send(read(config, dir));
            });
        if let Err(e) = started {
            return CheckResult::warn(
                "could not check the Spotify sign-in",
                "Run the doctor again.",
            )
            .with_detail(e.to_string());
        }
        grant_result(rx.recv_timeout(wait).ok(), wait)
    }
}

/// The verdict on a grant read; `None`: Spotify didn't answer within
/// `wait`.
fn grant_result(read: Option<Result<GrantCheck, SpotifyError>>, wait: Duration) -> CheckResult {
    let sign_in_again = "Sign in again with `fsonos setup`. Until then the DJ plays only the \
                         library it has cached.";
    let offline = "Check this host's internet connection, then run the doctor again.";
    match read {
        None => CheckResult::warn(
            format!(
                "Spotify didn't answer within {} s; the sign-in wasn't checked",
                wait.as_secs()
            ),
            offline,
        ),
        Some(Ok(grant)) => CheckResult::pass(if grant.refreshed {
            "signed in to Spotify: the grant works (its token was due and is refreshed)"
        } else {
            "signed in to Spotify: the grant works"
        })
        .with_evidence(json!({
            "refreshed": grant.refreshed,
            "expires_in_secs": grant.expires_in_secs,
        })),
        Some(Err(
            SpotifyError::Auth(why)
            | SpotifyError::Api {
                status: 401,
                body: why,
            },
        )) => CheckResult::fail(
            "the Spotify sign-in no longer works (revoked or lapsed): serve can't refresh \
                 the DJ's library",
            sign_in_again,
        )
        .with_detail(why),
        Some(Err(SpotifyError::Api { status: 403, body })) => CheckResult::fail(
            "Spotify refuses this app the owner's library (403)",
            "In the Spotify developer dashboard, add your account under the app's User \
             Management (an app in development mode serves only the users listed there), \
             then sign in again with `fsonos setup`.",
        )
        .with_detail(body),
        Some(Err(SpotifyError::Api { status: 429, .. })) => CheckResult::warn(
            "Spotify is rate-limiting this app; the sign-in wasn't checked",
            "Run the doctor again in a few minutes.",
        ),
        Some(Err(SpotifyError::Http(why))) => {
            CheckResult::warn("could not reach Spotify to check the sign-in", offline)
                .with_detail(why)
        }
        Some(Err(e)) => CheckResult::warn(
            "could not check the Spotify sign-in",
            "If the DJ's library refresh keeps failing too, sign in again with `fsonos setup`.",
        )
        .with_detail(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::{Runner, Status};
    use fsonos_spotify::client::CachedToken;
    use std::sync::Mutex;

    fn serve(client_id: Option<&str>) -> ServeArgs {
        ServeArgs {
            http: Some("127.0.0.1:0".parse().unwrap()),
            mcp_http: Some("127.0.0.1:0".parse().unwrap()),
            spotify_client_id: client_id.map(str::to_owned),
            spotify_redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
            events_port: 0,
            allow_unsafe_bind: false,
            tailscale: crate::config::TailscaleMode::Auto,
            tailscale_serve: false,
        }
    }

    fn run_one(check: SignInCheck) -> CheckResult {
        let mut runner = Runner::new();
        runner.register(check);
        runner.run().unwrap().get(SIGN_IN).cloned().unwrap()
    }

    #[test]
    fn the_grant_is_read_only_with_an_app_and_a_cached_sign_in() {
        let dir =
            std::env::temp_dir().join(format!("fsonos-doctor-sign-in-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reads = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&reads);
        let read: ReadGrant = Arc::new(move |config: SpotifyConfig, dir: PathBuf| {
            seen.lock().unwrap().push((config.client_id, dir));
            Ok(GrantCheck {
                refreshed: true,
                expires_in_secs: 3600,
            })
        });
        let check = |client_id| SignInCheck::with_read(&serve(client_id), &dir, Arc::clone(&read));

        let unset = run_one(check(None));
        assert_eq!(unset.status, Status::Warn, "{unset:?}");
        let remedy = unset.remedy.unwrap();
        assert!(
            remedy.contains(crate::spotify_app::DASHBOARD)
                && remedy.contains("http://127.0.0.1:8099/auth/spotify/callback"),
            "{remedy}"
        );
        let signed_out = run_one(check(Some("app-1")));
        assert_eq!(signed_out.status, Status::Warn);
        assert!(signed_out.remedy.unwrap().contains("fsonos setup"));
        assert!(reads.lock().unwrap().is_empty(), "nothing to read yet");

        TokenCache::in_data_dir(&dir)
            .store(&CachedToken {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: 0,
                scope: "user-library-read".into(),
            })
            .unwrap();
        let no_app = run_one(check(None));
        assert_eq!(no_app.status, Status::Warn);
        assert!(no_app.remedy.unwrap().contains("FSONOS_SPOTIFY_CLIENT_ID"));
        assert!(reads.lock().unwrap().is_empty(), "no app to read with");

        let works = run_one(check(Some("app-1")));
        assert_eq!(works.status, Status::Pass, "{works:?}");
        assert_eq!(works.evidence["refreshed"], true);
        assert_eq!(*reads.lock().unwrap(), [("app-1".to_owned(), dir.clone())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The app `fsonos setup` saved in the data directory is the one the
    /// check reads with when no option or variable names one.
    #[test]
    fn the_app_setup_saved_is_read_with() {
        let dir = std::env::temp_dir().join(format!(
            "fsonos-doctor-sign-in-saved-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let app = "0123456789abcdef0123456789abcdef";
        crate::spotify_app::save(&dir, app).unwrap();
        TokenCache::in_data_dir(&dir)
            .store(&CachedToken {
                access_token: "access-1".into(),
                refresh_token: "refresh-1".into(),
                expires_at: 0,
                scope: "user-library-read".into(),
            })
            .unwrap();
        let reads = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&reads);
        let read: ReadGrant = Arc::new(move |config: SpotifyConfig, _: PathBuf| {
            seen.lock().unwrap().push(config.client_id);
            Ok(GrantCheck {
                refreshed: false,
                expires_in_secs: 3600,
            })
        });
        let works = run_one(SignInCheck::with_read(&serve(None), &dir, read));
        assert_eq!(works.status, Status::Pass, "{works:?}");
        assert_eq!(*reads.lock().unwrap(), [app.to_owned()]);
    }

    #[test]
    fn a_revoked_or_refused_grant_fails_and_an_unreachable_spotify_warns() {
        let wait = Duration::from_secs(15);
        let verdict = |read| grant_result(read, wait);
        let fresh = verdict(Some(Ok(GrantCheck {
            refreshed: false,
            expires_in_secs: 1800,
        })));
        assert_eq!(fresh.status, Status::Pass);
        assert!(!fresh.summary.contains("refreshed"));

        let revoked = verdict(Some(Err(SpotifyError::Auth(
            "token endpoint 400: invalid_grant (Invalid refresh token)".into(),
        ))));
        assert_eq!(revoked.status, Status::Fail);
        assert!(revoked.remedy.unwrap().contains("fsonos setup"));
        assert!(revoked.detail.unwrap().contains("invalid_grant"));
        let rejected = verdict(Some(Err(SpotifyError::Api {
            status: 401,
            body: "The access token expired".into(),
        })));
        assert_eq!(rejected.status, Status::Fail);

        let unlisted = verdict(Some(Err(SpotifyError::Api {
            status: 403,
            body: "User not registered in the Developer Dashboard".into(),
        })));
        assert_eq!(unlisted.status, Status::Fail);
        assert!(unlisted.remedy.unwrap().contains("User Management"));

        for (read, says) in [
            (None, "didn't answer within 15 s"),
            (
                Some(Err(SpotifyError::Http("connection refused".into()))),
                "could not reach",
            ),
            (
                Some(Err(SpotifyError::Api {
                    status: 429,
                    body: String::new(),
                })),
                "rate-limiting",
            ),
            (
                Some(Err(SpotifyError::Decode("unexpected body".into()))),
                "could not check",
            ),
        ] {
            let r = verdict(read);
            assert_eq!(r.status, Status::Warn, "{r:?}");
            assert!(r.summary.contains(says), "{r:?}");
        }
    }
}
