use super::*;
use crate::client::SCOPE;
use crate::fake_spotify::{FakeSpotify, config, runtime, scratch_dir};
use std::sync::Arc;

fn new_sign_in() -> CachedToken {
    CachedToken {
        access_token: "new-owner-access".into(),
        refresh_token: "new-owner-refresh".into(),
        expires_at: unix_now() + 3600,
        scope: SCOPE.into(),
    }
}

/// The fake token endpoint executes the cache change before delivering its
/// successful response. That interleaving needs no timing sleeps and also
/// proves no token-cache lock is held over the HTTP request.
fn stale_refresh_keeps_current_cache(
    name: &str,
    replacement: Option<CachedToken>,
    during_request: bool,
) {
    let spotify = FakeSpotify::start();
    let data_dir = scratch_dir(name);
    let cache = TokenCache::in_data_dir(&data_dir);
    cache
        .store(&CachedToken {
            access_token: "expired-access".into(),
            refresh_token: "refresh-1".into(),
            expires_at: unix_now() - 3600,
            scope: SCOPE.into(),
        })
        .unwrap();
    "refresh-1".clone_into(&mut spotify.state.lock().unwrap().refresh);
    let endpoints = spotify.endpoints();
    let state = Arc::clone(&spotify.state);

    runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache.clone(), Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(endpoints);
        let changed_cache = cache.clone();
        let expected = replacement.clone();
        let change_cache = move || match replacement {
            Some(token) => changed_cache.store(&token).unwrap(),
            None => std::fs::remove_file(changed_cache.path()).unwrap(),
        };
        if during_request {
            state.lock().unwrap().before_refresh_response = Some(Box::new(change_cache));
        } else {
            change_cache();
        }

        let error = session.check_grant(&cx).await.unwrap_err();
        assert!(matches!(error, SpotifyError::Auth(_)), "{error}");
        assert_eq!(
            error.to_string(),
            "auth error: Spotify sign-in changed or was removed; retry with a new session"
        );
        assert_eq!(cache.load().unwrap(), expected);
    });

    let fake = spotify.stop();
    assert_eq!(fake.refreshes, usize::from(during_request));
    assert_eq!(fake.log.len(), usize::from(during_request));
    assert!(fake.log.iter().all(|entry| entry.ends_with("/api/token")));
    std::fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn a_new_sign_in_stops_an_old_session_before_refresh() {
    stale_refresh_keeps_current_cache("grant-replaced-before", Some(new_sign_in()), false);
}

#[test]
fn a_removed_grant_stops_an_old_session_before_refresh() {
    stale_refresh_keeps_current_cache("grant-removed-before", None, false);
}

#[test]
fn a_refresh_in_flight_cannot_overwrite_a_new_sign_in() {
    stale_refresh_keeps_current_cache("grant-replaced-during", Some(new_sign_in()), true);
}

#[test]
fn a_refresh_in_flight_cannot_recreate_a_removed_grant() {
    stale_refresh_keeps_current_cache("grant-removed-during", None, true);
}
