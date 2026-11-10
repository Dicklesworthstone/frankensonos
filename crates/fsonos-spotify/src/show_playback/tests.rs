use super::*;
use crate::client::{CachedToken, TokenCache};
use crate::fake_spotify::{FakeSpotify, config, runtime};
use asupersync::http::Client;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const SHOW: &str = "Show000000000000000001";
const OTHER: &str = "Show000000000000000002";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Server {
    fake: Option<FakeSpotify>,
    data: PathBuf,
}

impl Server {
    fn new() -> Self {
        let fake = FakeSpotify::start();
        let data = std::env::temp_dir().join(format!(
            "fsonos-show-playback-{}-{}-{}",
            std::process::id(),
            fake.addr.port(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        Self {
            fake: Some(fake),
            data,
        }
    }

    fn fake(&self) -> &FakeSpotify {
        self.fake.as_ref().unwrap()
    }

    fn session(&self, cx: &Cx, authorized: bool) -> Session {
        let cache = TokenCache::in_data_dir(&self.data);
        if authorized {
            cache
                .store(&CachedToken {
                    access_token: "show-test-access".into(),
                    refresh_token: "show-test-refresh".into(),
                    expires_at: i64::MAX / 2,
                    scope: "user-library-read".into(),
                })
                .unwrap();
            let mut state = self.fake().state.lock().unwrap();
            "show-test-access".clone_into(&mut state.access);
            "show-test-refresh".clone_into(&mut state.refresh);
        }
        Session::open(config(), cache, Client::default_for_runtime(cx))
            .unwrap()
            .with_endpoints(self.fake().endpoints())
    }

    fn set(&self, document: &Value) {
        self.fake()
            .state
            .lock()
            .unwrap()
            .library_pages
            .insert(format!("/v1/shows/{SHOW}"), document.to_string());
    }

    fn refuse(&self, status: u16, retry_after: Option<u32>) {
        self.fake()
            .state
            .lock()
            .unwrap()
            .api_errors
            .insert(format!("/v1/shows/{SHOW}"), (status, retry_after));
    }

    fn calls(&self) -> Vec<String> {
        self.fake().state.lock().unwrap().log.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(fake) = self.fake.take() {
            fake.stop();
        }
    }
}

fn episode_id(number: usize) -> String {
    format!("Episode{number:015}")
}

fn episode_uri(number: usize) -> String {
    format!("spotify:episode:{}", episode_id(number))
}

fn episode(number: usize) -> Value {
    json!({
        "id": episode_id(number), "uri": episode_uri(number), "type": "episode",
        "name": format!("Episode {number}"), "duration_ms": 1_800_001,
        "explicit": false, "is_playable": true, "is_externally_hosted": false,
    })
}

fn show_document(episodes: &[Value]) -> Value {
    json!({
        "id": SHOW, "uri": format!("spotify:show:{SHOW}"), "type": "show",
        "name": "Conversations & Sounds", "total_episodes": episodes.len() + 100,
        "episodes": {
            "items": episodes, "offset": 0, "limit": EPISODE_PAGE_LIMIT,
            "total": episodes.len() + 100,
            "next": "https://example.invalid/never-follow-this-page",
        },
    })
}

fn assert_malformed_document(document: &Value) {
    let server = Server::new();
    server.set(document);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        assert!(matches!(
            resolve(&mut session, &cx, SHOW).await,
            Err(SpotifyError::Decode(_))
        ));
    });
    assert_eq!(server.calls(), [format!("GET /v1/shows/{SHOW}")]);
}

#[test]
fn selection_preserves_provider_order_metadata_and_explicit_content() {
    let server = Server::new();
    let mut explicit = episode(3);
    explicit["explicit"] = true.into();
    explicit["is_externally_hosted"] = true.into();
    let mut unavailable = episode(4);
    unavailable["is_playable"] = false.into();
    unavailable["uri"] = "an unavailable row needs no valid identity".into();
    let mut restricted = episode(5);
    restricted["restrictions"] = json!({ "reason": "future-restriction" });
    server.set(&show_document(&[
        explicit.clone(),
        episode(1),
        explicit,
        unavailable,
        restricted,
        episode(2),
    ]));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let selected = resolve(&mut session, &cx, &format!("spotify:show:{SHOW}"))
            .await
            .unwrap();
        assert_eq!(selected.show_uri, format!("spotify:show:{SHOW}"));
        assert_eq!(selected.title, "Conversations & Sounds");
        assert_eq!(
            selected
                .episodes
                .iter()
                .map(|episode| episode.episode_uri.as_str())
                .collect::<Vec<_>>(),
            [episode_uri(3), episode_uri(1), episode_uri(2)]
        );
        let first = &selected.episodes[0];
        assert_eq!(first.title, "Episode 3");
        assert_eq!(first.duration_secs, 1801);
        assert!(
            first.explicit,
            "show playback does not apply DJ preferences"
        );
    });
    assert_eq!(server.calls(), [format!("GET /v1/shows/{SHOW}")]);
}

#[test]
fn selection_is_bounded_to_ten_episodes_and_one_catalog_page() {
    let server = Server::new();
    let episodes: Vec<_> = (0..EPISODE_PAGE_LIMIT as usize).map(episode).collect();
    server.set(&show_document(&episodes));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let selected = resolve(&mut session, &cx, SHOW).await.unwrap();
        assert_eq!(selected.episodes.len(), MAX_EPISODES);
        assert_eq!(selected.episodes[0].episode_uri, episode_uri(0));
        assert_eq!(selected.episodes[9].episode_uri, episode_uri(9));
    });
    assert_eq!(server.calls(), [format!("GET /v1/shows/{SHOW}")]);
}

