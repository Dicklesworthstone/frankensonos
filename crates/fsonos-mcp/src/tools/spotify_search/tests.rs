use super::*;
use crate::tools::Daemon;
use fsonos_api::source::{ArtistSelection, CatalogItem, CatalogPage, SpotifySources};
use fsonos_api::{ErrorCode, Failure, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct NoLan;

impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("Spotify metadata search must not contact speakers")
    }
}

struct Catalog(Arc<AtomicUsize>);

impl SpotifySources for Catalog {
    fn artist_tracks(&self, _: &str) -> Result<ArtistSelection, Failure> {
        panic!("search must not resolve or play artist tracks")
    }

    fn search_catalog(
        &self,
        query: &str,
        kind: CatalogKind,
        limit: u32,
    ) -> Result<CatalogPage, Failure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert_eq!(query, request().query.trim());
        assert_eq!(kind, CatalogKind::Artist);
        assert_eq!(limit, 2);
        let response = response();
        Ok(CatalogPage {
            items: response.items,
            more: response.more,
        })
    }
}

fn local(policy: Policy) -> (Backend, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let surface = Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("Spotify metadata search must not discover speakers")),
        policy,
        Box::new(SystemClock),
    )
    .with_spotify_sources(Box::new(Catalog(Arc::clone(&calls))));
    (
        Backend::shared(Arc::new(surface), Client::McpStdio),
        calls,
    )
}

fn request() -> SpotifySearchQuery {
    SpotifySearchQuery {
        query: "  Björk & 100% + ?/#  ".into(),
        kind: CatalogKind::Artist,
        limit: 2,
    }
}

fn response() -> SpotifySearchDto {
    SpotifySearchDto {
        query: request().query.trim().into(),
        kind: CatalogKind::Artist,
        items: vec![CatalogItem {
            source_uri: "spotify:artist:0000000000000000000001".into(),
            title: "A catalog artist".into(),
            subtitle: None,
        }],
        more: true,
    }
}

struct Remote {
    calls: Mutex<Vec<Value>>,
    reply: Option<Result<Value, Failure>>,
}

impl Daemon for Remote {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        self.calls.lock().unwrap().push(json!([method, path, body]));
        self.reply.clone()
    }
}

fn remote(reply: Option<Result<Value, Failure>>) -> Arc<Remote> {
    Arc::new(Remote {
        calls: Mutex::default(),
        reply,
    })
}

#[test]
fn standalone_search_uses_the_online_provider_without_store_or_lan() {
    let (backend, calls) = local(Policy::default());
    let result = backend.search_spotify(&request()).unwrap();
    assert!(!result.is_error);
    assert_eq!(
        result.structured_content,
        Some(serde_json::to_value(response()).unwrap())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn selected_daemon_gets_one_encoded_request_and_owns_the_catalog_observation() {
    let daemon = remote(Some(Ok(serde_json::to_value(response()).unwrap())));
    let (backend, calls) = local(Policy::default());
    let result = backend
        .with_daemon(daemon.clone())
        .search_spotify(&request())
        .unwrap();
    assert_eq!(
        result.structured_content,
        Some(serde_json::to_value(response()).unwrap())
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        *daemon.calls.lock().unwrap(),
        [json!([
            "GET",
            "/spotify/search?q=Bj%C3%B6rk%20%26%20100%25%20%2B%20%3F%2F%23&kind=artist&limit=2",
            null
        ])]
    );
}

#[test]
fn absent_daemon_allows_standalone_search_without_switching_to_saved_music() {
    let daemon = remote(None);
    let (backend, calls) = local(Policy::default());
    backend
        .with_daemon(daemon.clone())
        .search_spotify(&request())
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(daemon.calls.lock().unwrap().len(), 1);
}

#[test]
fn denied_or_invalid_requests_do_not_contact_the_daemon_or_catalog_provider() {
    let policy =
        Policy::from_toml("[clients.\"mcp-stdio\"]\ndeny = ['search_spotify']\n").unwrap();
    let daemon = remote(Some(Ok(serde_json::to_value(response()).unwrap())));
    let (backend, calls) = local(policy);
    let error = backend
        .with_daemon(daemon.clone())
        .search_spotify(&request())
        .unwrap_err();
    assert_eq!(error.data.unwrap()["code"], "POLICY_DENIED");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(daemon.calls.lock().unwrap().is_empty());

    let (backend, calls) = local(Policy::default());
    let backend = backend.with_daemon(daemon.clone());
    for invalid in [
        SpotifySearchQuery {
            query: "\ninvalid".into(),
            ..request()
        },
        SpotifySearchQuery {
            limit: 11,
            ..request()
        },
    ] {
        let error = backend.search_spotify(&invalid).unwrap_err();
        assert_eq!(error.data.unwrap()["code"], "INVALID_ARGUMENT");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(daemon.calls.lock().unwrap().is_empty());
}

#[test]
fn terminal_daemon_failures_and_malformed_replies_never_fall_back_locally() {
    for reply in [
        Err(Failure::new(ErrorCode::SpotifyAuthRequired, "sign in")),
        Err(Failure::new(ErrorCode::NotReady, "Spotify cooldown")),
        Err(Failure::new(ErrorCode::PolicyDenied, "denied")),
        Ok(json!({"items": []})),
        Ok(json!({"query": "different", "kind": "invalid", "items": [], "more": false})),
    ] {
        let daemon = remote(Some(reply));
        let (backend, calls) = local(Policy::default());
        assert!(
            backend
                .with_daemon(daemon.clone())
                .search_spotify(&request())
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(daemon.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn unrelated_or_unsafe_daemon_sources_are_refused_before_exposing_selections() {
    let mut invalid = Vec::new();
    let mut page = response();
    page.query = "another search".into();
    invalid.push(page);
    let mut page = response();
    page.kind = CatalogKind::Album;
    invalid.push(page);
    let mut page = response();
    page.items[0].source_uri = "https://example.invalid/injected".into();
    invalid.push(page);
    let mut page = response();
    page.items[0].title = "line\nbreak".into();
    invalid.push(page);
    let mut page = response();
    page.items.push(page.items[0].clone());
    invalid.push(page);

    for page in invalid {
        let daemon = remote(Some(Ok(serde_json::to_value(page).unwrap())));
        let (backend, calls) = local(Policy::default());
        let error = backend
            .with_daemon(daemon.clone())
            .search_spotify(&request())
            .unwrap_err();
        assert_eq!(error.data.unwrap()["code"], "NOT_READY");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(daemon.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn full_profile_registers_the_read_only_search_with_explicit_query_and_kind() {
    let server = crate::server_with(vec!["localhost".into()], crate::Profile::Full);
    let tools = server.tools();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "search_spotify")
        .expect("online Spotify search is registered");
    let value = serde_json::to_value(tool).unwrap();
    let required = value["inputSchema"]["required"].as_array().unwrap();
    for field in ["query", "kind"] {
        assert!(required.contains(&json!(field)), "{value}");
    }
    assert!(!required.contains(&json!("limit")), "{value}");
    assert_eq!(value["annotations"]["readOnlyHint"], true);
    assert_eq!(value["annotations"]["idempotentHint"], true);
}
