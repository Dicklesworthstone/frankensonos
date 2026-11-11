//! A real CLI follower survives a lost local read and a daemon replacement.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(10);

struct Follower(Child);

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_follower(dir: &Path, count: u64, url: Option<&str>) -> Follower {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fsonos"));
    command
        .arg("--data-dir")
        .arg(dir)
        .env_remove("FSONOS_DAEMON_URL")
        .env_remove("FSONOS_HTTP_ADDR")
        .env_remove("FSONOS_ROUTES")
        .env("FSONOS_TAILSCALE", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(url) = url {
        command.args(["--daemon-url", url]);
    }
    command
        .args(["--json", "events", "--since", "40", "--count"])
        .arg(count.to_string());
    Follower(command.spawn().unwrap())
}

fn completed(child: &mut Follower) -> ExitStatus {
    let until = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < until, "the event follower did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn daemon_file(dir: &Path, addr: SocketAddr, token: &str) {
    std::fs::write(
        dir.join("daemon.json"),
        serde_json::to_vec(&json!({
            "http": addr,
            "cli_token": token,
            "pid": 1,
            "routes": null,
            "serve": null,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn request(listener: &TcpListener) -> (TcpStream, String) {
    let until = Instant::now() + DEADLINE;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "the CLI did not make its next read");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept CLI: {error}"),
        }
    };
    stream.set_read_timeout(Some(DEADLINE)).unwrap();
    stream.set_write_timeout(Some(DEADLINE)).unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut headers = String::new();
    while !headers.ends_with("\r\n\r\n") {
        assert!(reader.read_line(&mut headers).unwrap() > 0);
    }
    drop(reader);
    (stream, headers)
}

fn answer(stream: TcpStream, value: &Value) {
    answer_status(stream, "200 OK", value);
}

fn answer_status(mut stream: TcpStream, status: &str, value: &Value) {
    let body = serde_json::to_string(value).unwrap();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn policy_refusal(stream: TcpStream) {
    answer_status(
        stream,
        "403 Forbidden",
        &json!({
            "code": "POLICY_DENIED",
            "detail": "loopback-http may not use events",
            "hint": "The current CLI identity is required.",
            "suggestions": [],
            "upnp_code": null,
            "retryable": false,
        }),
    );
}

#[test]
fn local_events_resume_with_new_address_token_and_reset_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let before = TcpListener::bind("127.0.0.1:0").unwrap();
    let after = TcpListener::bind("127.0.0.1:0").unwrap();
    before.set_nonblocking(true).unwrap();
    after.set_nonblocking(true).unwrap();
    daemon_file(dir.path(), before.local_addr().unwrap(), "old-local-token");
    let mut child = spawn_follower(dir.path(), 3, None);
    let (health, headers) = request(&before);
    assert!(headers.starts_with("GET /health HTTP/1.1\r\n"));
    answer(health, &json!({"status": "ok"}));

    let event = |id| json!({"id": id, "event": "zone.state", "data": {"volume": 30}});
    let (read, headers) = request(&before);
    assert!(headers.starts_with("GET /events?wait=15&since=40 HTTP/1.1\r\n"));
    assert!(headers.contains("old-local-token"));
    answer(
        read,
        &json!({"events": [event(41)], "last_id": 41, "reset": null}),
    );

    let (lost, headers) = request(&before);
    assert!(headers.starts_with("GET /events?wait=15&since=41 HTTP/1.1\r\n"));
    daemon_file(dir.path(), after.local_addr().unwrap(), "new-local-token");
    drop(lost);
    drop(before);

    let (read, headers) = request(&after);
    assert!(headers.starts_with("GET /events?wait=15&since=41 HTTP/1.1\r\n"));
    assert!(headers.contains("new-local-token"));
    assert!(!headers.contains("old-local-token"));
    answer(read, &json!({"events": [], "last_id": 0, "reset": 1}));
    let (read, headers) = request(&after);
    assert!(headers.starts_with("GET /events?wait=15&since=0 HTTP/1.1\r\n"));
    answer(
        read,
        &json!({"events": [event(1)], "last_id": 1, "reset": null}),
    );

    let status = completed(&mut child);
    let output: Vec<Value> = BufReader::new(child.0.stdout.take().unwrap())
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    let errors: Vec<_> = BufReader::new(child.0.stderr.take().unwrap())
        .lines()
        .collect();
    assert!(status.success(), "{errors:?}");
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["id"], 41);
    assert_eq!(output[1]["event"], "events.reset");
    assert_eq!(output[1]["data"], json!({"oldest": 1}));
    assert_eq!(output[2]["id"], 1);
    assert!(
        after.accept().is_err(),
        "a stopped reader made another request"
    );
}

#[test]
fn local_events_retry_when_credentials_rotate_during_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    daemon_file(dir.path(), address, "old-local-token");
    let mut child = spawn_follower(dir.path(), 1, None);
    let (health, _) = request(&listener);
    answer(health, &json!({"status": "ok"}));

    let (read, headers) = request(&listener);
    assert!(headers.starts_with("GET /events?wait=15&since=40 HTTP/1.1\r\n"));
    assert!(headers.contains("old-local-token"));
    // The new daemon uses the same socket. Only its credentials changed
    // after the follower resolved them and sent its request.
    daemon_file(dir.path(), address, "new-local-token");
    policy_refusal(read);

    let (read, headers) = request(&listener);
    assert!(headers.starts_with("GET /events?wait=15&since=40 HTTP/1.1\r\n"));
    assert!(headers.contains("new-local-token"));
    assert!(!headers.contains("old-local-token"));
    answer(
        read,
        &json!({
            "events": [{"id": 41, "event": "zone.state", "data": {"volume": 31}}],
            "last_id": 41,
            "reset": null,
        }),
    );
    assert!(completed(&mut child).success());
    let output: Vec<Value> = BufReader::new(child.0.stdout.take().unwrap())
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["id"], 41);
    assert!(listener.accept().is_err());
}

#[test]
fn an_unchanged_local_policy_refusal_is_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    daemon_file(dir.path(), listener.local_addr().unwrap(), "local-token");
    let mut child = spawn_follower(dir.path(), 1, None);
    let (health, _) = request(&listener);
    answer(health, &json!({"status": "ok"}));
    let (read, headers) = request(&listener);
    assert!(headers.contains("local-token"));
    policy_refusal(read);

    assert_eq!(completed(&mut child).code(), Some(5));
    assert!(
        listener.accept().is_err(),
        "unchanged credentials cannot turn a policy refusal into a retry"
    );
}

#[test]
fn a_url_refusal_stays_terminal_when_local_credentials_change() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    daemon_file(dir.path(), address, "old-local-token");
    let url = format!("http://{address}");
    let mut child = spawn_follower(dir.path(), 1, Some(&url));
    let (health, headers) = request(&listener);
    assert!(!headers.to_ascii_lowercase().contains("x-fsonos-cli-token"));
    answer(health, &json!({"status": "ok"}));
    let (read, headers) = request(&listener);
    assert!(!headers.to_ascii_lowercase().contains("x-fsonos-cli-token"));
    daemon_file(dir.path(), address, "new-local-token");
    policy_refusal(read);

    assert_eq!(completed(&mut child).code(), Some(5));
    assert!(
        listener.accept().is_err(),
        "URL readers never borrow a new local identity or retry its refusal"
    );
}
