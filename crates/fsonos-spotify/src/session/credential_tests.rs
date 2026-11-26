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

#[test]
fn a_rotating_http_refresh_preserves_the_library_authorization_receipt() {
    let spotify = FakeSpotify::start();
    let data_dir = scratch_dir("receipt-refresh");
    let cache = TokenCache::in_data_dir(&data_dir);
    cache
        .store(&CachedToken {
            access_token: "expired-access".into(),
            refresh_token: "refresh-1".into(),
            expires_at: unix_now() - 3600,
            scope: SCOPE.into(),
        })
        .unwrap();
    let authorization = cache.authorization_id().unwrap();
    "refresh-1".clone_into(&mut spotify.state.lock().unwrap().refresh);
    let endpoints = spotify.endpoints();
    runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache.clone(), Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(endpoints);
        session.check_grant(&cx).await.unwrap();
        assert_eq!(cache.load().unwrap().unwrap().refresh_token, "refresh-r1");
        assert_eq!(cache.authorization_id().unwrap(), authorization);
        assert_eq!(session.authorization_id(), authorization.as_deref());
        session.check_grant(&cx).await.unwrap();
    });
    assert_eq!(spotify.stop().refreshes, 1);
    std::fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn sibling_sessions_share_a_rotating_grant_without_reauthorizing() {
    let spotify = FakeSpotify::start();
    let data_dir = scratch_dir("receipt-sibling-refresh");
    let cache = TokenCache::in_data_dir(&data_dir);
    cache
        .store(&CachedToken {
            access_token: "expired-access".into(),
            refresh_token: "refresh-1".into(),
            expires_at: unix_now() - 3600,
            scope: SCOPE.into(),
        })
        .unwrap();
    let authorization = cache.authorization_id().unwrap();
    "refresh-1".clone_into(&mut spotify.state.lock().unwrap().refresh);
    let endpoints = spotify.endpoints();
    runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let open = || {
            Session::open(config(), cache.clone(), Client::default_for_runtime(&cx))
                .unwrap()
                .with_endpoints(endpoints.clone())
        };
        let mut reader = open();
        let mut sibling = open();
        sibling.check_grant(&cx).await.unwrap();
        let rotated = cache.load().unwrap().unwrap();
        assert_eq!(rotated.refresh_token, "refresh-r1");

        reader.check_grant(&cx).await.unwrap();
        assert_eq!(reader.token.as_ref(), Some(&rotated));
        assert_eq!(reader.authorization_id(), authorization.as_deref());
        assert_eq!(cache.authorization_id().unwrap(), authorization);
        assert_eq!(cache.load().unwrap(), Some(rotated));
    });
    let fake = spotify.stop();
    assert_eq!(fake.refreshes, 1, "the second reader reuses the rotation");
    assert_eq!(fake.log.len(), 3, "one token POST and two library checks");
    std::fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn identical_tokens_from_new_authorization_still_retire_an_existing_session() {
    let spotify = FakeSpotify::start();
    let data_dir = scratch_dir("receipt-identical-reauthorization");
    let cache = TokenCache::in_data_dir(&data_dir);
    let token = new_sign_in();
    cache.store(&token).unwrap();
    let endpoints = spotify.endpoints();
    runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache.clone(), Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(endpoints);
        let retired = session.authorization_id().unwrap().to_owned();
        cache.store(&token).unwrap();
        assert_ne!(
            cache.authorization_id().unwrap().as_deref(),
            Some(retired.as_str())
        );
        let error = session.check_grant(&cx).await.unwrap_err();
        assert!(matches!(error, SpotifyError::Auth(_)), "{error}");
    });
    assert!(
        spotify.stop().log.is_empty(),
        "retired sessions must stop before HTTP"
    );
    std::fs::remove_dir_all(data_dir).unwrap();
}

#[test]
fn completing_pkce_again_publishes_a_new_receipt_even_with_the_same_token_strings() {
    use crate::fake_spotify::{REDIRECT, query_param};

    let spotify = FakeSpotify::start();
    let data_dir = scratch_dir("receipt-pkce-reauthorization");
    let cache = TokenCache::in_data_dir(&data_dir);
    let endpoints = spotify.endpoints();
    let state = Arc::clone(&spotify.state);
    runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let mut session = Session::open(config(), cache.clone(), Client::default_for_runtime(&cx))
            .unwrap()
            .with_endpoints(endpoints.clone());
        let mut previous = None;
        let mut retired_reader = None;
        for _ in 0..2 {
            let pending = session.begin_authorization().unwrap();
            state.lock().unwrap().expected_challenge =
                Some(query_param(pending.url(), "code_challenge"));
            let callback = format!(
                "{REDIRECT}?code=good-code&state={}",
                query_param(pending.url(), "state")
            );
            session
                .complete_authorization(&cx, &pending, &callback)
                .await
                .unwrap();
            let receipt = cache.authorization_id().unwrap().unwrap();
            assert_eq!(session.authorization_id(), Some(receipt.as_str()));
            if let Some(previous) = previous {
                assert_ne!(receipt, previous);
            } else {
                retired_reader = Some(
                    Session::open(config(), cache.clone(), Client::default_for_runtime(&cx))
                        .unwrap()
                        .with_endpoints(endpoints.clone()),
                );
            }
            previous = Some(receipt);
            let token = cache.load().unwrap().unwrap();
            assert_eq!(
                (token.access_token.as_str(), token.refresh_token.as_str()),
                ("access-1", "refresh-1")
            );
        }
        let error = retired_reader.unwrap().access_token(&cx).await.unwrap_err();
        assert!(matches!(error, SpotifyError::Auth(_)), "{error}");
    });
    let fake = spotify.stop();
    assert_eq!(
        fake.log.len(),
        2,
        "only the two requested code exchanges reach Spotify"
    );
    assert!(fake.log.iter().all(|entry| entry.ends_with("/api/token")));
    std::fs::remove_dir_all(data_dir).unwrap();
}
