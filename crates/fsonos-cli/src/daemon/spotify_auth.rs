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

/// `surface` running the Spotify sign-in when serve has a Spotify app
/// client id, with its token cache in `data_dir`.
pub fn install(surface: Surface, args: &ServeArgs, data_dir: &Path) -> Surface {
    let surface = surface.with_spotify_redirect(args.spotify_redirect_uri.clone());
    let Some(client_id) = args.spotify_client_id.clone() else {
        return surface;
    };
    let config = SpotifyConfig {
        client_id,
        redirect_uri: args.spotify_redirect_uri.clone(),
    };
    match SpotifySignIn::new(config, data_dir.to_owned()) {
        Some(sign_in) => surface.with_spotify_auth(Box::new(sign_in)),
        None => surface,
    }
}

/// See the module docs.
pub struct SpotifySignIn {
    config: SpotifyConfig,
    data_dir: PathBuf,
    pending: Mutex<PendingSignIn>,
}

/// Keep the pending flow visible while exchanging it. A competing begin or
/// callback must not replace it until the exchange has finished.
#[derive(Default)]
struct PendingSignIn {
    authorization: Option<Arc<PendingAuthorization>>,
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
    #[must_use]
    pub fn new(config: SpotifyConfig, data_dir: PathBuf) -> Option<Self> {
        if let Err(e) = config.validate() {
            tracing::warn!("Spotify sign-in off: {e}");
            return None;
        }
        Some(Self {
            config,
            data_dir,
            pending: Mutex::new(PendingSignIn::default()),
        })
    }

    /// Run `work` with an open session, on a thread with its own runtime.
    fn with_session<R: Send + 'static>(
        &self,
        work: impl AsyncFnOnce(&mut Session, &Cx) -> Result<R, SpotifyError> + Send + 'static,
    ) -> Result<R, Failure> {
        let (config, cache) = (self.config.clone(), TokenCache::in_data_dir(&self.data_dir));
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

    fn claim(&self) -> Result<(Arc<PendingAuthorization>, Completing<'_>), Failure> {
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
        SignInDto {
            signed_in: TokenCache::in_data_dir(&self.data_dir)
                .load()
                .is_ok_and(|token| token.is_some()),
            pending: self.pending().authorization.is_some(),
            redirect_uri: self.config.redirect_uri.clone(),
        }
    }

    fn begin(&self) -> Result<SignInStartDto, Failure> {
        self.pending().idle()?;
        let authorization = self.with_session(async |session, _| session.begin_authorization())?;
        let started = SignInStartDto {
            url: authorization.url().to_owned(),
            redirect_uri: self.config.redirect_uri.clone(),
        };
        let mut pending = self.pending();
        // A callback could have claimed the previous flow while this one
        // was being generated. Recheck before replacing it.
        pending.idle()?;
        pending.authorization = Some(Arc::new(authorization));
        Ok(started)
    }

    fn complete(&self, redirect: &str) -> Result<(), Failure> {
        let (pending, mut completion) = self.claim()?;
        let redirect = redirect.to_owned();
        let result = self.with_session(async move |session, cx| {
            session
                .complete_authorization(cx, &pending, &redirect)
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
        auth.config.client_id = "invalid client id!".into();
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
}
