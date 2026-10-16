use super::*;
use crate::client::{CachedToken, Endpoints, SCOPE, TokenCache};
use crate::fake_spotify::{config, runtime, scratch_dir};
use asupersync::http::Client;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

#[test]
fn transient_statuses_and_one_shared_attempt_budget() {
    for status in [408, 429, 500, 502, 503, 504] {
        assert!(retry_delay(0, Some(status), None, 0, 5, READ_TIMEOUT).is_some());
        assert!(retry_delay(3, Some(status), None, 0, 5, READ_TIMEOUT).is_none());
    }
    for status in [301, 400, 401, 403, 404, 409, 501, 505] {
        assert!(retry_delay(0, Some(status), None, 0, 5, READ_TIMEOUT).is_none());
    }
    for (attempt, millis) in [(0, 250), (1, 500), (2, 1000)] {
        assert_eq!(
            retry_delay(attempt, Some(503), None, 0, 5, READ_TIMEOUT),
            Some(Duration::from_millis(millis))
        );
    }
}

#[test]
fn retry_after_is_a_minimum_not_a_delay_to_cap() {
    assert_eq!(
        retry_after(Some(" 12 "), 0),
        Ok(Some(Duration::from_secs(12)))
    );
    assert_eq!(retry_after(Some("0"), 0), Ok(Some(Duration::ZERO)));
    assert_eq!(retry_after(None, 0), Ok(None));
    for value in ["", "junk", "-2", "1.5"] {
        assert_eq!(retry_after(Some(value), 0), Ok(None));
    }
    assert_eq!(retry_after(Some("18446744073709551616"), 0), Err(()));
    for value in ["6", "36000", "18446744073709551615", "18446744073709551616"] {
        assert!(retry_delay(0, Some(429), Some(value), 0, 5, READ_TIMEOUT).is_none());
    }
    assert!(retry_delay(0, Some(503), Some("90"), 0, u64::MAX, READ_TIMEOUT).is_none());
    assert!(retry_delay(0, Some(503), None, 0, 5, Duration::from_millis(250)).is_none());
    assert!(retry_delay(0, Some(429), Some("0"), 0, 0, READ_TIMEOUT).is_none());
}

#[test]
fn http_dates_are_supported_without_early_retries() {
    let at = DateTime::parse_from_rfc2822("Thu, 15 Oct 2026 20:00:10 GMT")
        .unwrap()
        .timestamp();
    assert_eq!(
        retry_after(Some("Thu, 15 Oct 2026 20:00:10 GMT"), at - 10),
        Ok(Some(Duration::from_secs(10)))
    );
    assert_eq!(
        retry_after(Some("Thu, 15 Oct 2026 20:00:10 GMT"), at + 10),
        Ok(Some(Duration::ZERO))
    );
    assert!(
        retry_delay(
            0,
            Some(429),
            Some("Thu, 15 Oct 2026 20:00:10 GMT"),
            at - 10,
            5,
            READ_TIMEOUT,
        )
        .is_none()
    );
}

#[test]
fn only_transient_transport_failures_are_replayed() {
    for kind in [
        ErrorKind::ConnectionReset,
        ErrorKind::TimedOut,
        ErrorKind::UnexpectedEof,
    ] {
        assert!(transient_transport(&ClientError::Io(kind.into())));
        assert!(transient_transport(&ClientError::HttpError(HttpError::Io(
            kind.into()
        ))));
    }
    for error in [
        ClientError::Cancelled,
        ClientError::DeadlineExceeded,
        ClientError::InvalidUrl("invalid".into()),
        ClientError::TlsError("untrusted issuer".into()),
        ClientError::HttpError(HttpError::BadHeader),
        ClientError::HttpError(HttpError::BodyTooLarge),
        ClientError::HttpError(HttpError::BodyCancelled),
        ClientError::Io(ErrorKind::PermissionDenied.into()),
    ] {
        assert!(!transient_transport(&error), "{error}");
    }
}

/// A scripted peer on a real socket. The production asupersync client performs
/// all encoding, response parsing and deadline handling; no HTTP-client mock.
struct Reply {
    status: u16,
    body: String,
    after: Option<String>,
    stall: Duration,
    hang_up: bool,
}

