//! A queued clip uses the caps in force when its household can play it.

use super::*;
use chrono::{DateTime, FixedOffset, TimeDelta};
use fsonos_core::announce::Announcer;
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::policy::Policy;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

struct SharedClock(Arc<FakeClock>);

impl Clock for SharedClock {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }
}

/// Hold the first clip after it starts, so a second call really waits in
/// the household queue. Finish each clip after observing it playing.
struct QueuedClips {
    lan: SimLan,
    in_clip: AtomicBool,
    reads: AtomicUsize,
    first: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}

impl Transport for QueuedClips {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#GetTransportInfo")
            && self.in_clip.load(Ordering::Acquire)
            && self.reads.fetch_add(1, Ordering::AcqRel) == 1
        {
            soap::stop(&self.lan, host)?;
        }
        let result = self.lan.soap_post(host, path, action, body);
        if result.is_ok() && action.contains("#SetAVTransportURI") {
            let clip = body.contains("/media/");
            self.in_clip.store(clip, Ordering::Release);
            if clip {
                self.reads.store(0, Ordering::Release);
            }
        }
        if result.is_ok()
            && action.trim_matches('"').rsplit('#').next() == Some("Play")
            && self.in_clip.load(Ordering::Acquire)
        {
            let first = self.first.lock().unwrap().take();
            if let Some((entered, release)) = first {
                entered.send(()).unwrap();
                release.recv_timeout(WAIT).unwrap();
            }
        }
        result
    }
}

