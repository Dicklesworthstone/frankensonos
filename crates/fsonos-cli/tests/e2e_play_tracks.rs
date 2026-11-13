//! Ordered playback through a real CLI process and a synthetic loopback daemon.
//! The routed simulator detects any attempt to replay a remote write locally.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_api::surface::queue::MAX_QUEUE_TRACKS;
use fsonos_api::{ErrorCode, Failure};
use fsonos_proto::control as soap;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const TRACK: &str = "spotify:track:1000000000000000000001";
const EPISODE: &str = "https://open.spotify.com/episode/1000000000000000000002?si=fixture";
const REVISION: &str = "q1:original&?#";

#[derive(Debug)]
struct Request {
    line: String,
    body: Value,
}

enum Reply {
    Json(u16, Value),
    /// Receive the entire mutation, then close without acknowledging it.
    Lost,
}

fn receive(stream: &mut TcpStream) -> Request {
    // Accepted sockets inherit nonblocking mode on some platforms.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    assert!(reader.read_line(&mut line).unwrap() > 0);
    let request_line = line.trim_end().to_owned();
    let mut length = 0;
    let mut header_bytes = line.len();
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        header_bytes += line.len();
        assert!(header_bytes <= 16 * 1024, "oversized request headers");
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().unwrap();
        }
    }
    assert!(length <= 32 * 1024, "oversized selection request");
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        line: request_line,
        body: if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        },
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &Value) {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

fn through_daemon(
    s: &mut Scenario,
    step: &str,
    args: &[&str],
    reply: Reply,
) -> (Run, Vec<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    // Match the simulator's routes and token so normal daemon discovery selects
    // this endpoint. No --daemon flag is needed to prevent a local replay.
    std::fs::write(
        s.dir().join("data/daemon.json"),
        serde_json::to_vec(&json!({
            "http": address.to_string(),
            "cli_token": "play-tracks-fixture",
            "pid": std::process::id(),
            "routes": s.dir().join("routes.toml").canonicalize().unwrap(),
        }))
        .unwrap(),
    )
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut requests = Vec::new();
        while !stopping.load(Ordering::Acquire) {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "CLI did not finish its request");
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            let request = receive(&mut stream);
            let health = request.line == "GET /health HTTP/1.1";
            requests.push(request);
            if health {
                respond(&mut stream, 200, &json!({"status": "ok"}));
            } else if let Reply::Json(status, body) = &reply {
                respond(&mut stream, *status, body);
            }
        }
        requests
    });
    let address = address.to_string();
    let run = s.cli_env(
        step,
        args,
        &[("FSONOS_HTTP_ADDR", address.as_str())],
        &["FSONOS_DAEMON_URL"],
    );
    stop.store(true, Ordering::Release);
    (run, worker.join().expect("fake daemon"))
}

#[test]
fn cli_preserves_order_repeated_occurrences_revision_and_daemon_outcome() {
    let mut s = Scenario::start("play-tracks-cli-daemon");
    s.sim(SimHousehold::standard());
    let reply = json!({
        "done": "playing 3 selected tracks in Kitchen",
        "changed": true,
        "notes": [{"code": "VOLUME_CLAMPED", "detail": "Kitchen limited to 25 by policy"}],
    });
    let before = s.sim_handle().unwrap().soap_log().len();
    let (run, requests) = through_daemon(
        &mut s,
        "play-selected",
        &[
            "--json",
            "play-tracks",
            "Kitchen@S1",
            TRACK,
            EPISODE,
            TRACK,
            "--expected-revision",
            REVISION,
        ],
        Reply::Json(200, reply.clone()),
    );
    assert!(run.ok(), "{run:?}");
    assert_eq!(serde_json::from_str::<Value>(&run.stdout).unwrap(), reply);
    assert_eq!(requests.len(), 2, "one health probe and one mutation");
    assert_eq!(requests[0].line, "GET /health HTTP/1.1");
    assert_eq!(requests[1].line, "POST /play/tracks HTTP/1.1");
    assert_eq!(
        requests[1].body,
        json!({
            "zone": "Kitchen@S1",
            "tracks": [
                {"source_uri": TRACK},
                {"source_uri": EPISODE},
                {"source_uri": TRACK},
            ],
            "expected_revision": REVISION,
        }),
    );
    s.check(
        "play-selected",
        "cli",
        "the selected daemon receives one ordered replacement and its policy notes reach JSON output",
        s.sim_handle().unwrap().soap_log().len() == before,
        format!("{run:?}; {requests:?}"),
    );
    s.finish();
}

