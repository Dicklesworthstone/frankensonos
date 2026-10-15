#![forbid(unsafe_code)]

#[path = "support/spotify.rs"]
mod support;
use support::*;

fn exchange_body() -> Value {
    json!({"code": "good-code", "code_verifier": VERIFIER, "redirect_uri": REDIRECT})
}

fn body_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[test]
fn d1_d10_unconfigured_and_empty_cache_are_explicit() {
    let h = Harness::start(
        "daemon-empty",
        false,
        &Identity::fixed(Client::LoopbackHttp),
        Policy::default(),
        false,
    );
    let (status, body, _) = h.request_body(
        "POST",
        "/auth/spotify/exchange",
        &[],
        &exchange_body().to_string(),
    );
    assert_eq!(status, 503);
    assert_eq!(body_json(&body)["code"], "SPOTIFY_NOT_CONFIGURED");
    assert!(
        body_json(&body)["hint"]
            .as_str()
            .unwrap()
            .contains("FSONOS_SPOTIFY_CLIENT_ID")
    );
    let status = h.json("GET", "/spotify/status").1;
    assert_eq!(
        status,
        json!({"configured":false,"signed_in":false,"reauthorize":false, "app_redirect_uri":"frankensonos://spotify-callback",
        "library":{"albums":0,"tracks":0,"synced_at":null},
        "sync":{"running":false,"done":0,"total":0,"error":null}})
    );
    for path in ["/spotify/albums", "/spotify/tracks"] {
        assert_eq!(h.json("GET", path), (200, json!({"total":0,"items":[]})));
    }
    assert_eq!(
        h.json("GET", "/spotify/albums/missing/tracks"),
        (200, json!([]))
    );
    h.no_secrets();
}

#[test]
fn d13_search_reads_all_of_spotify_without_a_synced_library() {
    let h = Harness::local("daemon-search");
    // Signed out: the search says so and nothing else.
    let (code, body) = h.json("GET", "/spotify/search?q=gould");
    assert_eq!(code, 409);
    assert_eq!(body["code"], "SPOTIFY_AUTH_REQUIRED");
    h.authorize();
    // The library was never synced; the results still come from the catalog.
    assert_eq!(h.json("GET", "/spotify/status").1["library"]["albums"], 0);
    let (code, found) = h.json("GET", "/spotify/search?q=gould%20bach&limit=5");
    assert_eq!(code, 200, "{found}");
    let albums = found["albums"].as_array().unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0]["uri"], "spotify:album:SearchAlbum0000000001");
    assert_eq!(albums[0]["title"], "Bach: The Well-Tempered Clavier");
    assert_eq!(albums[0]["artist"], "Glenn Gould");
    assert_eq!(albums[0]["year"], 1963);
    assert_eq!(albums[0]["tracks"], 48);
    assert_eq!(
        albums[0]["art_url"],
        "https://cdn.example.invalid/search.jpg"
    );
    // The unplayable track is left out, the null entry is skipped.
    let tracks = found["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0]["uri"], "spotify:track:SearchTrack0000000001");
    assert_eq!(tracks[0]["artists"], json!(["Glenn Gould"]));
    assert_eq!(tracks[0]["album"], "Bach: The Well-Tempered Clavier");
    assert_eq!(tracks[0]["duration_secs"], 200);
    assert_eq!(h.json("GET", "/spotify/search").0, 422);
    assert_eq!(h.json("GET", "/spotify/search?q=%20%20").0, 422);
    assert_eq!(h.json("GET", "/spotify/search?q=x&limit=0").0, 422);
    h.no_secrets();
}

#[test]
fn d11_policy_names_the_new_operation_ids() {
    let policy = Policy::from_toml("[clients.unknown]\nallow = [\"spotify_status\"]\n").unwrap();
    let h = Harness::start(
        "daemon-policy",
        true,
        &Identity::fixed(Client::Unknown),
        policy,
        false,
    );
    assert_eq!(h.json("GET", "/spotify/status").0, 200);
    for (method, path, id) in [
        ("GET", "/spotify/albums", "list_spotify_albums"),
        ("GET", "/spotify/tracks", "list_spotify_tracks"),
        ("GET", "/spotify/search?q=x", "search_spotify"),
        (
            "GET",
            "/spotify/albums/x/tracks",
            "list_spotify_album_tracks",
        ),
    ] {
        let (code, body) = h.json(method, path);
        assert_eq!(code, 403);
        assert_eq!(body["code"], "POLICY_DENIED");
        assert!(body["detail"].as_str().unwrap().contains(id));
    }
}
