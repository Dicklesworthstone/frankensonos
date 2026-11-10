//! Online catalog search stays independent of the library, DJ and speaker LAN.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::source::{ArtistSelection, CatalogItem, CatalogKind, CatalogPage, SpotifySources};
use fsonos_api::{ErrorCode, Failure, SpotifySearchDto, SpotifySearchQuery, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const KINDS: [CatalogKind; 6] = [
    CatalogKind::Track,
    CatalogKind::Album,
    CatalogKind::Artist,
    CatalogKind::Playlist,
    CatalogKind::Show,
    CatalogKind::Episode,
];

struct NoLan;

impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("catalog search must not contact a speaker")
    }
}

#[derive(Default)]
struct Calls(Mutex<Vec<SpotifySearchQuery>>);

struct Catalog {
    calls: Arc<Calls>,
    reply: Result<CatalogPage, Failure>,
    during: Option<Box<dyn Fn() + Send + Sync>>,
}

impl SpotifySources for Catalog {
    fn artist_tracks(&self, _: &str) -> Result<ArtistSelection, Failure> {
        panic!("search must not resolve a playback selection")
    }

    fn search_catalog(
        &self,
        query: &str,
        kind: CatalogKind,
        limit: u32,
    ) -> Result<CatalogPage, Failure> {
        self.calls.0.lock().unwrap().push(SpotifySearchQuery {
            query: query.into(),
            kind,
            limit,
        });
        if let Some(during) = &self.during {
            during();
        }
        self.reply.clone()
    }
}

fn page(kind: CatalogKind) -> CatalogPage {
    CatalogPage {
        items: [2, 1]
            .into_iter()
            .map(|id| CatalogItem {
                source_uri: format!("spotify:{}:{id:022}", kind.as_str()),
                title: format!("Catalog choice {id}"),
                subtitle: Some("A performer & guest".into()),
            })
            .collect(),
        more: true,
    }
}

fn base(policy: Policy) -> Surface {
    Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("catalog search must not discover speakers")),
        policy,
        Box::new(SystemClock),
    )
}

fn ready(policy: Policy, reply: Result<CatalogPage, Failure>) -> (Surface, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let catalog = Catalog {
        calls: Arc::clone(&calls),
        reply,
        during: None,
    };
    (base(policy).with_spotify_sources(Box::new(catalog)), calls)
}

fn request() -> SpotifySearchQuery {
    SpotifySearchQuery {
        query: "  Björk & 100% + ?/#  ".into(),
        kind: CatalogKind::Artist,
        limit: 2,
    }
}

#[test]
fn every_kind_searches_without_a_store_dj_or_speaker_and_preserves_provider_order() {
    for kind in KINDS {
        let expected = page(kind);
        let (surface, calls) = ready(Policy::default(), Ok(expected.clone()));
        let answer = surface
            .search_spotify(&Client::McpStdio, "  new music  ", kind, 2)
            .unwrap();
        assert_eq!(answer.query, "new music");
        assert_eq!(answer.kind, kind);
        assert_eq!(answer.items, expected.items);
        assert!(answer.more);
        assert_eq!(
            *calls.0.lock().unwrap(),
            [SpotifySearchQuery {
                query: "new music".into(),
                kind,
                limit: 2,
            }]
        );
        assert_eq!(kind.as_str().parse::<CatalogKind>().unwrap(), kind);
        assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
    }
}

#[test]
fn invalid_queries_and_limits_are_refused_before_the_provider() {
    let (surface, calls) = ready(Policy::default(), Ok(page(CatalogKind::Track)));
    for query in [
        "".into(),
        "  ".into(),
        "\nmusic".into(),
        "music\t".into(),
        "é".repeat(129),
    ] {
        let error = surface
            .search_spotify(&Client::McpStdio, &query, CatalogKind::Track, 10)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
    for limit in [0, 11, u32::MAX] {
        let error = surface
            .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, limit)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
    assert!(calls.0.lock().unwrap().is_empty());
    surface
        .search_spotify(&Client::McpStdio, &"é".repeat(128), CatalogKind::Track, 10)
        .unwrap();
    assert_eq!(calls.0.lock().unwrap().len(), 1);
}

#[test]
fn read_policy_name_and_class_both_gate_the_online_request() {
    for rule in [
        "deny = ['search_spotify']",
        "deny = ['read']",
        "allow = ['search_library']",
    ] {
        let policy = Policy::from_toml(&format!("[clients.\"mcp-stdio\"]\n{rule}\n")).unwrap();
        let (surface, calls) = ready(policy, Ok(page(CatalogKind::Track)));
        let error = surface
            .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, 10)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::PolicyDenied);
        assert!(calls.0.lock().unwrap().is_empty());
    }
    for permission in ["read", "search_spotify"] {
        let policy = Policy::from_toml(&format!(
            "[clients.\"mcp-stdio\"]\nallow = ['{permission}']\n"
        ))
        .unwrap();
        let (surface, calls) = ready(policy, Ok(page(CatalogKind::Track)));
        surface
            .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, 10)
            .unwrap();
        assert_eq!(calls.0.lock().unwrap().len(), 1);
    }
}

