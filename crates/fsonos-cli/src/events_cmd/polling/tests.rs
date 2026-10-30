use super::*;
use fsonos_api::events::EventDto;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

const TEST_WAIT: Duration = Duration::from_secs(5);

fn state(id: u64) -> EventDto {
    EventDto {
        id,
        event: "zone.state".into(),
        data: json!({ "room": "Kitchen", "volume": id }),
    }
}

fn batch(ids: &[u64], last_id: u64, reset: Option<u64>) -> EventsDto {
    EventsDto {
        events: ids.iter().copied().map(state).collect(),
        last_id,
        reset,
    }
}

/// Exercise the follower against actual TCP replies, including a connection
/// lost before its answer. The HTTP/HTTPS adapter itself also has CLI e2e
/// coverage; this fixture deliberately supplies only the bounded read boundary.
fn server(replies: Vec<Option<EventsDto>>) -> (SocketAddr, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + TEST_WAIT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "event request never arrived");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept event request: {error}"),
                }
            };
            stream.set_read_timeout(Some(TEST_WAIT)).unwrap();
            stream.set_write_timeout(Some(TEST_WAIT)).unwrap();
            let mut request = BufReader::new(&mut stream);
            let mut line = String::new();
            request.read_line(&mut line).unwrap();
            requests.push(line.trim_end().to_owned());
            loop {
                line.clear();
                assert!(request.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            drop(request);
            if let Some(reply) = reply {
                let body = serde_json::to_string(&reply).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                )
                .unwrap();
            }
        }
        requests
    });
    (addr, task)
}

fn read(addr: SocketAddr, since: Option<u64>, wait: Duration) -> Result<EventsDto, Failure> {
    let gone = |error| Failure::new(ErrorCode::NotReady, format!("{error}"));
    let mut stream = TcpStream::connect_timeout(&addr, TEST_WAIT).map_err(gone)?;
    stream.set_read_timeout(Some(TEST_WAIT)).map_err(gone)?;
    stream.set_write_timeout(Some(TEST_WAIT)).map_err(gone)?;
    let since = since.map_or_else(String::new, |id| format!("&since={id}"));
    write!(
        stream,
        "GET /events?wait={}{since} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n",
        wait.as_secs()
    )
    .map_err(gone)?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).map_err(gone)?;
    let body = answer
        .split_once("\r\n\r\n")
        .ok_or_else(|| gone(io::Error::from(io::ErrorKind::UnexpectedEof)))?
        .1;
    serde_json::from_str(body).map_err(|error| Failure::new(ErrorCode::Internal, error.to_string()))
}

#[test]
fn loopback_reads_keep_the_cursor_through_quiet_retry_and_daemon_restart() {
    let (addr, served) = server(vec![
        Some(batch(&[], 41, None)),
        None,
        Some(batch(&[42, 43], 43, None)),
        Some(batch(&[], 0, Some(1))),
        Some(batch(&[1], 1, None)),
    ]);
    let mut seen = Vec::new();
    follow(
        move |since, wait| read(addr, since, wait),
        None,
        Arc::new(AtomicBool::new(false)),
        |event| {
            seen.push(event);
            Ok(seen.len() < 4)
        },
        &[Duration::ZERO],
    )
    .unwrap();
    assert_eq!(
        served.join().unwrap(),
        [
            "GET /events?wait=0 HTTP/1.1",
            "GET /events?wait=15&since=41 HTTP/1.1",
            "GET /events?wait=15&since=41 HTTP/1.1",
            "GET /events?wait=15&since=43 HTTP/1.1",
            "GET /events?wait=15&since=0 HTTP/1.1",
        ]
    );
    assert_eq!(
        seen.iter().map(|event| event.id).collect::<Vec<_>>(),
        [Some(42), Some(43), None, Some(1)]
    );
    assert_eq!(seen[2].kind, "events.reset");
    assert_eq!(seen[2].data, json!({ "oldest": 1 }));
}

