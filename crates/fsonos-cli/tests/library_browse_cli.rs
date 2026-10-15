//! Real CLI processes and loopback HTTP, with no Spotify or physical speakers.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
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
                    assert!(Instant::now() < deadline, "CLI did not send expected requests");
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request = line.trim().to_owned();
            assert!(request.starts_with("GET "), "{request}");
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0, "missing headers");
                if line == "\r\n" { break; }
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
    let dir = tempfile::tempdir().unwrap();
    let result = finish(command(dir.path())
        .env("FSONOS_HTTP_ADDR", addr.to_string())
        .arg("--daemon")
        .args(args));
    (result, server.join().unwrap())
}

#[test]
fn cli_json_and_text_browse_the_selected_daemons_cache() {
    let args = ["--json", "dj", "browse", "albums", "--limit", "2", "--q", "Björk & Guests"];
    let (output, paths) = through_daemon(&args, 200, &page());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&output.stdout).unwrap(), page());
    assert_eq!(paths[1],
        "GET /library/browse?kind=albums&offset=0&limit=2&q=Bj%C3%B6rk%20%26%20Guests HTTP/1.1");
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
    let args = ["--json", "dj", "browse", "albums", "--limit", "2", "--expected-revision", "old"];
    let (output, paths) = through_daemon(&args, 200, &page());
    assert!(!output.status.success());
    assert!(error_text(&output).contains("INVALID_ARGUMENT"));
    assert_eq!(paths.len(), 2, "health and one read; no automatic restart");
    let mut wrong = page();
    wrong["items"][0]["source_uri"] = "spotify:track:WrongKind".into();
    let (output, _) = through_daemon(&["--json", "dj", "browse", "albums", "--limit", "2"], 200, &wrong);
    assert!(!output.status.success());
    assert!(error_text(&output).contains("INTERNAL"));
}

#[test]
fn daemon_failures_do_not_fall_back_to_an_empty_local_cache() {
    let (output, paths) = through_daemon(
        &["--json", "dj", "browse", "albums", "--limit", "2"],
        503,
        &serde_json::to_value(
            fsonos_api::Failure::new(fsonos_api::ErrorCode::NotReady, "cache is unavailable").body(),
        ).unwrap(),
    );
    assert!(!output.status.success());
    assert!(error_text(&output).contains("NOT_READY"));
    assert_eq!(paths.len(), 2);
}

#[test]
fn direct_empty_cache_needs_no_daemon_or_speaker_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let output = finish(command(dir.path()).args(["--direct", "--json", "dj", "browse"]));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["coverage"], "cached");
    assert_eq!(page["total"], 0);
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["next_offset"], Value::Null);
}
