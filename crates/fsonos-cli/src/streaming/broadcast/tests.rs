use super::{Audio, LiveStream, MAX_FRAMES, frame_len, frames, serve};
use std::io::{self, BufRead as _, BufReader, Cursor, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn frame(value: u8) -> Vec<u8> {
    let mut bytes = vec![value; frame_len(&[0xff, 0xfb, 0xb0, 0]).unwrap()];
    bytes[..4].copy_from_slice(&[0xff, 0xfb, 0xb0, 0]);
    bytes
}

#[test]
fn frame_headers_enforce_the_encoder_contract() {
    assert_eq!(frame_len(&[0xff, 0xfb, 0xb0, 0]), Some(626));
    assert_eq!(frame_len(&[0xff, 0xfb, 0xb2, 0]), Some(627));
    for header in [
        [0xff, 0xfb, 0x00, 0], // free bitrate
        [0xff, 0xfb, 0xf0, 0], // reserved bitrate
        [0xff, 0xfb, 0xb4, 0], // wrong sample rate
        [0xff, 0xf3, 0xb0, 0], // MPEG-2
        [0xff, 0xfd, 0xb0, 0], // Layer II
        [0x00, 0xfb, 0xb0, 0],
    ] {
        assert_eq!(frame_len(&header), None);
    }
    assert_eq!(frame_len(&[0xff, 0xfb, 0xb0]), None);
}

#[test]
fn every_split_of_a_frame_is_reassembled_without_partial_publication() {
    let expected = frame(17);
    for split in 0..=expected.len() {
        let mut pending = Vec::new();
        let mut output = Vec::new();
        frames(&mut pending, &expected[..split], |value| {
            output.push(value);
            Ok(())
        })
        .unwrap();
        assert_eq!(output.len(), usize::from(split == expected.len()));
        frames(&mut pending, &expected[split..], |value| {
            output.push(value);
            Ok(())
        })
        .unwrap();
        assert_eq!(output, vec![expected.clone()]);
        assert!(pending.is_empty());
    }
}

#[test]
fn garbage_does_not_grow_the_assembler_and_frames_keep_their_boundaries() {
    let mut pending = Vec::new();
    for _ in 0..64 {
        frames(&mut pending, &[0; 8192], |_| panic!("not MPEG audio")).unwrap();
        assert!(pending.len() <= 3);
    }
    let expected = vec![frame(31), frame(63), frame(127)];
    let mut output = Vec::new();
    for chunk in expected.concat().chunks(7) {
        frames(&mut pending, chunk, |value| {
            output.push(value);
            Ok(())
        })
        .unwrap();
    }
    assert_eq!(output, expected);
    assert!(pending.is_empty());
}

#[test]
fn capture_drains_without_listeners_and_retains_only_a_bounded_live_tail() {
    let audio = Audio::default();
    let stop = AtomicBool::new(false);
    let input: Vec<u8> = (0..300u16)
        .flat_map(|index| frame(u8::try_from(index % 256).unwrap()))
        .collect();
    audio.ingest(Cursor::new(input), &stop).unwrap();
    {
        let state = audio.state.lock().unwrap();
        assert_eq!(state.next, 300);
        assert_eq!(state.frames.len(), MAX_FRAMES);
    }
    let mut cursor = audio.subscribe().unwrap();
    assert_eq!(cursor, 299);
    assert_eq!(
        audio.next(&mut cursor, &stop).unwrap().unwrap().as_ref(),
        frame(43)
    );
    assert!(audio.next(&mut cursor, &stop).unwrap().is_none());
    assert_eq!(
        audio.next(&mut 0, &stop).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

#[test]
fn a_slow_reader_never_blocks_or_consumes_another_readers_audio() {
    let audio = Audio::default();
    let stop = AtomicBool::new(false);
    audio.publish(frame(1)).unwrap();
    let mut slow = audio.subscribe().unwrap();
    for _ in 0..MAX_FRAMES {
        audio.publish(frame(2)).unwrap();
    }
    assert_eq!(
        audio.next(&mut slow, &stop).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    let mut first = audio.subscribe().unwrap();
    let mut second = audio.subscribe().unwrap();
    assert_eq!(
        audio.next(&mut first, &stop).unwrap(),
        audio.next(&mut second, &stop).unwrap()
    );
    audio.publish(frame(3)).unwrap();
    assert_eq!(
        audio.next(&mut first, &stop).unwrap().unwrap().as_ref(),
        frame(3)
    );
    assert_eq!(
        audio.next(&mut second, &stop).unwrap().unwrap().as_ref(),
        frame(3)
    );
}

#[test]
fn startup_requires_live_complete_audio_and_respects_cancellation() {
    let audio = Audio::default();
    let stop = AtomicBool::new(false);
    assert_eq!(
        audio.wait_ready(&stop, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    audio.publish(frame(1)).unwrap();
    audio.wait_ready(&stop, Duration::ZERO).unwrap();
    audio.finish().unwrap();
    assert_eq!(
        audio.wait_ready(&stop, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    stop.store(true, Ordering::Release);
    assert_eq!(
        audio.wait_ready(&stop, Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn truncated_output_is_not_published_as_a_frame() {
    let audio = Audio::default();
    let stop = AtomicBool::new(false);
    let bytes = frame(1);
    assert_eq!(
        audio
            .ingest(&bytes[..bytes.len() - 1], &stop)
            .unwrap_err()
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(audio.state.lock().unwrap().frames.is_empty());
    assert!(audio.state.lock().unwrap().ended);
}

#[test]
fn input_failure_wakes_startup_instead_of_hanging() {
    struct Failed;
    impl io::Read for Failed {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
    let audio = Arc::new(Audio::default());
    let stop = Arc::new(AtomicBool::new(false));
    let waiting = Arc::clone(&audio);
    let cancelled = Arc::clone(&stop);
    let waiter = thread::spawn(move || waiting.wait_ready(&cancelled, Duration::from_secs(2)));
    assert_eq!(
        audio.ingest(Failed, &stop).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        waiter.join().unwrap().unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}

struct Server {
    address: std::net::SocketAddr,
    audio: Arc<Audio>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let audio = Arc::new(Audio::default());
        let stop = Arc::new(AtomicBool::new(false));
        let source = Arc::clone(&audio);
        let cancelled = Arc::clone(&stop);
        let worker =
            thread::spawn(move || serve(&listener, "/stream/test.mp3", &source, &cancelled));
        Self {
            address,
            audio,
            stop,
            worker: Some(worker),
        }
    }

    fn connect(&self, method: &str, path: &str) -> BufReader<TcpStream> {
        let mut peer = TcpStream::connect(self.address).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        write!(peer, "{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        BufReader::new(peer)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn headers(peer: &mut BufReader<TcpStream>) -> String {
    let mut output = String::new();
    loop {
        let mut line = String::new();
        assert!(peer.read_line(&mut line).unwrap() > 0);
        output.push_str(&line);
        if line == "\r\n" {
            return output;
        }
    }
}

fn receive_frame(peer: &mut BufReader<TcpStream>, expected: &[u8]) {
    let mut bytes = vec![0; expected.len()];
    peer.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, expected);
}

#[test]
fn head_and_wrong_paths_do_not_consume_audio_or_expose_a_write_surface() {
    let server = Server::start();
    server.audio.publish(frame(5)).unwrap();
    for (method, path, status) in [
        ("HEAD", "/stream/test.mp3", "200 OK"),
        ("GET", "/stream/guess.mp3", "404 Not Found"),
        ("POST", "/stream/test.mp3", "404 Not Found"),
    ] {
        let mut peer = server.connect(method, path);
        let mut response = String::new();
        peer.read_to_string(&mut response).unwrap();
        assert!(response.starts_with(&format!("HTTP/1.1 {status}\r\n")));
        assert!(response.ends_with("\r\n\r\n"));
    }
    let mut peer = server.connect("GET", "/stream/test.mp3");
    assert!(headers(&mut peer).starts_with("HTTP/1.1 200 OK"));
    receive_frame(&mut peer, &frame(5));
}

#[test]
fn a_reconnecting_reader_can_join_while_the_old_connection_is_still_open() {
    let server = Server::start();
    server.audio.publish(frame(1)).unwrap();
    let mut first = server.connect("GET", "/stream/test.mp3");
    headers(&mut first);
    receive_frame(&mut first, &frame(1));
    server.audio.publish(frame(2)).unwrap();
    let mut second = server.connect("GET", "/stream/test.mp3");
    headers(&mut second);
    receive_frame(&mut second, &frame(2));
    receive_frame(&mut first, &frame(2));
    server.audio.publish(frame(3)).unwrap();
    receive_frame(&mut first, &frame(3));
    receive_frame(&mut second, &frame(3));
}

#[test]
fn partial_http_requests_do_not_block_other_clients_or_shutdown() {
    let mut server = Server::start();
    let mut partial = TcpStream::connect(server.address).unwrap();
    partial
        .write_all(b"GET /stream/test.mp3 HTTP/1.1\r\nHost:")
        .unwrap();
    let mut healthy = server.connect("HEAD", "/stream/test.mp3");
    assert!(headers(&mut healthy).starts_with("HTTP/1.1 200 OK"));
    server.stop.store(true, Ordering::Release);
    let worker = server.worker.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !worker.is_finished() {
        assert!(
            Instant::now() < deadline,
            "partial request held shutdown open"
        );
        thread::sleep(Duration::from_millis(10));
    }
    worker.join().unwrap().unwrap();
}

#[test]
#[ignore = "child process fixture; launched only by the lifecycle tests"]
fn encoder_fixture() {
    let Ok(mode) = std::env::var("FSONOS_STREAM_TEST_ENCODER") else {
        return;
    };
    if mode == "exit" {
        return;
    }
    let mut stdout = io::stdout().lock();
    loop {
        if mode == "audio" {
            stdout.write_all(&frame(42)).unwrap();
            stdout.flush().unwrap();
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn encoder(mode: &str, stdout: Stdio) -> Child {
    // libtest names omit the crate prefix included by module_path!().
    let (_, module) = module_path!().split_once("::").unwrap();
    let fixture = format!("{module}::encoder_fixture");
    Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", &fixture, "--nocapture"])
        .env("FSONOS_STREAM_TEST_ENCODER", mode)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn live(mode: &str) -> LiveStream {
    LiveStream::start(
        encoder(mode, Stdio::piped()),
        TcpListener::bind("127.0.0.1:0").unwrap(),
        "/stream/test.mp3".into(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
}

#[test]
fn live_capture_starts_without_a_client_and_shutdown_reaps_and_joins() {
    let mut stream = live("audio");
    stream.wait_ready(Duration::from_secs(3)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while stream.audio.state.lock().unwrap().next < 8 {
        assert!(
            Instant::now() < deadline,
            "capture stalled without a client"
        );
        thread::sleep(Duration::from_millis(10));
    }
    stream.shutdown().unwrap();
    assert!(stream.encoder.try_wait().unwrap().is_some());
    assert!(stream.producer.is_none());
    assert!(stream.server.is_none());
    stream.shutdown().unwrap();
}

#[test]
fn silent_encoder_deadline_and_drop_unblock_a_pipe_read() {
    let mut stream = live("silent");
    assert_eq!(
        stream
            .wait_ready(Duration::from_millis(30))
            .unwrap_err()
            .kind(),
        io::ErrorKind::TimedOut
    );
    let closer = thread::spawn(move || drop(stream));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !closer.is_finished() {
        assert!(
            Instant::now() < deadline,
            "silent encoder held cleanup open"
        );
        thread::sleep(Duration::from_millis(10));
    }
    closer.join().unwrap();
}

#[test]
fn exited_encoder_does_not_pass_startup_readiness() {
    let mut stream = live("exit");
    assert!(stream.wait_ready(Duration::from_secs(3)).is_err());
    // The fixture may leave a partial non-audio test-harness trailer. Cleanup
    // still joins both workers even when the reader reports malformed output.
    let _ = stream.shutdown();
    assert!(stream.encoder.try_wait().unwrap().is_some());
    assert!(stream.producer.is_none());
    assert!(stream.server.is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn setup_failure_after_launch_reaps_the_owned_encoder() {
    let child = encoder("silent", Stdio::null());
    let process = std::path::PathBuf::from(format!("/proc/{}", child.id()));
    let result = LiveStream::start(
        child,
        TcpListener::bind("127.0.0.1:0").unwrap(),
        "/stream/test.mp3".into(),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(result.is_err());
    assert!(
        !process.exists(),
        "setup failure left the encoder alive or a zombie"
    );
}

#[test]
fn cancellation_keeps_audio_available_until_playback_restoration_finishes() {
    let mut stream = live("audio");
    stream.wait_ready(Duration::from_secs(3)).unwrap();
    stream.cancel.store(true, Ordering::Release);
    stream.wait_until_stopped().unwrap();
    assert_eq!(
        stream.check().unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(!stream.stop.load(Ordering::Acquire));
    assert!(stream.encoder.try_wait().unwrap().is_none());
    let before = stream.audio.state.lock().unwrap().next;
    let deadline = Instant::now() + Duration::from_secs(3);
    while stream.audio.state.lock().unwrap().next == before {
        assert!(
            Instant::now() < deadline,
            "cancellation cut audio before cleanup"
        );
        thread::sleep(Duration::from_millis(10));
    }
    stream.shutdown().unwrap();
}
