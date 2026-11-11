//! Hold one real request before it reaches a simulator player. Background
//! discovery and subscriptions still run, so races are controlled at the
//! command boundary rather than by assuming the live model is idle.

use super::Scenario;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct HeldRequest {
    seen: mpsc::Sender<String>,
    release: mpsc::Receiver<()>,
}

/// A loopback proxy that holds the first request containing `needle` until
/// released. Install it before starting the client that reads the routes.
pub struct RequestGate {
    addr: SocketAddr,
    seen: mpsc::Receiver<String>,
    release: mpsc::Sender<()>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RequestGate {
    pub fn routed(s: &Scenario, room: &str, needle: &'static str) -> Self {
        let player = s
            .sim_handle()
            .and_then(|sim| sim.player(room))
            .expect("the request gate's simulator player");
        let backend = player.addr;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind request gate");
        let addr = listener.local_addr().expect("request gate address");
        let (seen_tx, seen) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let held = Arc::new(Mutex::new(Some(HeldRequest {
            seen: seen_tx,
            release: release_rx,
        })));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for stream in listener.incoming() {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let held = Arc::clone(&held);
                requests.push(thread::spawn(move || {
                    // A moved player's old listener refuses connections;
                    // closing this stream preserves that network failure.
                    let _ = forward(stream, backend, needle, &held);
                }));
            }
            for request in requests {
                let _ = request.join();
            }
        });
        let gate = Self {
            addr,
            seen,
            release,
            stop,
            worker: Some(worker),
        };
        let path = s.dir().join("routes.toml");
        let routes = fs::read_to_string(&path).expect("read simulator routes");
        let original = format!("\"{}\" = \"{backend}\"", player.ip);
        assert!(routes.contains(&original), "the original player route");
        fs::write(
            path,
            routes.replace(&original, &format!("\"{}\" = \"{addr}\"", player.ip)),
        )
        .expect("route the player through its request gate");
        gate
    }

    pub fn wait(&self, timeout: Duration) -> Option<String> {
        self.seen.recv_timeout(timeout).ok()
    }

    pub fn release(&self) {
        let _ = self.release.send(());
    }
}

impl Drop for RequestGate {
    fn drop(&mut self) {
        self.release();
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn forward(
    stream: TcpStream,
    backend: SocketAddr,
    needle: &str,
    held: &Mutex<Option<HeldRequest>>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);
    let mut request = Vec::new();
    let mut length = 0;
    loop {
        let start = request.len();
        if reader.read_until(b'\n', &mut request)? == 0 || request.len() > 64 * 1024 {
            return Err(io::Error::other("incomplete or oversized request headers"));
        }
        let line = String::from_utf8_lossy(&request[start..]);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().map_err(io::Error::other)?;
        }
    }
    if length > 1024 * 1024 {
        return Err(io::Error::other("oversized simulator request"));
    }
    let body = request.len();
    request.resize(body + length, 0);
    reader.read_exact(&mut request[body..])?;
    if String::from_utf8_lossy(&request).contains(needle) {
        let barrier = held.lock().expect("request gate").take();
        if let Some(barrier) = barrier {
            let _ = barrier
                .seen
                .send(String::from_utf8_lossy(&request).into_owned());
            barrier
                .release
                .recv_timeout(Duration::from_secs(10))
                .map_err(io::Error::other)?;
        }
    }
    let mut upstream = TcpStream::connect_timeout(&backend, Duration::from_secs(1))?;
    upstream.set_read_timeout(Some(Duration::from_secs(5)))?;
    upstream.set_write_timeout(Some(Duration::from_secs(5)))?;
    upstream.write_all(&request)?;
    // Simulator responses close the connection, just as real players do.
    io::copy(&mut upstream, &mut reader.into_inner())?;
    Ok(())
}