#[test]
fn auth_and_provider_failures_do_not_fall_back_to_local_music() {
    let error = base(Policy::default())
        .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, 10)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::SpotifyAuthRequired);
    let failed = Failure::new(ErrorCode::NotReady, "Spotify retry deadline is active")
        .with_hint("Try again after 90 seconds.");
    let (surface, calls) = ready(Policy::default(), Err(failed.clone()));
    assert_eq!(
        surface
            .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, 10)
            .unwrap_err(),
        failed
    );
    assert_eq!(calls.0.lock().unwrap().len(), 1);
}

#[test]
fn playback_only_providers_remain_compatible_and_report_search_not_implemented() {
    struct PlaybackOnly;

    impl SpotifySources for PlaybackOnly {
        fn artist_tracks(&self, _: &str) -> Result<ArtistSelection, Failure> {
            panic!("catalog search must not call artist playback")
        }
    }

    let surface = base(Policy::default()).with_spotify_sources(Box::new(PlaybackOnly));
    let error = surface
        .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, 10)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotImplemented);
}

fn invalid_pages() -> Vec<CatalogPage> {
    let mut invalid = Vec::new();
    for source in [
        "spotify:album:0000000000000000000001",
        "spotify:track:short",
        "spotify:track:000000000000000000000!",
        "https://open.spotify.com/track/0000000000000000000001",
        "http://example.invalid/audio.mp3",
    ] {
        let mut page = page(CatalogKind::Track);
        page.items[0].source_uri = source.into();
        invalid.push(page);
    }
    for title in [
        "".into(),
        "  ".into(),
        "broken\nlabel".into(),
        "é".repeat(2049),
    ] {
        let mut page = page(CatalogKind::Track);
        page.items[0].title = title;
        invalid.push(page);
    }
    for subtitle in ["broken\tlabel".into(), "x".repeat(4097)] {
        let mut page = page(CatalogKind::Track);
        page.items[0].subtitle = Some(subtitle);
        invalid.push(page);
    }
    let mut duplicate = page(CatalogKind::Track);
    duplicate.items[1] = duplicate.items[0].clone();
    invalid.push(duplicate);
    let mut oversized = page(CatalogKind::Track);
    oversized.items.push(CatalogItem {
        source_uri: "spotify:track:0000000000000000000003".into(),
        title: "Third choice".into(),
        subtitle: None,
    });
    invalid.push(oversized);
    invalid
}

#[test]
fn invalid_provider_results_are_rejected_as_a_whole_before_exposing_sources() {
    for invalid in invalid_pages() {
        let (surface, calls) = ready(Policy::default(), Ok(invalid));
        let error = surface
            .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, 2)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotReady);
        assert_eq!(calls.0.lock().unwrap().len(), 1);
    }
}

#[test]
fn empty_search_is_a_valid_observation_and_does_not_start_fallback_work() {
    let (surface, calls) = ready(
        Policy::default(),
        Ok(CatalogPage {
            items: Vec::new(),
            more: false,
        }),
    );
    let answer = surface
        .search_spotify(&Client::McpStdio, "no matches", CatalogKind::Artist, 10)
        .unwrap();
    assert!(answer.items.is_empty());
    assert!(!answer.more);
    assert!(answer.text().contains("No Spotify artist results"));
    assert_eq!(calls.0.lock().unwrap().len(), 1);
}

#[test]
fn forwarded_results_must_match_the_exact_normalized_query_and_kind() {
    let request = request();
    assert_eq!(
        request.path().unwrap(),
        "/spotify/search?q=Bj%C3%B6rk%20%26%20100%25%20%2B%20%3F%2F%23&kind=artist&limit=2"
    );
    let page = page(request.kind);
    let answer = SpotifySearchDto {
        query: request.query.trim().into(),
        kind: request.kind,
        items: page.items,
        more: page.more,
    };
    answer.validate_for(&request).unwrap();
    let mut changed = answer.clone();
    changed.query.push('!');
    assert!(changed.validate_for(&request).is_err());
    let mut changed = answer;
    changed.kind = CatalogKind::Track;
    assert!(changed.validate_for(&request).is_err());
}

