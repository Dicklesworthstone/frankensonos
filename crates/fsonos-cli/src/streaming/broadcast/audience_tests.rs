//! Exact production HTTP delivery and readiness logic over real loopback sockets.

use super::*;

fn frame() -> Vec<u8> {
    let mut bytes = vec![0; frame_len(&[0xff, 0xfb, 0xb0, 0]).unwrap()];
    bytes[..4].copy_from_slice(&[0xff, 0xfb, 0xb0, 0]);
    bytes
}

fn count_becomes(audio: &Audio, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while audio.readers.load(Ordering::Acquire) != expected {
        assert!(Instant::now() < deadline, "reader count did not settle");
        thread::sleep(Duration::from_millis(5));
    }
}

struct Peer {
    client: TcpStream,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl Peer {
    fn new(audio: &Arc<Audio>, request: &[u8]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let (server, _) = listener.accept().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let audio = Arc::clone(audio);
        let worker = thread::spawn(move || serve_peer(server, "/test.mp3", &audio, &cancelled));
        client.write_all(request).unwrap();
        Self {
            client,
            stop,
            worker: Some(worker),
        }
    }

    fn read(&mut self, expected: &[u8]) {
        let mut actual = vec![0; expected.len()];
        self.client.read_exact(&mut actual).unwrap();
        assert_eq!(actual, expected);
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap().unwrap();
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.finish();
    }
}

#[test]
fn a_live_get_is_ready_only_after_a_complete_frame_is_written() {
    let audio = Arc::new(Audio::default());
    let mut peer = Peer::new(&audio, b"GET /test.mp3 HTTP/1.1\r\nHost: localhost\r\n\r\n");
    peer.read(AUDIO_HEADERS);
    assert_eq!(audio.readers.load(Ordering::Acquire), 0);
    assert_eq!(
        await_reader(&audio.readers, Duration::ZERO, || Ok(())).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    audio.publish(frame()).unwrap();
    peer.read(&frame());
    count_becomes(&audio, 1);
    await_reader(&audio.readers, Duration::ZERO, || Ok(())).unwrap();
    peer.finish();
    count_becomes(&audio, 0);
    assert!(await_reader(&audio.readers, Duration::ZERO, || Ok(())).is_err());
}

#[test]
fn head_wrong_paths_and_methods_never_count_as_audio_delivery() {
    let audio = Arc::new(Audio::default());
    audio.publish(frame()).unwrap();
    for (request, response) in [
        (b"HEAD /test.mp3 HTTP/1.1\r\n\r\n".as_slice(), AUDIO_HEADERS),
        (b"GET /other.mp3 HTTP/1.1\r\n\r\n".as_slice(), NOT_FOUND),
        (b"POST /test.mp3 HTTP/1.1\r\n\r\n".as_slice(), NOT_FOUND),
    ] {
        let mut peer = Peer::new(&audio, request);
        peer.read(response);
        peer.finish();
        assert_eq!(audio.readers.load(Ordering::Acquire), 0);
    }
}

#[test]
fn one_disconnected_reader_does_not_retire_another_live_reader() {
    let audio = Arc::new(Audio::default());
    audio.publish(frame()).unwrap();
    let mut first = Peer::new(&audio, b"GET /test.mp3 HTTP/1.1\r\n\r\n");
    first.read(AUDIO_HEADERS);
    first.read(&frame());
    let mut second = Peer::new(&audio, b"GET /test.mp3 HTTP/1.0\r\n\r\n");
    second.read(AUDIO_HEADERS);
    second.read(&frame());
    count_becomes(&audio, 2);
    first.finish();
    count_becomes(&audio, 1);
    await_reader(&audio.readers, Duration::ZERO, || Ok(())).unwrap();
    second.finish();
    count_becomes(&audio, 0);
}

#[test]
fn encoder_eof_without_audio_never_authorizes_launch() {
    let audio = Arc::new(Audio::default());
    let mut peer = Peer::new(&audio, b"GET /test.mp3 HTTP/1.1\r\n\r\n");
    peer.read(AUDIO_HEADERS);
    audio.finish().unwrap();
    peer.finish();
    assert_eq!(audio.readers.load(Ordering::Acquire), 0);
}

#[test]
fn delivery_guard_counts_once_and_clears_on_unwind() {
    let count = AtomicUsize::new(0);
    drop(Reader::new(&count));
    assert_eq!(count.load(Ordering::Acquire), 0);
    let result = std::panic::catch_unwind(|| {
        let mut reader = Reader::new(&count);
        reader.delivered();
        reader.delivered();
        assert_eq!(count.load(Ordering::Acquire), 1);
        panic!("injected reader failure");
    });
    assert!(result.is_err());
    assert_eq!(count.load(Ordering::Acquire), 0);
}

#[test]
fn cancellation_and_dead_encoder_win_over_existing_reader_evidence() {
    let count = AtomicUsize::new(1);
    for kind in [io::ErrorKind::Interrupted, io::ErrorKind::BrokenPipe] {
        let error = await_reader(&count, Duration::from_secs(1), || Err(kind.into()))
            .unwrap_err();
        assert_eq!(error.kind(), kind);
    }
}

#[test]
fn readiness_wait_has_a_finite_deadline_and_rechecks_health() {
    let count = AtomicUsize::new(0);
    let mut calls = 0;
    let started = Instant::now();
    let error = await_reader(&count, Duration::from_millis(25), || {
        calls += 1;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(calls >= 1);
    assert!(started.elapsed() < Duration::from_secs(2));
    let mut checks = 0;
    await_reader(&count, Duration::from_secs(3), || {
        checks += 1;
        if checks == 2 {
            count.store(1, Ordering::Release);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(checks, 2);
}