#[test]
fn invalid_selections_reject_before_daemon_probe_or_speaker_contact() {
    let mut s = Scenario::start("play-tracks-cli-invalid");
    s.sim(SimHousehold::standard());
    let mut oversized = vec!["--json", "play-tracks", "Kitchen"];
    oversized.extend(std::iter::repeat_n(TRACK, MAX_QUEUE_TRACKS + 1));
    let cases = [
        ("no-selection", vec!["--json", "play-tracks", "Kitchen"]),
        (
            "bad-track",
            vec!["--json", "play-tracks", "Kitchen", "spotify:track:short"],
        ),
        (
            "container-in-selection",
            vec![
                "--json",
                "play-tracks",
                "Kitchen",
                TRACK,
                EPISODE,
                "spotify:album:1000000000000000000001",
            ],
        ),
        ("oversized", oversized),
    ];
    let before = s.sim_handle().unwrap().soap_log().len();
    for (step, args) in cases {
        let (run, requests) = through_daemon(
            &mut s,
            step,
            &args,
            Reply::Json(200, json!({"done": "unexpected write", "changed": true})),
        );
        s.check(
            step,
            "cli",
            "invalid ordered playback fails before backend routing",
            !run.ok() && requests.is_empty() && s.sim_handle().unwrap().soap_log().len() == before,
            format!("{run:?}; {requests:?}"),
        );
    }
    s.finish();
}

#[test]
fn failed_or_lost_daemon_replies_never_retry_or_replace_local_music() {
    let mut s = Scenario::start("play-tracks-cli-terminal-daemon");
    s.sim(SimHousehold::standard());
    soap::add_uri_to_queue(
        &s.lan(),
        s.ip("Kitchen"),
        "x-file-cifs://example.invalid/original.flac",
        "",
        false,
    )
    .unwrap();
    soap::set_av_transport_uri(
        &s.lan(),
        s.ip("Kitchen"),
        "x-rincon-mp3radio://example.invalid/original-radio.mp3",
        "",
    )
    .unwrap();
    soap::play(&s.lan(), s.ip("Kitchen")).unwrap();
    let before = s.sim_handle().unwrap().soap_log().len();
    let cases = [
        (
            "queue-changed",
            Reply::Json(
                409,
                serde_json::to_value(
                    Failure::new(
                        ErrorCode::QueueChanged,
                        "destination queue changed before replacement",
                    )
                    .body(),
                )
                .unwrap(),
            ),
            "QUEUE_CHANGED",
        ),
        ("lost-reply", Reply::Lost, "OUTCOME_UNCERTAIN"),
    ];
    for (step, reply, code) in cases {
        let (run, requests) = through_daemon(
            &mut s,
            step,
            &["--json", "play-tracks", "Kitchen", TRACK, EPISODE, TRACK],
            reply,
        );
        assert_eq!(requests.len(), 2, "one health probe and no mutation retry");
        assert_eq!(requests[0].line, "GET /health HTTP/1.1");
        assert_eq!(requests[1].line, "POST /play/tracks HTTP/1.1");
        assert_eq!(
            requests[1].body,
            json!({
                "zone": "Kitchen",
                "tracks": [
                    {"source_uri": TRACK},
                    {"source_uri": EPISODE},
                    {"source_uri": TRACK},
                ],
            }),
        );
        s.check(
            step,
            "cli",
            "a daemon's explicit or ambiguous write failure is terminal and leaves local music alone",
            !run.ok()
                && (run.stdout.contains(code) || run.stderr.contains(code))
                && s.sim_handle().unwrap().soap_log().len() == before,
            format!("{run:?}; {requests:?}"),
        );
    }
    s.finish();
}