fn queued_cap(s2: bool, change_policy: bool) {
    let spec = SimPlayerSpec::new(
        "Kitchen",
        if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        },
    );
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2([spec])
    } else {
        builder.s1([spec])
    }
    .spawn()
    .unwrap();
    let host = sim.player("Kitchen").unwrap().ip;
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
    let (entered, playing) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let clock = Arc::new(FakeClock::new(
        DateTime::parse_from_rfc3339("2026-10-07T21:59:00-04:00").unwrap(),
    ));
    let initial = "[defaults]\nmax_volume = 70\n\
        [quiet_hours]\nstart = \"22:00\"\nend = \"07:00\"\nmax_volume = 20\n";
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("policy.toml"), initial).unwrap();
    let mut announcements = Announcements::new(
        MediaStore::new(data.path()),
        Box::new(|| Some("http://192.0.2.250:3400".into())),
    );
    announcements.announcer =
        Announcer::with_timing(Duration::from_millis(5), Duration::from_millis(100));
    let surface = Surface::new(
        Box::new(QueuedClips {
            lan: sim.lan(),
            in_clip: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            first: Mutex::new(Some((entered, waiting))),
        }),
        Box::new(move |_| Ok(vec![house.clone()])),
        Policy::from_toml(initial).unwrap(),
        Box::new(SharedClock(Arc::clone(&clock))),
    )
    .with_policy_file(data.path().to_path_buf())
    .with_announcements(announcements);
    let request = AnnounceRequest {
        chime: Some("beep".into()),
        rooms: vec!["Kitchen".into()],
        volume: Some(60),
        ..AnnounceRequest::default()
    };
    std::thread::scope(|scope| {
        let first = scope.spawn(|| surface.announce(&Client::LoopbackHttp, &request));
        playing.recv_timeout(WAIT).unwrap();
        let second = scope.spawn(|| surface.announce(&Client::LoopbackHttp, &request));
        let deadline = Instant::now() + WAIT;
        while surface.announcements_pending() != 2 {
            assert!(
                Instant::now() < deadline,
                "second announcement did not queue"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let expected = if change_policy {
            std::fs::write(
                data.path().join("policy.toml"),
                "[defaults]\nmax_volume = 70\n[rooms.Kitchen]\nmax_volume = 12\n",
            )
            .unwrap();
            12
        } else {
            clock.advance(TimeDelta::minutes(2));
            20
        };
        release.send(()).unwrap();
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();
        assert_eq!(first.households[0].levels[0].volume, 60, "{first:?}");
        assert_eq!(
            second.households[0].levels[0].volume, expected,
            "{second:?}"
        );
        assert_eq!(second.households[0].outcome, "finished", "{second:?}");
        assert_eq!(surface.announcements_pending(), 0);
    });
    sim.shutdown();
}

#[test]
fn queued_announcements_obey_quiet_hours_when_their_turn_starts() {
    for s2 in [false, true] {
        queued_cap(s2, false);
    }
}

#[test]
fn queued_announcements_use_the_latest_room_caps() {
    for s2 in [false, true] {
        queued_cap(s2, true);
    }
}

fn finished_job(surface: &Surface, id: &str) -> AnnouncementJobDto {
    let deadline = Instant::now() + WAIT;
    loop {
        let job = surface.get_announcement(&Client::Cli, id).unwrap();
        if matches!(
            job.phase,
            AnnouncementJobPhase::Completed
                | AnnouncementJobPhase::Cancelled
                | AnnouncementJobPhase::Failed
        ) && surface.announcements_pending() == 0
        {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "announcement did not finish: {job:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_background_job_rechecks_its_creator_after_waiting_for_the_household() {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Kitchen", SimModel::One)])
        .spawn()
        .unwrap();
    let host = sim.player("Kitchen").unwrap().ip;
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
    let (entered, playing) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let initial = "[clients.mcp-stdio]\nallow = [\"announce_start\"]\n";
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("policy.toml"), initial).unwrap();
    let mut announcements = Announcements::new(
        MediaStore::new(data.path()),
        Box::new(|| Some("http://192.0.2.250:3400".into())),
    );
    announcements.announcer =
        Announcer::with_timing(Duration::from_millis(5), Duration::from_millis(100));
    let surface = Arc::new(
        Surface::new(
            Box::new(QueuedClips {
                lan: sim.lan(),
                in_clip: AtomicBool::new(false),
                reads: AtomicUsize::new(0),
                first: Mutex::new(Some((entered, waiting))),
            }),
            Box::new(move |_| Ok(vec![house.clone()])),
            Policy::from_toml(initial).unwrap(),
            Box::new(fsonos_core::clock::SystemClock),
        )
        .with_policy_file(data.path().to_path_buf())
        .with_announcements(announcements),
    );
    let request = AnnounceRequest {
        chime: Some("bell".into()),
        rooms: vec!["Kitchen".into()],
        ..AnnounceRequest::default()
    };
    std::thread::scope(|scope| {
        let first = scope.spawn(|| surface.announce(&Client::LoopbackHttp, &request));
        playing.recv_timeout(WAIT).unwrap();
        let second = surface
            .start_announcement(&Client::McpStdio, &request)
            .unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            let observed = surface.get_announcement(&Client::Cli, &second.id).unwrap();
            if observed.progress.iter().any(|row| row.phase == "queued") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "second job never reached the household wait: {observed:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(
            data.path().join("policy.toml"),
            "[clients.mcp-stdio]\nallow = [\"list_zones\"]\n",
        )
        .unwrap();
        release.send(()).unwrap();
        assert!(first.join().unwrap().unwrap().clean);
        let stopped = finished_job(&surface, &second.id);
        assert_eq!(stopped.phase, AnnouncementJobPhase::Failed, "{stopped:?}");
        let report = stopped.result.unwrap();
        assert!(!report.clean);
        assert!(
            report.households[0].levels.is_empty(),
            "a revoked queued job must not interrupt fades or prepare levels"
        );
        assert!(
            report.households[0]
                .error
                .as_deref()
                .is_some_and(|why| why.contains("announce_start")),
            "{report:?}"
        );
        assert_eq!(surface.announcements_pending(), 0);
    });
    sim.shutdown();
}

