#![forbid(unsafe_code)]

#[path = "support/spotify.rs"]
mod support;
use fsonos_spotify::fake_spotify::CLIENT_ID;
use support::*;

fn prepare(h: &Harness) {
    let mut fake = h.fake.as_ref().unwrap().state.lock().unwrap();
    fake.expected_challenge = Some(CHALLENGE.into());
    fake.expected_redirect = Some(REDIRECT.into());
}
fn exchange(h: &Harness, body: &Value) -> (u16, Value) {
    let (status, text, _) =
        h.request_body("POST", "/auth/spotify/exchange", &[], &body.to_string());
    (status, serde_json::from_str(&text).unwrap())
}
fn valid() -> Value {
    json!({"code":"good-code", "code_verifier":VERIFIER, "redirect_uri":REDIRECT})
}
fn assert_failure(h: &Harness, body: &Value, status: u16, code: &str) {
    let (got, error) = exchange(h, body);
    assert_eq!(got, status, "{error}");
    assert_eq!(error["code"], code);
    assert!(TokenCache::in_data_dir(&h.dir).load().unwrap().is_none());
    assert_eq!(h.json("GET", "/spotify/status").1["signed_in"], false);
    h.no_secrets_in(&["good-code", VERIFIER]);
}

/// A store holding the previous account's synced library and browse cache.
fn previous_account() -> SqliteStore {
    use fsonos_core::store::{LibraryEntry, LibraryOrigin, SpotifyAlbum, SpotifyCache, Store};
    let mut store = SqliteStore::open_in_memory().unwrap();
    let track = fsonos_types::Track {
        title: "Goldberg Variations".into(),
        artist: Some("Johann Sebastian Bach".into()),
        album: Some("Bach: Goldberg".into()),
        source_uri: "spotify:track:PreviousTrack000001".into(),
        uri: None,
        duration_secs: Some(120),
    };
    store
        .upsert_library(&[LibraryEntry {
            track,
            origin: LibraryOrigin::SavedAlbum,
            album_uri: Some("spotify:album:PreviousAlbum000001".into()),
            ..LibraryEntry::default()
        }])
        .unwrap();
    store
        .save_spotify_cache(&SpotifyCache {
            albums: vec![SpotifyAlbum {
                id: "PreviousAlbum000001".into(),
                title: "Bach: Goldberg".into(),
                artist: "Johann Sebastian Bach".into(),
                year: Some(1982),
                tracks: 1,
                uri: "spotify:album:PreviousAlbum000001".into(),
                art_url: None,
                saved: true,
            }],
            liked_uris: Vec::new(),
            track_uris: vec!["spotify:track:PreviousTrack000001".into()],
            synced_at: Some(42),
        })
        .unwrap();
    store
}

