//! Real CLI processes and loopback HTTP, with no Spotify or physical speakers.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A scratch data directory for one fsonos run, removed afterwards.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fsonos-library-browse-cli-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn page() -> Value {
    json!({
        "kind": "albums", "album_uri": null, "offset": 0, "limit": 2,
        "total": 3, "next_offset": 2, "revision": "cache-test", "coverage": "cached",
        "items": [
            { "kind": "album", "source_uri": "spotify:album:TestA", "title": "First",
              "subtitle": "Björk & Guests", "album": null, "album_uri": "spotify:album:TestA",
              "disc_number": null, "track_number": null, "duration_secs": null,
              "explicit": true, "cached_tracks": 5 },
            { "kind": "album", "source_uri": "spotify:album:TestB", "title": "Second",
              "subtitle": "Other", "album": null, "album_uri": "spotify:album:TestB",
              "disc_number": null, "track_number": null, "duration_secs": null,
              "explicit": false, "cached_tracks": 1 }
        ]
    })
}

fn command(dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fsonos"));
    command
        .env("FSONOS_DATA_DIR", dir)
        .env_remove("FSONOS_DAEMON_URL")
        .env_remove("FSONOS_HTTP_ADDR")
        .env_remove("FSONOS_ROUTES")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn finish(command: &mut Command) -> Output {
    let mut child = command.spawn().expect("spawn CLI");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().expect("child status").is_some() {
            return child.wait_with_output().expect("CLI output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cached browsing exceeded its test deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn error_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn through_daemon(args: &[&str], status: u16, body: &Value) -> (Output, Vec<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let body = body.to_string();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut paths = Vec::new();
        while paths.len() < 2 {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "CLI did not send expected requests"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            // macOS hands it the listener's non-blocking mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request = line.trim().to_owned();
            assert!(request.starts_with("GET "), "{request}");
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0, "missing headers");
                if line == "\r\n" {
                    break;
                }
            }
            drop(reader);
            let (code, reply) = if paths.is_empty() {
                assert_eq!(request, "GET /health HTTP/1.1");
                (200, "{\"status\":\"ok\"}")
            } else {
                (status, body.as_str())
            };
            write!(stream,
                "HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            ).unwrap();
            paths.push(request);
        }
        paths
    });
    let dir = Scratch::new("daemon");
    let result = finish(
        command(dir.path())
            .env("FSONOS_HTTP_ADDR", addr.to_string())
            .arg("--daemon")
            .args(args),
    );
    (result, server.join().unwrap())
}

#[test]
fn cli_json_and_text_browse_the_selected_daemons_cache() {
    let args = [
        "--json",
        "dj",
        "browse",
        "albums",
        "--limit",
        "2",
        "--q",
        "Björk & Guests",
    ];
    let (output, paths) = through_daemon(&args, 200, &page());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        page()
    );
    assert_eq!(
        paths[1],
        "GET /library/browse?kind=albums&offset=0&limit=2&q=Bj%C3%B6rk%20%26%20Guests HTTP/1.1"
    );
    let (output, _) = through_daemon(&["dj", "browse", "albums", "--limit", "2"], 200, &page());
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("source_uri: spotify:album:TestA"));
    assert!(text.contains("5 cached track(s); the album may contain more"));
    assert!(text.contains("Explicit content"));
    assert!(text.contains("offset=2"));
}

#[test]
fn stale_and_inconsistent_pages_do_not_become_successful_cli_output() {
    let args = [
        "--json",
        "dj",
        "browse",
        "albums",
        "--limit",
        "2",
        "--expected-revision",
        "old",
    ];
    let (output, paths) = through_daemon(&args, 200, &page());
    assert!(!output.status.success());
    assert!(error_text(&output).contains("INVALID_ARGUMENT"));
    assert_eq!(paths.len(), 2, "health and one read; no automatic restart");
    let mut wrong = page();
    wrong["items"][0]["source_uri"] = "spotify:track:WrongKind".into();
    let (output, _) = through_daemon(
        &["--json", "dj", "browse", "albums", "--limit", "2"],
        200,
        &wrong,
    );
    assert!(!output.status.success());
    assert!(error_text(&output).contains("INTERNAL"));
}

#[test]
fn daemon_failures_do_not_fall_back_to_an_empty_local_cache() {
    let (output, paths) = through_daemon(
        &["--json", "dj", "browse", "albums", "--limit", "2"],
        503,
        &serde_json::to_value(
            fsonos_api::Failure::new(fsonos_api::ErrorCode::NotReady, "cache is unavailable")
                .body(),
        )
        .unwrap(),
    );
    assert!(!output.status.success());
    assert!(error_text(&output).contains("NOT_READY"));
    assert_eq!(paths.len(), 2);
}

#[test]
fn direct_empty_cache_needs_no_daemon_or_speaker_discovery() {
    let dir = Scratch::new("direct");
    let output = finish(command(dir.path()).args(["--direct", "--json", "dj", "browse"]));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["coverage"], "cached");
    assert_eq!(page["total"], 0);
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["next_offset"], Value::Null);
}

