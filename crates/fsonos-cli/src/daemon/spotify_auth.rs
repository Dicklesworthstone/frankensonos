//! The daemon's Spotify sign-in, as the surfaces' `SpotifyAuth`, over
//! fsonos-spotify's `Session`. It keeps the one sign-in that waits for the
//! browser in memory (its PKCE verifier and `state`), and finishes it with
//! the redirect. Each step runs on a thread with its own runtime, so the
//! token exchange never holds up the server's. The token goes only to the
//! local token cache in the data directory, where `fsonos setup` and every
//! later library read find it.

use asupersync::Cx;
use asupersync::http::Client;
use fsonos_api::Surface;
use fsonos_api::surface::spotify_auth::{SignInDto, SignInStartDto, SpotifyAuth};
use fsonos_api::{ErrorCode, Failure};
use fsonos_spotify::SpotifyError;
use fsonos_spotify::client::{SpotifyConfig, TokenCache};
use fsonos_spotify::session::{PendingAuthorization, Session};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::ServeArgs;

/// Install before setup too: a newly saved app activates the same sign-in
/// engine without replacing the daemon or its pending authorization.
pub fn install(surface: Surface, args: &ServeArgs, data_dir: &Path) -> Surface {
    let surface = crate::spotify_sources::install(surface, data_dir, Some(args));
    let surface = surface.with_spotify_redirect(args.spotify_redirect_uri.clone());
    surface.with_spotify_auth(Box::new(SpotifySignIn {
        settings: crate::spotify_app::Settings {
            client_id: args.spotify_client_id.clone(),
            redirect_uri: args.spotify_redirect_uri.clone(),
        },
        data_dir: data_dir.to_owned(),
        pending: Mutex::new(PendingSignIn::default()),
    }))
}

/// See the module docs.
pub struct SpotifySignIn {
    settings: crate::spotify_app::Settings,
    data_dir: PathBuf,
    pending: Mutex<PendingSignIn>,
}

/// Keep the pending flow visible while exchanging it. A competing begin or
/// callback must not replace it until the exchange has finished.
#[derive(Default)]
struct PendingSignIn {
    authorization: Option<Arc<Authorization>>,
    exchanging: bool,
}

impl PendingSignIn {
    fn idle(&self) -> Result<(), Failure> {
        if self.exchanging {
            return Err(
                Failure::new(ErrorCode::NotReady, "a Spotify sign-in is being completed")
                    .with_hint(
                        "Check sign-in status before starting or completing another sign-in.",
                    ),
            );
        }
        Ok(())
    }
}

/// The authorization code belongs to exactly the app and redirect used at begin.
struct Authorization {
    flow: PendingAuthorization,
    config: SpotifyConfig,
}

/// Always release the reservation, including session-open failures and
/// unwinding. No mutex is held while the worker opens its cache or uses HTTP.
struct Completing<'a> {
    owner: &'a SpotifySignIn,
    successful: bool,
}

impl Drop for Completing<'_> {
    fn drop(&mut self) {
        let mut pending = self.owner.pending();
        if self.successful {
            pending.authorization = None;
        }
        pending.exchanging = false;
    }
}

impl SpotifySignIn {
    /// A sign-in for the app `config` names, caching the token in
    /// `data_dir`; `None` when the config cannot work (the reason is
    /// logged).
    #[cfg(test)]
    #[must_use]
    pub fn new(config: SpotifyConfig, data_dir: PathBuf) -> Option<Self> {
        if let Err(e) = config.validate() {
            tracing::warn!("Spotify sign-in off: {e}");
            return None;
        }
        Some(Self {
            settings: config.into(),
            data_dir,
            pending: Mutex::new(PendingSignIn::default()),
        })
    }

