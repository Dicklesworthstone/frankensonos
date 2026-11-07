//! Real sockets prove that an expired caller cannot leave a queued SOAP
//! mutation behind. The held exchange gives the test a deterministic backlog.

use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};

const BUDGET: Duration = Duration::from_millis(500);
const WAIT: Duration = Duration::from_secs(3);

struct Server {
    addr: SocketAddr,
    seen: mpsc::Receiver<String>,
    release: mpsc::Sender<()>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (seen_tx, seen) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                stream.set_read_timeout(Some(WAIT)).unwrap();
                let Some(path) = request_path(&stream) else {
                    continue;
                };
                seen_tx.send(path.clone()).unwrap();
                if path == "/held" {
                    // Also bounded when a failed assertion unwinds the caller.
                    let _ = held.recv_timeout(Duration::from_secs(10));
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });
        Self {
            addr,
            seen,
            release,
            stop,
            worker: Some(worker),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.release.send(());
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn request_path(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut size = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            size = value.trim().parse().ok()?;
        }
    }
    reader.read_exact(&mut vec![0; size]).ok()?;
    Some(path)
}

#[test]
fn queued_requests_expire_without_sending_a_late_write() {
    let slow = Server::start();
    let healthy = Server::start();
    let player: IpAddr = "192.0.2.10".parse().unwrap();
    let lan = Lan::with_timeout(BUDGET)
        .unwrap()
        .with_routes(vec![(player, slow.addr)]);

    thread::scope(|scope| {
        let lan = &lan;
        // Hold the same player's lane past the queued requests' deadlines.
        // An explicit longer first budget makes the ordering deterministic;
        // the calls behind it use the public transport's normal budget.
        let first_deadline = Instant::now() + Duration::from_secs(10);
        let lane = slow.addr.to_string();
        let held_url = slow.url("/held");
        let first = scope.spawn(move || {
            lan.submit(&lane, Some(first_deadline), |reply| Job::Http {
                method: Method::Get,
                url: held_url,
                headers: Vec::new(),
                body: Vec::new(),
                deadline: first_deadline,
                reply,
            })
        });
        assert_eq!(slow.seen.recv_timeout(WAIT).unwrap(), "/held");

        let (finished, returned) = mpsc::channel();
        let write_finished = finished.clone();
        let write = scope.spawn(move || {
            let result = lan.soap_post(
                player,
                "/expired-write",
                "\"urn:example:service#SetVolume\"",
                "<DesiredVolume>80</DesiredVolume>",
            );
            let _ = write_finished.send(());
            result
        });
        let read_url = slow.url("/expired-read");
        let read = scope.spawn(move || {
            let result = lan.http_get(&read_url);
            let _ = finished.send(());
            result
        });

        // A different player stays usable while this one is blocked.
        let healthy_result = lan.http_get(&healthy.url("/healthy"));
        let both_returned =
            returned.recv_timeout(WAIT).is_ok() && returned.recv_timeout(WAIT).is_ok();
        // Release before asserting so a broken implementation is cleaned up.
        slow.release.send(()).unwrap();
        let first_result = first.join().unwrap();
        let write_result = write.join().unwrap();
        let read_result = read.join().unwrap();
        let fresh_result = lan.http_get(&slow.url("/fresh"));

        assert!(
            both_returned,
            "queued callers waited for the earlier exchange"
        );
        assert_eq!(healthy_result.unwrap(), "ok");
        assert!(first_result.is_ok());
        assert!(matches!(write_result, Err(ProtoError::Network { .. })));
        assert!(matches!(read_result, Err(ProtoError::Network { .. })));
        assert_eq!(fresh_result.unwrap(), "ok");
        assert_eq!(slow.seen.try_iter().collect::<Vec<_>>(), ["/fresh"]);
        assert_eq!(healthy.seen.try_iter().collect::<Vec<_>>(), ["/healthy"]);
    });
}

#[test]
fn admission_wait_expires_without_enqueuing_a_late_write() {
    let server = Server::start();
    let player: IpAddr = "192.0.2.10".parse().unwrap();
    let lan = Lan::with_timeout(BUDGET)
        .unwrap()
        .with_routes(vec![(player, server.addr)]);

    thread::scope(|scope| {
        let lan = &lan;
        let admission = lan.lanes.lock().unwrap();
        let (finished, returned) = mpsc::channel();
        let write = scope.spawn(move || {
            let result = lan.soap_post(player, "/expired-write", "\"urn:x#Play\"", "<env/>");
            let _ = finished.send(());
            result
        });
        let returned_while_locked = returned.recv_timeout(WAIT).is_ok();
        drop(admission);
        let write_result = write.join().unwrap();
        let fresh_result = lan.http_get(&server.url("/fresh"));

        assert!(
            returned_while_locked,
            "admission ignored the request deadline"
        );
        assert!(matches!(write_result, Err(ProtoError::Network { .. })));
        assert_eq!(fresh_result.unwrap(), "ok");
        assert_eq!(server.seen.try_iter().collect::<Vec<_>>(), ["/fresh"]);
    });
}