#[test]
fn empty_polls_retain_the_same_cursor_until_a_change_arrives() {
    let asks = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&asks);
    let mut replies = [
        batch(&[], 5, None),
        batch(&[], 5, None),
        batch(&[6], 6, None),
    ]
    .into_iter();
    let mut seen = Vec::new();
    follow(
        move |since, wait| {
            recorded.lock().unwrap().push((since, wait));
            Ok(replies.next().unwrap())
        },
        Some(5),
        Arc::new(AtomicBool::new(false)),
        |event| {
            seen.push(event);
            Ok(false)
        },
        &[],
    )
    .unwrap();
    assert_eq!(*asks.lock().unwrap(), vec![(Some(5), WAIT); 3]);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, Some(6));
}

#[test]
fn a_gap_is_reported_before_retained_events_and_counts_as_one_output() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let mut seen = Vec::new();
    follow(
        move |_, _| {
            counted.fetch_add(1, Ordering::AcqRel);
            Ok(batch(&[8, 9], 9, Some(8)))
        },
        Some(2),
        Arc::new(AtomicBool::new(false)),
        |event| {
            seen.push(event);
            Ok(false)
        },
        &[],
    )
    .unwrap();
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, None);
    assert_eq!(seen[0].kind, "events.reset");
    assert_eq!(seen[0].data, json!({ "oldest": 8 }));
}

#[test]
fn nonretryable_failures_stop_without_another_request_or_output() {
    for code in [
        ErrorCode::PolicyDenied,
        ErrorCode::NotImplemented,
        ErrorCode::Internal,
    ] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let failure = follow(
            move |_, _| {
                counted.fetch_add(1, Ordering::AcqRel);
                Err(Failure::new(code, "read refused"))
            },
            Some(7),
            Arc::new(AtomicBool::new(false)),
            |_| panic!("a refused read prints no event"),
            &[Duration::ZERO; 3],
        )
        .unwrap_err();
        assert_eq!(failure.downcast_ref::<Failure>().unwrap().code, code);
        assert_eq!(attempts.load(Ordering::Acquire), 1);
    }
}

#[test]
fn exhausted_read_retries_preserve_the_last_cursor_and_resume_hint() {
    let asks = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&asks);
    let failure = follow(
        move |since, _| {
            recorded.lock().unwrap().push(since);
            Err(Failure::new(ErrorCode::NotReady, "connection dropped"))
        },
        Some(27),
        Arc::new(AtomicBool::new(false)),
        |_| panic!("a failed read prints no event"),
        &[Duration::ZERO; 3],
    )
    .unwrap_err();
    assert_eq!(*asks.lock().unwrap(), vec![Some(27); 4]);
    let failure = failure.downcast_ref::<Failure>().unwrap();
    assert_eq!(failure.code, ErrorCode::NotReady);
    assert!(failure.hint.contains("--since 27"));
    assert!(failure.hint.contains("--daemon-url"));
}

#[test]
fn a_successful_read_replenishes_the_consecutive_retry_budget() {
    let mut replies = [
        Err(Failure::new(ErrorCode::NotReady, "first outage")),
        Ok(batch(&[2], 2, None)),
        Err(Failure::new(ErrorCode::NotReady, "later outage")),
        Ok(batch(&[3], 3, None)),
    ]
    .into_iter();
    let mut seen = Vec::new();
    follow(
        move |_, _| replies.next().unwrap(),
        Some(1),
        Arc::new(AtomicBool::new(false)),
        |event| {
            seen.push(event.id);
            Ok(seen.len() < 2)
        },
        &[Duration::ZERO],
    )
    .unwrap();
    assert_eq!(seen, [Some(2), Some(3)]);
}

#[test]
fn stopping_a_waiting_reader_returns_before_the_network_request_finishes() {
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let (entered, began) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let (completed, done) = mpsc::sync_channel(1);
    let (finished, exited) = mpsc::sync_channel(1);
    let caller = thread::spawn(move || {
        let result = follow(
            move |_, _| {
                entered.send(()).unwrap();
                released.recv_timeout(TEST_WAIT).unwrap();
                completed.send(()).unwrap();
                Ok(batch(&[2], 2, None))
            },
            Some(1),
            stopping,
            |_| panic!("a cancelled read prints no event"),
            &[],
        );
        finished.send(result).unwrap();
    });
    began.recv_timeout(TEST_WAIT).unwrap();
    stop.store(true, Ordering::Release);
    let result = exited.recv_timeout(Duration::from_secs(1));
    // Always release the injected I/O before asserting the cancellation result.
    release.send(()).unwrap();
    done.recv_timeout(TEST_WAIT).unwrap();
    caller.join().unwrap();
    assert!(result.is_ok_and(|result| result.is_ok()));
}