impl Reply {
    fn status(status: u16) -> Self {
        Self {
            status,
            body: if status == 200 {
                "{\"ok\":true}".into()
            } else {
                format!(r#"{{"error":{{"status":{status},"message":"scripted failure"}}}}"#)
            },
            after: None,
            stall: Duration::ZERO,
            hang_up: false,
        }
    }

    fn after(mut self, value: &str) -> Self {
        self.after = Some(value.into());
        self
    }

    fn token() -> Self {
        Self {
            body: format!(
                r#"{{"access_token":"rotated-access","refresh_token":"rotated-refresh","token_type":"Bearer","expires_in":3600,"scope":"{SCOPE}"}}"#
            ),
            ..Self::status(200)
        }
    }
}

#[derive(Debug)]
struct Seen {
    method: String,
    target: String,
    authorization: Option<String>,
    at: Instant,
}

struct Server {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn read_request(stream: &mut TcpStream) -> Seen {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap().to_string();
    let target = parts.next().unwrap().to_string();
    let (mut length, mut authorization) = (0, None);
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().unwrap();
        }
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().into());
        }
    }
    assert!(length < 65536);
    reader.read_exact(&mut vec![0; length]).unwrap();
    Seen {
        method,
        target,
        authorization,
        at: Instant::now(),
    }
}