#[test]
fn e1_status_and_unconfigured_exchange() {
    let h = Harness::start(
        "exchange-unconfigured",
        false,
        &Identity::fixed(Client::LoopbackHttp),
        Policy::default(),
        false,
    );
    let status = h.json("GET", "/spotify/status").1;
    assert!(status.get("client_id").is_none());
    assert_eq!(status["app_redirect_uri"], REDIRECT);
    assert_failure(&h, &valid(), 503, "SPOTIFY_NOT_CONFIGURED");
    assert!(
        h.fake
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .log
            .is_empty()
    );
}
#[test]
fn e2_e3_invalid_inputs_never_reach_spotify_or_echo_secrets() {
    let h = Harness::local("exchange-validation");
    prepare(&h);
    for redirect in [
        "http://127.0.0.1:8099/auth/spotify/callback",
        "frankensonos://other",
        "frankensonos://spotify-callback/",
    ] {
        let mut body = valid();
        body["redirect_uri"] = redirect.into();
        assert_failure(&h, &body, 400, "SPOTIFY_REDIRECT_NOT_ALLOWED");
    }
    for verifier in [
        "a".repeat(42),
        "a".repeat(129),
        format!("{}!", "a".repeat(42)),
        "é".repeat(43),
        format!("{} ", "a".repeat(43)),
    ] {
        let mut body = valid();
        body["code_verifier"] = verifier.clone().into();
        assert_failure(&h, &body, 400, "INVALID_ARGUMENT");
        h.no_secrets_in(&[&verifier]);
    }
    for code in [String::new(), "secret-code".repeat(52)] {
        let mut body = valid();
        body["code"] = code.clone().into();
        assert_failure(&h, &body, 400, "INVALID_ARGUMENT");
        if !code.is_empty() {
            h.no_secrets_in(&[&code]);
        }
    }
    for body in [
        json!({}),
        json!({"code":VERIFIER,"code_verifier":42,"redirect_uri":REDIRECT}),
        json!({"code":["good-code"],"code_verifier":VERIFIER,"redirect_uri":REDIRECT}),
    ] {
        assert_failure(&h, &body, 400, "INVALID_ARGUMENT");
    }
    let (status, _, _) = h.request_body(
        "POST",
        "/auth/spotify/exchange",
        &[],
        &format!("{{\"code\":\"good-code\",\"code_verifier\":\"{VERIFIER}\","),
    );
    assert_eq!(status, 400);
    assert!(
        h.fake
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .log
            .is_empty()
    );
    h.no_secrets_in(&["good-code", VERIFIER]);
}
#[test]
fn e4_pkce_and_redirect_are_verified_by_fake_and_errors_are_safe() {
    let h = Harness::local("exchange-grant");
    prepare(&h);
    for body in [
        json!({"code":"expired-code","code_verifier":VERIFIER,"redirect_uri":REDIRECT}),
        json!({"code":"good-code","code_verifier":"a".repeat(43),"redirect_uri":REDIRECT}),
    ] {
        let (status, error) = exchange(&h, &body);
        assert_eq!(status, 400);
        assert!(error["detail"].as_str().unwrap().contains("invalid_grant"));
    }
    h.fake
        .as_ref()
        .unwrap()
        .state
        .lock()
        .unwrap()
        .expected_redirect = Some("frankensonos://different".into());
    assert_eq!(exchange(&h, &valid()).0, 400);
    h.fake.as_ref().unwrap().state.lock().unwrap().token_error = Some((
        400,
        format!(
            "{{\"error\":\"invalid_grant\",\"error_description\":\"good-code {VERIFIER} access-1 refresh-1\"}}"
        ),
    ));
    assert_eq!(exchange(&h, &valid()).0, 400);
    assert!(TokenCache::in_data_dir(&h.dir).load().unwrap().is_none());
    assert_eq!(h.json("GET", "/spotify/status").1["signed_in"], false);
    h.no_secrets_in(&["good-code", "expired-code", VERIFIER]);
}
#[test]
fn e5_upstream_and_network_failures_are_retryable() {
    let mut h = Harness::local("exchange-upstream");
    prepare(&h);
    h.fake.as_ref().unwrap().state.lock().unwrap().token_error = Some((
        503,
        format!("{{\"error\":\"server_error\",\"error_description\":\"{VERIFIER}\"}}"),
    ));
    let (status, body) = exchange(&h, &valid());
    assert_eq!(status, 502);
    assert_eq!(body["retryable"], true);
    h.fake.take().unwrap().stop();
    let (status, body) = exchange(&h, &valid());
    assert_eq!(status, 502);
    assert_eq!(body["retryable"], true);
    assert!(TokenCache::in_data_dir(&h.dir).load().unwrap().is_none());
    h.no_secrets_in(&["good-code", VERIFIER]);
}
#[test]
fn e6_a_second_exchange_while_one_runs_is_busy() {
    let h = Harness::local("exchange-busy");
    prepare(&h);
    h.fake.as_ref().unwrap().state.lock().unwrap().token_delay = Duration::from_millis(800);
    let worker = {
        let base = h.base.clone();
        thread::spawn(move || {
            runtime().block_on(async {
                let cx = Cx::current().unwrap();
                HttpClient::builder()
                    .no_redirects()
                    .build()
                    .post(format!("{base}/auth/spotify/exchange"))
                    .header("content-type", "application/json")
                    .body(valid().to_string())
                    .send(&cx)
                    .await
                    .unwrap()
            })
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while h
        .fake
        .as_ref()
        .unwrap()
        .state
        .lock()
        .unwrap()
        .log
        .is_empty()
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(exchange(&h, &valid()).0, 409);
    let response = worker.join().unwrap();
    assert_eq!(response.status, 200);
    h.bodies
        .lock()
        .unwrap()
        .push(String::from_utf8(response.body).unwrap());
    h.no_secrets_in(&["good-code", VERIFIER]);
}
#[test]
fn e7_cache_failure_names_path_and_stays_signed_out() {
    let h = Harness::start(
        "exchange-cache",
        true,
        &Identity::fixed(Client::LoopbackHttp),
        Policy::default(),
        true,
    );
    prepare(&h);
    let (status, body) = exchange(&h, &valid());
    assert_eq!(status, 500);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains(TokenCache::in_data_dir(&h.dir).path().to_str().unwrap())
    );
    assert_eq!(h.json("GET", "/spotify/status").1["signed_in"], false);
    assert!(TokenCache::in_data_dir(&h.dir).load().unwrap().is_none());
    h.no_secrets_in(&["good-code", VERIFIER]);
}
#[test]
fn e8_e9_allowed_remote_exchange_replaces_token_and_clears_both_caches() {
    let h = Harness::start_surface(
        "exchange-switch",
        true,
        &Identity::fixed(Client::LoopbackHttp),
        false,
        |spotify| {
            Surface::new(
                Box::new(NoLan),
                Box::new(|_| Ok(Vec::new())),
                Policy::default(),
                Box::new(SystemClock),
            )
            .with_action_log(Box::new(previous_account()), "test")
            .with_spotify(spotify)
        },
    );
    TokenCache::in_data_dir(&h.dir)
        .store(&fsonos_spotify::client::CachedToken {
            access_token: "previous-access".into(),
            refresh_token: "previous-refresh".into(),
            expires_at: i64::MAX,
            scope: String::new(),
        })
        .unwrap();
    assert!(
        h.json("GET", "/spotify/albums").1["total"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        !h.json("GET", "/library/search?q=Bach")
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    prepare(&h);
    assert_eq!(exchange(&h, &valid()), (200, json!({"signed_in":true})));
    assert_eq!(
        TokenCache::in_data_dir(&h.dir)
            .load()
            .unwrap()
            .unwrap()
            .access_token,
        "access-1"
    );
    for path in ["/spotify/albums", "/spotify/tracks"] {
        assert_eq!(h.json("GET", path).1, json!({"total":0,"items":[]}));
    }
    assert_eq!(h.json("GET", "/library/search?q=Bach").1, json!([]));
    let status = h.json("GET", "/spotify/status").1;
    assert_eq!(status["library"]["synced_at"], Value::Null);
    assert_eq!(status["client_id"], CLIENT_ID);
    h.no_secrets_in(&["good-code", VERIFIER, "previous-access"]);
    let policy = Policy::from_toml(
        "[clients.unknown]\nallow = [\"spotify_exchange\",\"spotify_status\",\"recent_actions\"]",
    )
    .unwrap();
    let h = Harness::start(
        "exchange-remote",
        true,
        &Identity::fixed(Client::Unknown),
        policy,
        false,
    );
    prepare(&h);
    assert_eq!(exchange(&h, &valid()), (200, json!({"signed_in":true})));
    h.no_secrets_in(&["good-code", VERIFIER]);
}
#[test]
fn e10_policy_denies_exchange_without_upstream_request() {
    let policy =
        Policy::from_toml("[clients.unknown]\nallow = [\"spotify_status\",\"recent_actions\"]")
            .unwrap();
    let h = Harness::start(
        "exchange-policy",
        true,
        &Identity::fixed(Client::Unknown),
        policy,
        false,
    );
    let (status, body) = exchange(&h, &valid());
    assert_eq!(status, 403);
    assert_eq!(body["code"], "POLICY_DENIED");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("spotify_exchange")
    );
    assert!(
        h.fake
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .log
            .is_empty()
    );
    h.no_secrets_in(&["good-code", VERIFIER]);
}