#[test]
fn a_permission_revoked_during_the_lookup_does_not_release_catalog_results() {
    let directory = tempfile::tempdir().unwrap();
    let policy_file = directory.path().join("policy.toml");
    std::fs::write(&policy_file, "[clients.\"mcp-stdio\"]\nallow = ['read']\n").unwrap();
    let calls = Arc::new(Calls::default());
    let catalog = Catalog {
        calls: Arc::clone(&calls),
        reply: Ok(page(CatalogKind::Track)),
        during: Some(Box::new(move || {
            std::fs::write(
                &policy_file,
                "[clients.\"mcp-stdio\"]\ndeny = ['search_spotify']\n",
            )
            .unwrap();
        })),
    };
    let surface = base(Policy::default())
        .with_policy_file(directory.path().to_path_buf())
        .with_spotify_sources(Box::new(catalog));
    let error = surface
        .search_spotify(&Client::McpStdio, "music", CatalogKind::Track, 10)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::PolicyDenied);
    assert_eq!(calls.0.lock().unwrap().len(), 1);
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .blocking_threads(0, 4)
        .build()
        .expect("runtime")
}

fn http(surface: Surface, paths: &[&str]) -> Vec<(u16, Value)> {
    let app = Arc::new(fsonos_api::app(
        &Arc::new(surface),
        &fsonos_api::Identity::fixed(Client::LoopbackHttp),
        &fsonos_api::WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]),
    ));
    let server = Arc::new(TcpServer::new(ServerConfig::new("127.0.0.1:0")));
    let (sender, receiver) = mpsc::channel();
    let worker = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            runtime().block_on(async move {
                let cx = Cx::current().unwrap();
                let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
                sender.send(listener.local_addr().unwrap()).unwrap();
                let _ = server.serve_on_app(&cx, listener, app).await;
            });
        })
    };
    let address = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    let replies = runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let client = HttpClient::default_for_runtime(&cx);
        let mut replies = Vec::new();
        for path in paths {
            let response = client
                .get(format!("http://{address}{path}"))
                .send(&cx)
                .await
                .unwrap();
            replies.push((
                response.status,
                serde_json::from_slice(&response.body).unwrap(),
            ));
        }
        replies
    });
    server.shutdown();
    drop(std::net::TcpStream::connect(address));
    worker.join().unwrap();
    replies
}

#[test]
fn http_search_routes_decode_validate_and_describe_online_search() {
    let (surface, calls) = ready(Policy::default(), Ok(page(CatalogKind::Artist)));
    let paths = [
        "/spotify/search?q=%20Bj%C3%B6rk+%26+guests%20&kind=artist",
        "/spotify/search?q=music&kind=artist&limit=2",
        "/spotify/search?kind=artist",
        "/spotify/search?q=music",
        "/spotify/search?q=music&kind=unknown",
        "/spotify/search?q=music&kind=artist&limit=11",
        "/spotify/search?q=music&kind=artist&limit=-1",
        "/spotify/search?q=music&kind=artist&limit=2&limit=3",
        "/spotify/search?q=music&kind=artist&zone=Den",
        "/spotify/search?q=music%0A&kind=artist",
        "/spotify/search?q=%FF&kind=artist",
        "/openapi.json",
    ];
    let replies = http(surface, &paths);
    for (index, (status, body)) in replies.iter().enumerate() {
        assert_eq!(
            *status,
            if (2..11).contains(&index) { 422 } else { 200 },
            "{}: {body}",
            paths[index]
        );
    }
    assert_eq!(replies[0].1["query"], "Björk & guests");
    assert_eq!(replies[0].1["kind"], "artist");
    assert_eq!(replies[0].1["items"].as_array().unwrap().len(), 2);
    assert_eq!(replies[0].1["more"], true);
    let calls = calls.0.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].limit, 10);
    assert_eq!(calls[1].limit, 2);
    assert_eq!(
        replies[11].1["paths"]["/spotify/search"]["get"]["operationId"],
        "search_spotify"
    );
}

#[test]
fn catalog_kind_schema_uses_the_same_lowercase_names_as_requests_and_replies() {
    let schema = <CatalogKind as fastapi::fastapi_openapi::JsonSchema>::schema();
    let schema = serde_json::to_value(schema).unwrap();
    assert_eq!(
        schema["enum"],
        json!(["track", "album", "artist", "playlist", "show", "episode"])
    );
}

#[test]
fn http_denial_prevents_provider_io_and_returns_the_policy_error() {
    let policy = Policy::from_toml("[clients.\"loopback-http\"]\ndeny = ['read']\n").unwrap();
    let (surface, calls) = ready(policy, Ok(page(CatalogKind::Track)));
    let replies = http(surface, &["/spotify/search?q=music&kind=track"]);
    assert_eq!(replies[0].0, 403);
    assert_eq!(replies[0].1["code"], json!("POLICY_DENIED"));
    assert!(calls.0.lock().unwrap().is_empty());
}