    /// Run `work` with an open session, on a thread with its own runtime.
    fn with_session<R: Send + 'static>(
        &self,
        config: SpotifyConfig,
        work: impl AsyncFnOnce(&mut Session, &Cx) -> Result<R, SpotifyError> + Send + 'static,
    ) -> Result<R, Failure> {
        let cache = TokenCache::in_data_dir(&self.data_dir);
        std::thread::spawn(move || {
            let runtime = crate::runtime().map_err(|e| SpotifyError::Http(format!("{e:#}")))?;
            runtime.block_on(async move {
                let cx = Cx::current()
                    .ok_or_else(|| SpotifyError::Http("no async context".to_owned()))?;
                let mut session = Session::open(config, cache, Client::default_for_runtime(&cx))?;
                work(&mut session, &cx).await
            })
        })
        .join()
        .map_err(|_| Failure::new(ErrorCode::Internal, "the Spotify sign-in thread panicked"))?
        .map_err(failure)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, PendingSignIn> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn claim(&self) -> Result<(Arc<Authorization>, Completing<'_>), Failure> {
        let mut pending = self.pending();
        pending.idle()?;
        let authorization = pending.authorization.clone().ok_or_else(|| {
            Failure::invalid("no Spotify sign-in is waiting")
                .with_hint("Start one: fsonos setup (or POST /auth/spotify/begin).")
        })?;
        pending.exchanging = true;
        Ok((
            authorization,
            Completing {
                owner: self,
                successful: false,
            },
        ))
    }

    fn unchanged(&self, expected: &SpotifyConfig) -> Result<(), Failure> {
        let active = self.settings.resolve(&self.data_dir)?;
        if active.client_id != expected.client_id || active.redirect_uri != expected.redirect_uri {
            return Err(
                Failure::invalid("the Spotify app changed while sign-in was pending")
                    .with_hint("Restore the original app configuration or start a new sign-in."),
            );
        }
        Ok(())
    }
}

/// A sign-in failure, coded: a refused or mismatched redirect is the
/// owner's to retry; anything else is the daemon's.
fn failure(err: SpotifyError) -> Failure {
    match err {
        SpotifyError::Auth(why) => Failure::new(ErrorCode::SpotifyAuthRequired, why).with_hint(
            "Start the sign-in again (fsonos setup), and allow access on Spotify's page.",
        ),
        SpotifyError::Config(why) => Failure::invalid(why),
        other => Failure::new(ErrorCode::Internal, format!("Spotify sign-in: {other}")),
    }
}

impl SpotifyAuth for SpotifySignIn {
    fn status(&self) -> SignInDto {
        let active = self.settings.resolve(&self.data_dir).ok();
        let configured = active.is_some();
        let pending = self
            .pending()
            .authorization
            .as_ref()
            .is_some_and(|authorization| {
                active.as_ref().is_some_and(|config| {
                    config.client_id == authorization.config.client_id
                        && config.redirect_uri == authorization.config.redirect_uri
                })
            });
        SignInDto {
            configured,
            signed_in: configured
                && TokenCache::in_data_dir(&self.data_dir)
                    .load()
                    .is_ok_and(|token| token.is_some()),
            pending,
            redirect_uri: self.settings.redirect_uri.clone(),
        }
    }

    fn configure(&self, client_id: &str) -> Result<(), Failure> {
        let pending = self.pending();
        if pending
            .authorization
            .as_ref()
            .is_some_and(|authorization| authorization.config.client_id != client_id.trim())
        {
            return Err(
                Failure::invalid("a sign-in for a different Spotify app is still pending")
                    .with_hint(
                        "Restore the original app configuration before completing this sign-in.",
                    ),
            );
        }
        self.settings.configure(&self.data_dir, client_id)
    }

    fn begin(&self) -> Result<SignInStartDto, Failure> {
        self.pending().idle()?;
        let config = self.settings.resolve(&self.data_dir)?;
        let flow = self.with_session(config.clone(), async |session, _| {
            session.begin_authorization()
        })?;
        let started = SignInStartDto {
            url: flow.url().to_owned(),
            redirect_uri: config.redirect_uri.clone(),
        };
        let mut pending = self.pending();
        // A callback could have claimed the previous flow while this one
        // was being generated. Recheck before replacing it.
        pending.idle()?;
        self.unchanged(&config)?;
        pending.authorization = Some(Arc::new(Authorization { flow, config }));
        Ok(started)
    }