#[cfg(unix)]
#[test]
fn slow_speech_cannot_retain_a_grant_revoked_during_synthesis() {
    use fsonos_core::announce::speech::{BackendKind, SpeechConfig};
    struct NoSpeakerIo;
    impl Transport for NoSpeakerIo {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("a grant revoked during synthesis must fail before speaker I/O")
        }
    }
    for background in [false, true] {
        let operation = if background {
            "announce_start"
        } else {
            "announce"
        };
        let data = tempfile::tempdir().unwrap();
        let initial = format!("[clients.mcp-stdio]\nallow = [\"{operation}\"]\n");
        let policy = data.path().join("policy.toml");
        std::fs::write(&policy, &initial).unwrap();
        let wav = data.path().join("voice.wav");
        std::fs::write(&wav, Chime::Beep.wav()).unwrap();
        // The configured local engine revokes the test caller while rendering
        // valid WAV output; no timing-dependent race or installed TTS is needed.
        let media = MediaStore::new(data.path()).with_speech_config(SpeechConfig {
            backend: BackendKind::Command,
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat >/dev/null; printf '%s' \"$1\" >\"$2\"; cp \"$3\" \"$4\"".into(),
                "test-speech".into(),
                "[clients.mcp-stdio]\nallow = [\"list_zones\"]\n".into(),
                policy.display().to_string(),
                wav.display().to_string(),
                "{output}".into(),
            ],
            timeout_secs: 5,
            ..SpeechConfig::default()
        });
        let houses = crate::zones::fixtures::households();
        let surface = Arc::new(
            Surface::new(
                Box::new(NoSpeakerIo),
                Box::new(move |_| Ok(houses.clone())),
                Policy::from_toml(&initial).unwrap(),
                Box::new(fsonos_core::clock::SystemClock),
            )
            .with_policy_file(data.path().to_path_buf())
            .with_announcements(Announcements::new(
                media,
                Box::new(|| Some("http://192.0.2.250:3400".into())),
            )),
        );
        let request = AnnounceRequest {
            text: Some("Test announcement".into()),
            rooms: vec!["Patio".into()],
            ..AnnounceRequest::default()
        };
        if background {
            let accepted = surface
                .start_announcement(&Client::McpStdio, &request)
                .unwrap();
            let stopped = finished_job(&surface, &accepted.id);
            assert_eq!(stopped.phase, AnnouncementJobPhase::Failed);
            assert_eq!(stopped.error.unwrap().code, ErrorCode::PolicyDenied);
            assert!(stopped.result.is_none());
        } else {
            assert_eq!(
                surface
                    .announce(&Client::McpStdio, &request)
                    .unwrap_err()
                    .code,
                ErrorCode::PolicyDenied
            );
        }
        assert_eq!(surface.announcements_pending(), 0);
    }
}

#[cfg(unix)]
#[test]
fn cancelling_a_synthesizing_job_releases_its_turn_without_touching_speakers() {
    use fsonos_core::announce::speech::{BackendKind, SpeechConfig};
    struct NoSpeakerIo;
    impl Transport for NoSpeakerIo {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("speech cancellation must complete before speaker I/O")
        }
    }
    let data = tempfile::tempdir().unwrap();
    let ready = data.path().join("started");
    let media_path = data.path().join("media");
    let media = MediaStore::new(data.path()).with_speech_config(SpeechConfig {
        backend: BackendKind::Command,
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' started >\"$1\"; exec /bin/sleep 20".into(),
            "test-cancelled-speech".into(),
            ready.display().to_string(),
            "{output}".into(),
        ],
        timeout_secs: 30,
        ..SpeechConfig::default()
    });
    let houses = crate::zones::fixtures::households();
    let surface = Arc::new(
        Surface::new(
            Box::new(NoSpeakerIo),
            Box::new(move |_| Ok(houses.clone())),
            Policy::default(),
            Box::new(fsonos_core::clock::SystemClock),
        )
        .with_announcements(Announcements::new(
            media,
            Box::new(|| Some("http://192.0.2.250:3400".into())),
        )),
    );
    let accepted = surface
        .start_announcement(
            &Client::Cli,
            &AnnounceRequest {
                text: Some("This speech is cancelled while it is being synthesized".into()),
                rooms: vec!["Patio".into()],
                ..AnnounceRequest::default()
            },
        )
        .unwrap();
    let deadline = Instant::now() + WAIT;
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    // Always cancel before asserting readiness: a failed test must not strand
    // a synthesis process for its full timeout.
    let cancelled_at = Instant::now();
    surface
        .cancel_announcement(&Client::Cli, &accepted.id)
        .unwrap();
    let finished = finished_job(&surface, &accepted.id);
    assert!(ready.exists(), "the scripted speech engine did not start");
    assert!(cancelled_at.elapsed() < Duration::from_secs(5));
    assert_eq!(finished.phase, AnnouncementJobPhase::Cancelled);
    assert!(finished.error.is_none());
    let result = finished.result.as_ref().unwrap();
    assert!(result.done.contains("before playback"));
    assert_eq!(surface.announcements_pending(), 0);
    assert_eq!(
        surface
            .cancel_announcement(&Client::Cli, &accepted.id)
            .unwrap()
            .phase,
        AnnouncementJobPhase::Cancelled
    );
    if media_path.exists() {
        assert!(std::fs::read_dir(&media_path).unwrap().all(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_none_or(|extension| extension != "wav")
        }));
    }
}