#[test]
fn cancellation_before_reading_never_contacts_the_daemon() {
    follow(
        |_, _| panic!("cancelled before the first request"),
        None,
        Arc::new(AtomicBool::new(true)),
        |_| panic!("cancelled before any output"),
        &[],
    )
    .unwrap();
}

#[test]
fn retry_backoff_is_interruptible() {
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let (finished, done) = mpsc::sync_channel(1);
    let waiter = thread::spawn(move || {
        finished
            .send(wait(Duration::from_secs(30), &stopping))
            .unwrap();
    });
    stop.store(true, Ordering::Release);
    assert!(!done.recv_timeout(Duration::from_secs(1)).unwrap());
    waiter.join().unwrap();
}

#[test]
fn malformed_cursors_fail_before_printing_a_reset_or_any_event() {
    for reply in [
        batch(&[], 3, None),
        batch(&[], 6, None),
        batch(&[5, 5], 5, None),
        batch(&[5], 6, None),
        batch(&[5], 5, Some(0)),
        batch(&[], 4, Some(8)),
    ] {
        let failure = follow(
            move |_, _| Ok(reply.clone()),
            Some(4),
            Arc::new(AtomicBool::new(false)),
            |_| panic!("inconsistent response must not publish partial events"),
            &[],
        )
        .unwrap_err();
        assert_eq!(
            failure.downcast_ref::<Failure>().unwrap().code,
            ErrorCode::Internal
        );
    }
}

#[test]
fn a_writer_error_ends_the_follower_without_prefetching() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let failure = follow(
        move |_, _| {
            counted.fetch_add(1, Ordering::AcqRel);
            Ok(batch(&[2, 3], 3, None))
        },
        Some(1),
        Arc::new(AtomicBool::new(false)),
        |_| Err(io::ErrorKind::PermissionDenied.into()),
        &[],
    )
    .unwrap_err();
    assert_eq!(
        failure.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(attempts.load(Ordering::Acquire), 1);
}

#[cfg(unix)]
struct Child(std::process::Child);

#[cfg(unix)]
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[test]
fn real_terminal_signals_stop_a_follower_with_an_unanswered_read() {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};

    for signal in ["INT", "TERM"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut child = Child(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "events_cmd::polling::tests::signal_fixture",
                ])
                .env(
                    "FSONOS_EVENTS_SIGNAL_FIXTURE",
                    listener.local_addr().unwrap().to_string(),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + TEST_WAIT;
        let mut peer = loop {
            match listener.accept() {
                Ok((peer, _)) => break peer,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        child.0.try_wait().unwrap().is_none(),
                        "fixture exited before its read"
                    );
                    assert!(Instant::now() < deadline, "fixture did not start its read");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept fixture: {error}"),
            }
        };
        peer.set_read_timeout(Some(TEST_WAIT)).unwrap();
        let mut ready = [0];
        peer.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [1]);
        assert!(
            Command::new("/bin/kill")
                .args(["-s", signal, "--", &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        let result = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(5));
        };
        // Keep the peer open and unanswered until the CLI has exited: its
        // signal handler must wake the caller independently of request I/O.
        drop(peer);
        assert!(
            result.is_some_and(|status| status.success()),
            "{signal}: {result:?}"
        );
    }
}

#[cfg(unix)]
#[test]
#[ignore = "self-spawned isolated signal fixture"]
fn signal_fixture() {
    let addr: SocketAddr = std::env::var("FSONOS_EVENTS_SIGNAL_FIXTURE")
        .unwrap()
        .parse()
        .unwrap();
    run(
        move |_, _| {
            let mut peer = TcpStream::connect_timeout(&addr, TEST_WAIT).unwrap();
            peer.set_read_timeout(Some(TEST_WAIT)).unwrap();
            peer.write_all(&[1]).unwrap();
            let mut response = [0];
            let _ = peer.read_exact(&mut response);
            Err(Failure::new(ErrorCode::NotReady, "unanswered fixture read"))
        },
        Some(0),
        |_| panic!("the fixture produces no events"),
    )
    .unwrap();
}