    fn complete(&self, redirect: &str) -> Result<(), Failure> {
        let (pending, mut completion) = self.claim()?;
        self.unchanged(&pending.config)?;
        let redirect = redirect.to_owned();
        let result = self.with_session(pending.config.clone(), async move |session, cx| {
            session
                .complete_authorization(cx, &pending.flow, &redirect)
                .await
        });
        // A forged/stale redirect or failed worker leaves the same pending
        // flow in place. It cannot overwrite a newer sign-in: begin is
        // refused for the entire reservation, including the cache write.
        completion.successful = result.is_ok();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign_in(name: &str) -> SpotifySignIn {
        let dir =
            std::env::temp_dir().join(format!("fsonos-sign-in-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        SpotifySignIn::new(
            SpotifyConfig {
                client_id: "0123456789abcdef0123456789abcdef".into(),
                redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
            },
            dir,
        )
        .expect("a valid config")
    }

    #[test]
    fn a_bad_config_turns_the_sign_in_off() {
        let config = SpotifyConfig {
            client_id: "not an id!".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        };
        assert!(SpotifySignIn::new(config, std::env::temp_dir()).is_none());
    }

    #[test]
    fn a_sign_in_waits_and_a_wrong_redirect_keeps_it_waiting() {
        let auth = sign_in("flow");
        let status = auth.status();
        assert!(!status.signed_in && !status.pending);
        let err = auth
            .complete("/auth/spotify/callback?code=c&state=s")
            .unwrap_err();
        assert!(
            err.detail.contains("no Spotify sign-in is waiting"),
            "{}",
            err.detail
        );

        let started = auth.begin().unwrap();
        assert!(
            started.url.contains("code_challenge_method=S256"),
            "{}",
            started.url
        );
        assert!(auth.status().pending);
        // The state check fails before any request is made, and the sign-in
        // still waits for the real redirect.
        let err = auth
            .complete("/auth/spotify/callback?code=c&state=forged")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::SpotifyAuthRequired, "{}", err.detail);
        assert!(auth.status().pending);
    }

    #[test]
    fn an_exchange_stays_pending_and_refuses_competing_writes() {
        let auth = sign_in("reserved");
        auth.begin().unwrap();
        let (flow, reservation) = auth.claim().unwrap();
        assert!(auth.status().pending);
        assert_eq!(auth.begin().unwrap_err().code, ErrorCode::NotReady);
        assert_eq!(
            auth.complete("?code=c&state=s").unwrap_err().code,
            ErrorCode::NotReady
        );
        assert!(Arc::ptr_eq(
            auth.pending().authorization.as_ref().unwrap(),
            &flow
        ));
        drop(reservation);
        assert!(auth.status().pending);
        assert!(!auth.pending().exchanging);
    }

    #[test]
    fn a_successful_exchange_consumes_the_flow_once() {
        let auth = sign_in("success");
        auth.begin().unwrap();
        let (_, mut reservation) = auth.claim().unwrap();
        reservation.successful = true;
        drop(reservation);
        assert!(!auth.status().pending);
        assert!(auth.claim().is_err());
        assert!(!auth.pending().exchanging);
        auth.begin().unwrap();
        assert!(auth.status().pending);
    }

    #[test]
    fn a_session_open_failure_keeps_the_original_flow() {
        let mut auth = sign_in("open-failure");
        auth.begin().unwrap();
        let flow = auth.pending().authorization.clone().unwrap();
        // Session::open rejects this before network I/O. Previously its
        // error bypassed restoration and silently discarded the pending flow.
        auth.settings.client_id = Some("invalid client id!".into());
        assert_eq!(
            auth.complete("?code=c&state=s").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert!(Arc::ptr_eq(
            auth.pending().authorization.as_ref().unwrap(),
            &flow
        ));
        assert!(!auth.pending().exchanging);
    }

    #[test]
    fn unwinding_releases_the_reservation_without_losing_the_flow() {
        let auth = sign_in("unwind");
        auth.begin().unwrap();
        let flow = auth.pending().authorization.clone().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (_, _reservation) = auth.claim().unwrap();
            panic!("synthetic completion failure");
        }));
        assert!(result.is_err());
        assert!(!auth.pending().exchanging);
        assert!(Arc::ptr_eq(
            auth.pending().authorization.as_ref().unwrap(),
            &flow
        ));
    }

    #[test]
    fn another_thread_can_read_status_while_an_exchange_is_reserved() {
        let auth = sign_in("concurrent");
        auth.begin().unwrap();
        let (_, reservation) = auth.claim().unwrap();
        std::thread::scope(|scope| {
            let (send, receive) = std::sync::mpsc::channel();
            let auth = &auth;
            scope.spawn(move || {
                let pending = auth.status().pending;
                let begin = auth.begin().unwrap_err().code;
                let complete = auth.complete("?code=c&state=s").unwrap_err().code;
                send.send((pending, begin, complete)).unwrap();
            });
            let answer = receive.recv_timeout(std::time::Duration::from_secs(5));
            // Release even on timeout so a regression cannot deadlock the
            // scope's join. This test must not hold a mutex across exchange.
            drop(reservation);
            assert_eq!(
                answer.unwrap(),
                (true, ErrorCode::NotReady, ErrorCode::NotReady)
            );
        });
    }
    fn unconfigured(data_dir: PathBuf) -> SpotifySignIn {
        SpotifySignIn {
            settings: crate::spotify_app::Settings {
                client_id: None,
                redirect_uri: crate::config::SPOTIFY_REDIRECT_URI.into(),
            },
            data_dir,
            pending: Mutex::new(PendingSignIn::default()),
        }
    }

    #[test]
    fn first_run_configuration_activates_the_existing_auth_engine() {
        let dir = tempfile::tempdir().unwrap();
        let auth = unconfigured(dir.path().to_path_buf());
        assert!(!auth.status().configured);
        assert!(!auth.status().signed_in);
        assert_eq!(
            auth.begin().unwrap_err().code,
            ErrorCode::SpotifyAuthRequired
        );

        let app = "0123456789abcdef0123456789abcdef";
        auth.configure(app).unwrap();
        assert!(auth.status().configured);
        let started = auth.begin().unwrap();
        assert!(started.url.contains(app));
        let flow = auth.pending().authorization.clone().unwrap();
        auth.configure(app).unwrap();
        assert!(auth.configure("fedcba9876543210fedcba9876543210").is_err());
        assert!(Arc::ptr_eq(
            auth.pending().authorization.as_ref().unwrap(),
            &flow
        ));
    }

    #[test]
    fn a_pending_flow_keeps_its_original_app_and_rejects_external_reconfiguration() {
        let dir = tempfile::tempdir().unwrap();
        let auth = unconfigured(dir.path().to_path_buf());
        let app = "0123456789abcdef0123456789abcdef";
        auth.configure(app).unwrap();
        auth.begin().unwrap();
        let flow = auth.pending().authorization.clone().unwrap();
        assert_eq!(flow.config.client_id, app);
        std::fs::write(
            dir.path().join(crate::spotify_app::FILE_NAME),
            "client_id = \"fedcba9876543210fedcba9876543210\"\n",
        )
        .unwrap();
        let error = auth.complete("?code=unused&state=unused").unwrap_err();
        assert!(error.detail.contains("app changed"));
        assert!(auth.status().configured);
        assert!(!auth.status().pending);
        assert!(!auth.pending().exchanging);
        assert!(Arc::ptr_eq(
            auth.pending().authorization.as_ref().unwrap(),
            &flow
        ));
        assert!(
            TokenCache::in_data_dir(dir.path())
                .load()
                .unwrap()
                .is_none()
        );

        let path = dir.path().join(crate::spotify_app::FILE_NAME);
        std::fs::rename(&path, path.with_extension("toml.saved")).unwrap();
        assert!(!auth.status().configured);
        assert!(!auth.status().pending);
        assert!(auth.configure("fedcba9876543210fedcba9876543210").is_err());
        assert!(
            !path.exists(),
            "a pending flow must not bind to a different app"
        );
        std::fs::write(&path, "not valid TOML = [").unwrap();
        assert!(!auth.status().configured);
        assert!(!auth.status().pending);
        std::fs::write(&path, format!("client_id = \"{app}\"\n")).unwrap();
        assert!(auth.status().configured);
        assert!(auth.status().pending);
        assert!(Arc::ptr_eq(
            auth.pending().authorization.as_ref().unwrap(),
            &flow
        ));
    }

    #[test]
    fn an_unconfigured_engine_does_not_advertise_an_orphaned_token_as_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        TokenCache::in_data_dir(dir.path())
            .store(&fsonos_spotify::client::CachedToken {
                access_token: "fixture-access".into(),
                refresh_token: "fixture-refresh".into(),
                expires_at: i64::MAX,
                scope: fsonos_spotify::client::SCOPE.into(),
            })
            .unwrap();
        let auth = unconfigured(dir.path().to_path_buf());
        assert!(!auth.status().configured);
        assert!(!auth.status().signed_in);
    }
}
