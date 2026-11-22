//! Held localhost replies make overlap and cancellation observable without
//! depending on public hosts or measuring a discovery pass against a stopwatch.

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};

const WAIT: Duration = Duration::from_secs(10);

struct Server {
    addr: SocketAddr,
    release: mpsc::Sender<()>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn start(index: usize, status: u16, held: bool, seen: mpsc::Sender<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (release, permit) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                stream.set_read_timeout(Some(WAIT)).unwrap();
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                assert_eq!(line, "GET /xml/device_description.xml HTTP/1.1\r\n");
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let _ = seen.send(index);
                if held {
                    let _ = permit.recv_timeout(WAIT);
                }
                let body = format!("description-{index}");
                let response = format!(
                    "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            addr,
            release,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.release.send(());
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn routed(servers: &[Server]) -> (Arc<Lan>, Vec<String>) {
    let addresses: Vec<_> = (0..servers.len())
        .map(|index| IpAddr::V4(Ipv4Addr::new(192, 0, 2, u8::try_from(index + 1).unwrap())))
        .collect();
    let routes = addresses
        .iter()
        .zip(servers)
        .map(|(&ip, server)| (ip, server.addr))
        .collect();
    let urls = addresses.into_iter().map(ssdp::description_url).collect();
    (
        Arc::new(Lan::with_timeout(WAIT).unwrap().with_routes(routes)),
        urls,
    )
}

#[test]
fn slow_descriptions_do_not_serialize_healthy_or_failed_players() {
    let (seen, arrived) = mpsc::channel();
    let servers = [
        Server::start(0, 200, true, seen.clone()),
        Server::start(1, 503, false, seen.clone()),
        Server::start(2, 200, false, seen),
    ];
    let (lan, urls) = routed(&servers);
    thread::scope(|scope| {
        // The Arc forwarding must retain the LAN's concurrent implementation.
        let batch = scope.spawn(|| lan.http_get_many(&urls, &|| false));
        let mut started = Vec::new();
        for _ in 0..servers.len() {
            if let Ok(index) = arrived.recv_timeout(Duration::from_secs(3)) {
                started.push(index);
            } else {
                break;
            }
        }
        servers[0].release.send(()).unwrap();
        let results = batch.join().unwrap();
        started.sort_unstable();
        assert_eq!(
            started,
            [0, 1, 2],
            "all requests start before the held reply"
        );
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap(), "description-0");
        assert!(
            matches!(&results[1], Err(ProtoError::Network { target, detail })
            if target == &urls[1] && detail == "HTTP 503")
        );
        assert_eq!(results[2].as_ref().unwrap(), "description-2");
    });
}

#[test]
fn description_admission_is_bounded_and_cancellation_stops_the_remaining_urls() {
    let (seen, arrived) = mpsc::channel();
    let servers: Vec<_> = (0..MAX_DESCRIPTION_READS + 3)
        .map(|index| Server::start(index, 200, true, seen.clone()))
        .collect();
    let (lan, urls) = routed(&servers);
    let cancelled = AtomicBool::new(false);
    thread::scope(|scope| {
        let batch = scope.spawn(|| lan.http_get_many(&urls, &|| cancelled.load(Ordering::Acquire)));
        let mut started = Vec::new();
        for _ in 0..MAX_DESCRIPTION_READS {
            if let Ok(index) = arrived.recv_timeout(Duration::from_secs(3)) {
                started.push(index);
            } else {
                break;
            }
        }
        let extra = arrived.recv_timeout(Duration::from_millis(100)).ok();
        let completed = started.first().copied();
        if let Some(index) = completed {
            let _ = servers[index].release.send(());
        }
        let next = arrived.recv_timeout(Duration::from_secs(3)).ok();
        let beyond_window = arrived.recv_timeout(Duration::from_millis(100)).ok();
        cancelled.store(true, Ordering::Release);
        for server in &servers {
            let _ = server.release.send(());
        }
        let results = batch.join().unwrap();
        assert_eq!(started.len(), MAX_DESCRIPTION_READS);
        assert!(extra.is_none(), "more than the bounded window was admitted");
        assert!(
            next.is_some_and(|index| !started.contains(&index)),
            "one completed description admits the next URL while other players wait"
        );
        assert!(
            beyond_window.is_none(),
            "the replacement exceeded the window"
        );
        assert!(
            arrived.try_iter().next().is_none(),
            "cancelled URLs were sent"
        );
        assert_eq!(results.len(), urls.len());
        for (index, result) in results.iter().enumerate() {
            if Some(index) == completed {
                assert_eq!(result.as_ref().unwrap(), &format!("description-{index}"));
            } else {
                assert!(matches!(
                    result,
                    Err(ProtoError::NotWired("description read cancelled"))
                ));
            }
        }
    });
}

#[test]
fn an_already_cancelled_or_empty_description_batch_opens_no_connections() {
    let (seen, arrived) = mpsc::channel();
    let servers = [Server::start(0, 200, false, seen)];
    let (lan, urls) = routed(&servers);
    assert!(lan.http_get_many(&[], &|| false).is_empty());
    assert!(matches!(
        lan.http_get_many(&urls, &|| true).as_slice(),
        [Err(ProtoError::NotWired("description read cancelled"))]
    ));
    assert!(arrived.try_iter().next().is_none());
}