impl Server {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let log = Arc::clone(&seen);
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut replies = VecDeque::from(replies);
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                log.lock().unwrap().push(read_request(&mut stream));
                let reply = replies.pop_front().unwrap_or_else(|| Reply::status(418));
                if reply.hang_up {
                    continue;
                }
                let after = reply
                    .after
                    .map_or_else(String::new, |value| format!("Retry-After: {value}\r\n"));
                let head = format!(
                    "HTTP/1.1 {} Scripted\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n{after}\r\n",
                    reply.status,
                    reply.body.len()
                );
                if stream.write_all(head.as_bytes()).is_err() {
                    continue;
                }
                thread::sleep(reply.stall);
                let _ = stream.write_all(reply.body.as_bytes());
            }
        });
        Self {
            address,
            seen,
            stop,
            worker: Some(worker),
        }
    }

    fn session(&self) -> (Session, PathBuf) {
        let directory = scratch_dir("read-recovery");
        let cache = TokenCache::in_data_dir(&directory);
        cache
            .store(&CachedToken {
                access_token: "test-access".into(),
                refresh_token: "test-refresh".into(),
                expires_at: unix_now() + 3600,
                scope: SCOPE.into(),
            })
            .unwrap();
        let mut endpoints = Endpoints::for_session();
        endpoints.api = format!("http://{}/v1", self.address);
        endpoints.token = format!("http://{}/api/token", self.address);
        let client = Client::builder().no_retries().no_redirects().build();
        (
            Session::open(config(), cache, client)
                .unwrap()
                .with_endpoints(endpoints),
            directory,
        )
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn transient_status_recovers_with_the_same_authorized_get() {
    let server = Server::start(vec![
        Reply::status(503),
        Reply::status(502),
        Reply::status(200),
    ]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert_eq!(session.get(&cx, &url).await.unwrap(), b"{\"ok\":true}");
    });
    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|seen| {
        seen.method == "GET"
            && seen.target == "/v1/me/tracks"
            && seen.authorization.as_deref() == Some("Bearer test-access")
    }));
    assert!(seen[1].at.duration_since(seen[0].at) >= Duration::from_millis(250));
    assert!(seen[2].at.duration_since(seen[1].at) >= Duration::from_millis(500));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_lost_read_response_can_be_requested_again() {
    let lost = Reply {
        hang_up: true,
        ..Reply::status(200)
    };
    let server = Server::start(vec![lost, Reply::status(200)]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        session.get(&cx, &url).await.unwrap();
    });
    assert_eq!(server.count(), 2);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn repeated_failures_end_with_the_last_api_error() {
    let server = Server::start((0..4).map(|_| Reply::status(503)).collect());
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert!(matches!(
            session.get(&cx, &url).await,
            Err(SpotifyError::Api { status: 503, .. })
        ));
    });
    assert_eq!(server.count(), 4);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn permanent_failures_and_grant_checks_are_not_retried() {
    for (status, grant) in [
        (400, false),
        (403, false),
        (404, false),
        (501, false),
        (503, true),
    ] {
        let server = Server::start(vec![Reply::status(status), Reply::status(200)]);
        let (mut session, directory) = server.session();
        runtime().block_on(async move {
            let cx = Cx::current().unwrap();
            let url = format!("{}/me/tracks", session.endpoints.api);
            let result = if grant {
                session.check_grant(&cx).await.map(|_| Vec::new())
            } else {
                session.get(&cx, &url).await
            };
            assert!(matches!(result, Err(SpotifyError::Api { status: got, .. }) if got == status));
        });
        assert_eq!(server.count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn oversized_retry_after_never_becomes_an_early_retry() {
    for header in ["36000", "18446744073709551616"] {
        let server = Server::start(vec![Reply::status(429).after(header), Reply::status(200)]);
        let (mut session, directory) = server.session();
        runtime().block_on(async move {
            let cx = Cx::current().unwrap();
            let url = format!("{}/me/tracks", session.endpoints.api);
            assert!(matches!(
                session.get_brief(&cx, &url).await,
                Err(SpotifyError::Api { status: 429, .. })
            ));
        });
        assert_eq!(server.count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn mixed_rate_limits_server_failure_and_refresh_share_one_budget() {
    let server = Server::start(vec![
        Reply::status(429).after("0"),
        Reply::status(503),
        Reply::status(401),
        Reply::token(),
        Reply::status(200),
    ]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        session.get(&cx, &url).await.unwrap();
        assert_eq!(
            session.cache.load().unwrap().unwrap().refresh_token,
            "rotated-refresh"
        );
    });
    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen.iter().filter(|seen| seen.method == "POST").count(), 1);
    assert_eq!(
        seen[4].authorization.as_deref(),
        Some("Bearer rotated-access")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_second_unauthorized_response_does_not_refresh_again() {
    let server = Server::start(vec![Reply::status(401), Reply::token(), Reply::status(401)]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert!(matches!(
            session.get(&cx, &url).await,
            Err(SpotifyError::Api { status: 401, .. })
        ));
    });
    assert_eq!(server.count(), 3);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn token_posts_are_not_replayed_after_transient_failures() {
    let server = Server::start(vec![Reply::status(503), Reply::token()]);
    let (session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        assert!(
            session
                .post_token(&cx, "grant_type=refresh_token".into())
                .await
                .is_err()
        );
    });
    assert_eq!(server.count(), 1);
    assert_eq!(server.seen.lock().unwrap()[0].method, "POST");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_stalled_response_body_is_bounded_by_the_total_deadline() {
    let server = Server::start(vec![Reply {
        stall: Duration::from_millis(300),
        ..Reply::status(200)
    }]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        let error = get_with_limit(&mut session, &cx, &url, 5, Duration::from_millis(40))
            .await
            .unwrap_err();
        assert!(matches!(error, SpotifyError::Http(_)));
    });
    assert_eq!(server.count(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn zero_total_budget_and_other_origins_send_nothing() {
    let server = Server::start(vec![Reply::status(200)]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert!(
            get_with_limit(&mut session, &cx, &url, 5, Duration::ZERO)
                .await
                .is_err()
        );
        assert!(matches!(
            session.get(&cx, "https://example.invalid/v1/me").await,
            Err(SpotifyError::Config(_))
        ));
    });
    assert_eq!(server.count(), 0);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn backoff_and_a_later_body_cannot_each_get_a_fresh_deadline() {
    let server = Server::start(vec![
        Reply::status(503),
        Reply {
            stall: Duration::from_millis(300),
            ..Reply::status(200)
        },
    ]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        let error = get_with_limit(&mut session, &cx, &url, 5, Duration::from_millis(400))
            .await
            .unwrap_err();
        assert!(matches!(error, SpotifyError::Http(_)));
    });
    assert_eq!(server.count(), 2);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn reactive_refresh_is_inside_the_same_read_deadline() {
    let server = Server::start(vec![
        Reply::status(401),
        Reply {
            stall: Duration::from_millis(300),
            ..Reply::token()
        },
        Reply::status(200),
    ]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert!(
            get_with_limit(&mut session, &cx, &url, 5, Duration::from_millis(80))
                .await
                .is_err()
        );
        assert_eq!(
            session.cache.load().unwrap().unwrap().refresh_token,
            "test-refresh"
        );
    });
    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].method, "POST");
    assert_eq!(seen[1].target, "/api/token");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_failed_read_does_not_poison_later_reads_on_the_session() {
    let mut replies: Vec<_> = (0..4).map(|_| Reply::status(503)).collect();
    replies.push(Reply::status(200));
    let server = Server::start(replies);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert!(session.get(&cx, &url).await.is_err());
        assert_eq!(session.get(&cx, &url).await.unwrap(), b"{\"ok\":true}");
    });
    assert_eq!(server.count(), 5);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_long_rate_limit_blocks_other_endpoints_without_refreshing() {
    let server = Server::start(vec![Reply::status(429).after("60"), Reply::status(200)]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert!(matches!(
            session.get_brief(&cx, &url).await,
            Err(SpotifyError::Api { status: 429, .. })
        ));
        // A known cooldown should not even rotate an expired token.
        session.token.as_mut().unwrap().expires_at = 0;
        for path in ["me/albums", "artists/fixture"] {
            let url = format!("{}/{path}", session.endpoints.api);
            assert!(matches!(
                session.get_brief(&cx, &url).await,
                Err(SpotifyError::Api { status: 429, .. })
            ));
        }
        assert!(matches!(
            session.check_grant(&cx).await,
            Err(SpotifyError::Api { status: 429, .. })
        ));
    });
    assert_eq!(server.count(), 1, "no GET or token POST during cooldown");
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn cooldown_expiration_uses_elapsed_time_without_overflow_or_clock_reinterpretation() {
    let mut limit = Cooldown::default();
    limit.record(Some("10"), 0);
    let started = limit.limited.unwrap().0;
    assert_eq!(limit.remaining_at(started), Duration::from_secs(10));
    assert_eq!(
        limit.remaining_at(started + Duration::from_secs(4)),
        Duration::from_secs(6)
    );
    assert_eq!(
        limit.remaining_at(started + Duration::from_secs(10)),
        Duration::ZERO
    );
    limit.record(Some("18446744073709551616"), 0);
    assert_eq!(limit.limited.unwrap().1, Duration::MAX);
    limit.record(None, 0);
    assert_eq!(limit.limited.unwrap().1, Duration::from_secs(1));
    limit.record(Some("0"), 0);
    assert!(limit.limited.is_none());
}

#[test]
fn a_later_read_waits_out_a_short_cooldown_before_sending() {
    let server = Server::start(vec![Reply::status(429).after("1"), Reply::status(200)]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        assert!(session.check_grant(&cx).await.is_err());
        let url = format!("{}/me/albums", session.endpoints.api);
        session.get_brief(&cx, &url).await.unwrap();
        assert!(session.api_cooldown.limited.is_none());
    });
    let seen = server.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].target, "/v1/me/albums");
    assert!(seen[1].at.duration_since(seen[0].at) >= Duration::from_secs(1));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dropping_a_wait_preserves_the_limit_for_the_next_caller() {
    let server = Server::start(vec![Reply::status(429).after("1"), Reply::status(200)]);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        assert!(session.check_grant(&cx).await.is_err());
        let url = format!("{}/me/albums", session.endpoints.api);
        let pending = session.get(&cx, &url);
        assert!(
            timeout(now(&cx), Duration::from_millis(20), std::pin::pin!(pending))
                .await
                .is_err()
        );
        assert!(matches!(
            session.check_grant(&cx).await,
            Err(SpotifyError::Api { status: 429, .. })
        ));
    });
    assert_eq!(server.count(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn retry_after_on_the_last_attempt_still_gates_the_next_read() {
    let mut replies: Vec<_> = (0..3).map(|_| Reply::status(503)).collect();
    replies.push(Reply::status(429).after("60"));
    let server = Server::start(replies);
    let (mut session, directory) = server.session();
    runtime().block_on(async move {
        let cx = Cx::current().unwrap();
        let url = format!("{}/me/tracks", session.endpoints.api);
        assert!(matches!(
            session.get(&cx, &url).await,
            Err(SpotifyError::Api { status: 429, .. })
        ));
        assert!(matches!(
            session.get_brief(&cx, &url).await,
            Err(SpotifyError::Api { status: 429, .. })
        ));
    });
    assert_eq!(server.count(), 4);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn changing_the_api_endpoint_does_not_transfer_an_old_peers_rate_limit() {
    let server = Server::start(vec![Reply::status(200)]);
    let (mut session, directory) = server.session();
    session.api_cooldown.record(Some("60"), 0);
    let mut endpoints = session.endpoints().clone();
    session = session.with_endpoints(endpoints.clone());
    assert!(
        session.api_cooldown.limited.is_some(),
        "same API keeps its limit"
    );
    endpoints.api.push_str("/alternate");
    session = session.with_endpoints(endpoints);
    assert!(session.api_cooldown.limited.is_none());
    assert_eq!(server.count(), 0);
    std::fs::remove_dir_all(directory).unwrap();
}