fn playlist_page(offset: usize, limit: usize, q: &str) -> Value {
    let items = [1, 3, 5]
        .into_iter()
        .map(|position| {
            let repeated = position != 3;
            json!({
                "position": position,
                "source_uri": if repeated { "spotify:track:Repeated" } else { "spotify:track:Different" },
                "title": if repeated { "Again" } else { "Middle" },
                "subtitle": "Björk & Guests", "album": "A recording",
                "album_uri": "spotify:album:TestAlbum", "disc_number": 1,
                "track_number": if repeated { 1 } else { 2 },
                "duration_secs": 180, "explicit": repeated
            })
        })
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let end = offset + items.len();
    json!({
        "playlist_uri": "spotify:playlist:ObservedList", "name": "A list", "q": q,
        "coverage": "partial", "fetched_at": 100, "source_total": 8, "examined": 5,
        "cached_total": 3, "total": 3, "offset": offset, "limit": limit,
        "next_offset": if end < 3 { Some(end) } else { None },
        "revision": "playlist-observation", "items": items
    })
}

#[test]
fn cli_playlist_json_and_text_preserve_order_duplicates_and_partial_coverage() {
    let args = [
        "--json",
        "dj",
        "browse",
        "tracks",
        "--playlist-uri",
        "spotify:playlist:ObservedList",
        "--limit",
        "3",
    ];
    let page = playlist_page(0, 3, "");
    let (output, paths) = through_daemon(&args, 200, &page);
    assert!(output.status.success(), "{}", error_text(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        page
    );
    assert_eq!(
        paths[1],
        "GET /library/playlist?playlist_uri=spotify%3Aplaylist%3AObservedList&offset=0&limit=3 HTTP/1.1"
    );
    let (output, _) = through_daemon(&args[1..], 200, &page);
    assert!(output.status.success(), "{}", error_text(&output));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Coverage: partial"));
    assert!(text.contains("Revision: playlist-observation"));
    assert!(text.contains("Source entries examined: 5 of 8"));
    assert_eq!(
        text.matches("source_uri: spotify:track:Repeated").count(),
        2
    );
    assert!(text.find("#1 Again").unwrap() < text.find("#3 Middle").unwrap());
    assert!(text.find("#3 Middle").unwrap() < text.find("#5 Again").unwrap());
    assert!(text.contains("not a Sonos queue"));
}

#[test]
fn cli_playlist_continuation_keeps_the_filter_revision_and_original_position() {
    let args = [
        "--json",
        "dj",
        "browse",
        "tracks",
        "--playlist-uri",
        "spotify:playlist:ObservedList",
        "--offset",
        "1",
        "--limit",
        "1",
        "--q",
        "Björk & Guests",
        "--expected-revision",
        "playlist-observation",
    ];
    let page = playlist_page(1, 1, "Björk & Guests");
    let (output, paths) = through_daemon(&args, 200, &page);
    assert!(output.status.success(), "{}", error_text(&output));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        page
    );
    assert_eq!(
        paths[1],
        "GET /library/playlist?playlist_uri=spotify%3Aplaylist%3AObservedList&offset=1&limit=1&q=Bj%C3%B6rk%20%26%20Guests&expected_revision=playlist-observation HTTP/1.1"
    );
    assert_eq!(page["items"][0]["position"], 3);
    assert_eq!(page["next_offset"], 2);
}

#[test]
fn cli_playlist_rejects_stale_or_inconsistent_results_without_switching_caches() {
    let args = [
        "--json",
        "dj",
        "browse",
        "tracks",
        "--playlist-uri",
        "spotify:playlist:ObservedList",
        "--limit",
        "3",
        "--expected-revision",
        "playlist-observation",
    ];
    let mut changed = playlist_page(0, 3, "");
    changed["revision"] = "different-observation".into();
    let (output, paths) = through_daemon(&args, 200, &changed);
    assert!(!output.status.success());
    assert!(error_text(&output).contains("INVALID_ARGUMENT"));
    assert_eq!(paths.len(), 2, "health and one read; no automatic restart");
    let mut malformed = playlist_page(0, 3, "");
    malformed["items"][1]["position"] = 1.into();
    let (output, _) = through_daemon(&args, 200, &malformed);
    assert!(!output.status.success());
    assert!(error_text(&output).contains("INTERNAL"));
    let failure = fsonos_api::Failure::new(fsonos_api::ErrorCode::NotReady, "cache unavailable");
    let (output, paths) =
        through_daemon(&args, 503, &serde_json::to_value(failure.body()).unwrap());
    assert!(!output.status.success());
    assert!(error_text(&output).contains("NOT_READY"));
    assert_eq!(paths.len(), 2);
}

#[test]
fn invalid_playlist_continuation_fails_before_daemon_discovery() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let dir = Scratch::new("invalid-playlist");
    let output = finish(
        command(dir.path())
            .env(
                "FSONOS_HTTP_ADDR",
                listener.local_addr().unwrap().to_string(),
            )
            .args([
                "--daemon",
                "--json",
                "dj",
                "browse",
                "tracks",
                "--playlist-uri",
                "spotify:playlist:ObservedList",
                "--offset",
                "1",
            ]),
    );
    assert!(!output.status.success());
    assert!(error_text(&output).contains("INVALID_ARGUMENT"));
    assert!(error_text(&output).contains("expected_revision"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "invalid continuation must not probe the daemon"
    );
}
