#![forbid(unsafe_code)]

use asupersync::Cx;
use asupersync::runtime::RuntimeBuilder;
use fastapi::{Method, Request, RequestContext, ResponseBody};
use fsonos_api::{Identity, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{MemStore, SpotifyAlbum, SpotifyCache, Store};
use serde_json::{Value, json};
use std::sync::Arc;

struct NoLan;
impl fsonos_proto::Transport for NoLan {
    fn soap_post(
        &self,
        _: std::net::IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        panic!("cache routes must not ask speakers")
    }
}
fn request(
    client: Client,
    policy: Policy,
    store: MemStore,
    path: &str,
    method: Method,
) -> (u16, Value) {
    let surface = Arc::new(
        Surface::new(
            Box::new(NoLan),
            Box::new(|_| panic!("no survey")),
            policy,
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(store), "test"),
    );
    let app = fsonos_api::app(
        &surface,
        &Identity::fixed(client),
        &WebPolicy::for_listener("127.0.0.1:8099".parse().unwrap(), &[]),
    );
    RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = RequestContext::new(cx, 0);
            let (path, query) = path
                .split_once('?')
                .map_or((path, None), |(p, q)| (p, Some(q.to_string())));
            let mut req = Request::new(method, path);
            req.set_query(query);
            req.headers_mut()
                .insert("content-type", b"application/json".to_vec());
            let (status, _, body) = app.handle(&ctx, &mut req).await.into_parts();
            let ResponseBody::Bytes(body) = body else {
                panic!("JSON bytes")
            };
            (status.as_u16(), serde_json::from_slice(&body).unwrap())
        })
}
#[test]
fn empty_cache_contract_and_configuration_hint() {
    assert_eq!(
        request(
            Client::LoopbackHttp,
            Policy::default(),
            MemStore::default(),
            "/spotify/tracks",
            Method::Get
        ),
        (200, json!({"total":0,"items":[]}))
    );
}
#[test]
fn cache_artwork_and_pagination_are_available_without_a_session() {
    let mut store = MemStore::default();
    store
        .save_spotify_cache(&SpotifyCache {
            albums: vec![SpotifyAlbum {
                id: "fake".into(),
                title: "Bach".into(),
                artist: "Pianist".into(),
                year: Some(1982),
                tracks: 3,
                uri: "spotify:album:fake".into(),
                art_url: Some("https://cdn.example.invalid/art.jpg".into()),
                saved: true,
            }],
            liked_uris: Vec::new(),
            track_uris: Vec::new(),
            synced_at: Some(42),
        })
        .unwrap();
    let (code, albums) = request(
        Client::LoopbackHttp,
        Policy::default(),
        store,
        "/spotify/albums?q=bach&limit=1",
        Method::Get,
    );
    assert_eq!(code, 200);
    assert_eq!(albums["total"], 1);
    assert_eq!(
        albums["items"][0]["art_url"],
        "https://cdn.example.invalid/art.jpg"
    );
    assert_eq!(albums["items"][0]["year"], 1982);
    assert!(albums["items"][0].get("saved").is_none());
}
#[test]
fn policy_denial_names_the_operation_id() {
    let policy = Policy::from_toml("[clients.unknown]\nallow = [\"spotify_status\"]").unwrap();
    let (code, body) = request(
        Client::Unknown,
        policy,
        MemStore::default(),
        "/spotify/tracks",
        Method::Get,
    );
    assert_eq!(code, 403);
    assert_eq!(body["code"], "POLICY_DENIED");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("list_spotify_tracks")
    );
}
#[test]
fn openapi_documents_all_spotify_operations_and_schemas() {
    let (_, spec) = request(
        Client::LoopbackHttp,
        Policy::default(),
        MemStore::default(),
        "/openapi.json",
        Method::Get,
    );
    for (path, method, operation, status) in [
        ("/auth/spotify/exchange", "post", "spotify_exchange", "200"),
        ("/spotify/status", "get", "spotify_status", "200"),
        ("/spotify/albums", "get", "list_spotify_albums", "200"),
        (
            "/spotify/albums/{id}/tracks",
            "get",
            "list_spotify_album_tracks",
            "200",
        ),
        ("/spotify/tracks", "get", "list_spotify_tracks", "200"),
        ("/spotify/search", "get", "search_spotify", "200"),
    ] {
        assert_eq!(spec["paths"][path][method]["operationId"], operation);
        assert!(
            spec["paths"][path][method]["responses"]
                .get(status)
                .is_some()
        );
    }
}
