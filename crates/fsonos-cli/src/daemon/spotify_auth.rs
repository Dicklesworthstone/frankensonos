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
use std::sync::{Mutex, PoisonError};

use crate::config::ServeArgs;

/// `surface` running the Spotify sign-in when serve has a Spotify app
/// client id, with its token cache in `data_dir`.
pub fn install(surface: Surface, args: &ServeArgs, data_dir: &Path) -> Surface {
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
    pending: Mutex<Option<PendingAuthorization>>,
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
            pending: Mutex::new(None),
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

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<PendingAuthorization>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
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
            pending: self.pending().is_some(),
            redirect_uri: self.config.redirect_uri.clone(),
        }
    }

    fn begin(&self) -> Result<SignInStartDto, Failure> {
        let pending = self.with_session(async |session, _| session.begin_authorization())?;
        let started = SignInStartDto {
            url: pending.url().to_owned(),
            redirect_uri: self.config.redirect_uri.clone(),
        };
        *self.pending() = Some(pending);
        Ok(started)
    }

    fn complete(&self, redirect: &str) -> Result<(), Failure> {
        let Some(pending) = self.pending().take() else {
            return Err(Failure::invalid("no Spotify sign-in is waiting")
                .with_hint("Start one: fsonos setup (or POST /auth/spotify/begin)."));
        };
        let redirect = redirect.to_owned();
        let (result, pending) = self.with_session(async move |session, cx| {
            let result = session
                .complete_authorization(cx, &pending, &redirect)
                .await;
            Ok((result, pending))
        })?;
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                // A forged or stale redirect leaves the real one able to
                // finish.
                *self.pending() = Some(pending);
                Err(failure(e))
            }
        }
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
}