#[test]
fn an_empty_or_unplayable_first_page_fails_without_chasing_next() {
    let mut unavailable = episode(1);
    unavailable["is_playable"] = false.into();
    for episodes in [vec![], vec![unavailable; EPISODE_PAGE_LIMIT as usize]] {
        assert_malformed_document(&show_document(&episodes));
    }
}

#[test]
fn invalid_input_fails_before_any_authorized_request() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        for value in [
            "",
            "short",
            "spotify:show:short",
            "spotify:track:Show000000000000000001",
            "Show00000000000000000/",
            "https://open.spotify.com/show/Show000000000000000001",
        ] {
            assert!(matches!(
                resolve(&mut session, &cx, value).await,
                Err(SpotifyError::Config(_))
            ));
        }
    });
    assert!(server.calls().is_empty());
}

#[test]
fn show_identity_must_match_the_requested_show_and_kind() {
    for variant in 0..6 {
        let mut show = show_document(&[episode(1)]);
        match variant {
            0 => show["id"] = "short".into(),
            1 => {
                show["id"] = OTHER.into();
                show["uri"] = format!("spotify:show:{OTHER}").into();
            }
            2 => show["uri"] = format!("spotify:show:{OTHER}").into(),
            3 => show["type"] = "episode".into(),
            4 => show["name"] = "  ".into(),
            _ => show["episodes"] = Value::Null,
        }
        assert_malformed_document(&show);
    }
}

#[test]
fn malformed_playable_episode_never_returns_the_earlier_valid_selection() {
    for variant in 0..9 {
        let mut malformed = episode(2);
        match variant {
            0 => malformed["id"] = "short".into(),
            1 => malformed["uri"] = episode_uri(3).into(),
            2 => malformed["type"] = "track".into(),
            3 => {
                malformed.as_object_mut().unwrap().remove("is_playable");
            }
            4 => {
                malformed.as_object_mut().unwrap().remove("explicit");
            }
            5 => malformed["name"] = "invalid\u{0}title".into(),
            6 => malformed["name"] = "".into(),
            7 => malformed["name"] = "x".repeat(4097).into(),
            _ => malformed["duration_ms"] = u64::MAX.into(),
        }
        assert_malformed_document(&show_document(&[episode(1), malformed]));
    }
}

#[test]
fn inconsistent_page_counters_and_oversized_pages_are_rejected() {
    for variant in 0..8 {
        let mut show = show_document(&[episode(1), episode(2)]);
        match variant {
            0 => show["episodes"]["offset"] = 1.into(),
            1 => show["episodes"]["total"] = 0.into(),
            2 => show["episodes"]["limit"] = 0.into(),
            3 => show["episodes"]["limit"] = (EPISODE_PAGE_LIMIT + 1).into(),
            4 => show["episodes"]["items"] = (0..51).map(episode).collect::<Vec<_>>().into(),
            5 => show["episodes"]["limit"] = 1.into(),
            6 => show["total_episodes"] = 0.into(),
            _ => {
                show["episodes"].as_object_mut().unwrap().remove("limit");
            }
        }
        assert_malformed_document(&show);
    }
}

#[test]
fn missing_authorization_stops_before_http() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, false);
        assert!(matches!(
            resolve(&mut session, &cx, SHOW).await,
            Err(SpotifyError::Auth(_))
        ));
    });
    assert!(server.calls().is_empty());
}

#[test]
fn catalog_refusals_propagate_without_inventing_a_fallback() {
    for status in [400, 403, 404] {
        let server = Server::new();
        server.refuse(status, None);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            assert!(matches!(
                resolve(&mut session, &cx, SHOW).await,
                Err(SpotifyError::Api { status: actual, .. }) if actual == status
            ));
        });
        assert_eq!(server.calls(), [format!("GET /v1/shows/{SHOW}")]);
    }
}

#[test]
fn a_long_rate_limit_remains_active_for_subsequent_resolution() {
    let server = Server::new();
    server.refuse(429, Some(3600));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        for _ in 0..2 {
            assert!(matches!(
                resolve(&mut session, &cx, SHOW).await,
                Err(SpotifyError::Api { status: 429, .. })
            ));
            assert!(session.retry_after_secs() > 3500);
        }
    });
    assert_eq!(server.calls(), [format!("GET /v1/shows/{SHOW}")]);
}

#[test]
fn expired_access_is_refreshed_by_the_existing_session_then_reads_the_show() {
    let server = Server::new();
    server.set(&show_document(&[episode(1)]));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        "expired-access".clone_into(&mut server.fake().state.lock().unwrap().access);
        let selected = resolve(&mut session, &cx, SHOW).await.unwrap();
        assert_eq!(selected.episodes[0].episode_uri, episode_uri(1));
    });
    assert_eq!(
        server.calls(),
        [
            format!("GET /v1/shows/{SHOW}"),
            "POST /api/token".into(),
            format!("GET /v1/shows/{SHOW}"),
        ]
    );
    assert_eq!(server.fake().state.lock().unwrap().refreshes, 1);
    let cached = TokenCache::in_data_dir(&server.data)
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(cached.access_token, "access-r1");
    assert_eq!(cached.refresh_token, "refresh-r1");
}

#[test]
fn transport_failure_is_preserved_without_other_catalog_requests() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut endpoints = server.fake().endpoints();
        endpoints.api = "http://[".into();
        let mut session = server.session(&cx, true).with_endpoints(endpoints);
        assert!(matches!(
            resolve(&mut session, &cx, SHOW).await,
            Err(SpotifyError::Http(_))
        ));
    });
    assert!(server.calls().is_empty());
}
