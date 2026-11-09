//! Exercise cancellation against real local processes, without a speech engine.

use super::*;
use crate::announce::clip::ClipSource;

#[test]
fn cancelled_render_never_spawns_an_engine() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let backend = scripted_backend(
        dir.path(),
        "printf '%s' started > \"$2\"",
        &["{output}", started.to_str().unwrap()],
        None,
    );
    let control = AnnouncementControl::default();
    assert!(control.cancel());
    let media = dir.path().join("media");
    assert!(matches!(
        backend.render_controlled("Already cancelled", &media, &control),
        Err(ClipError::Cancelled)
    ));
    assert!(!started.exists());
    assert!(!media.exists());
}

/// Always cancel, even when the engine failed to start, so an assertion cannot
/// strand a worker in a long synthesis timeout on a loaded test runner.
fn cancel_when_started(control: &AnnouncementControl, ready: &Path) -> (bool, Instant) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let started = loop {
        if fs::metadata(ready).is_ok_and(|metadata| metadata.len() > 0) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let cancelled_at = Instant::now();
    let _ = control.cancel();
    (started, cancelled_at)
}

#[test]
fn cancellation_interrupts_an_engine_that_never_reads_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("started");
    let backend = scripted_backend(
        dir.path(),
        "printf '%s' started > \"$2\"\nexec /bin/sleep 10",
        &["{output}", ready.to_str().unwrap()],
        None,
    );
    let media = dir.path().join("media");
    let control = AnnouncementControl::default();
    let (result, started, elapsed) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            backend.render_controlled(&"\u{1f5e3}".repeat(MAX_SPEECH_CHARS), &media, &control)
        });
        let (started, cancelled_at) = cancel_when_started(&control, &ready);
        let result = worker.join().unwrap();
        (result, started, cancelled_at.elapsed())
    });
    assert!(started, "engine did not reach its cancellation checkpoint");
    assert!(matches!(result, Err(ClipError::Cancelled)), "{result:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "cancellation took {elapsed:?}"
    );
    assert_eq!(fs::read_dir(media).unwrap().count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn cancelling_synthesis_reaps_wrapper_and_descendant_and_discards_valid_partial_audio() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let proc_pid_file = dir.path().join("descendant-proc.pid");
    let wrapper_proc_pid = dir.path().join("wrapper-proc.pid");
    let ready = dir.path().join("started");
    let fixture = dir.path().join("partial.wav");
    fs::write(&fixture, encode_wav(&[0; 2400], 24_000)).unwrap();
    let body = format!(
        "{START_SLEEPING_DESCENDANT}\n\
         /bin/cat \"$4\" > \"$1\"\n\
         read -r wrapper_pid rest < /proc/self/stat\n\
         printf '%s' \"$wrapper_pid\" > \"$6\"\n\
         printf '%s' started > \"$5\"\n\
         wait \"$engine_pid\""
    );
    let executable = script(dir.path(), &body);
    let mut config = custom_config(
        executable.to_str().unwrap(),
        &[
            "{output}",
            pid_file.to_str().unwrap(),
            proc_pid_file.to_str().unwrap(),
            fixture.to_str().unwrap(),
            ready.to_str().unwrap(),
            wrapper_proc_pid.to_str().unwrap(),
        ],
    );
    config.timeout_secs = 10;
    let store = MediaStore::new(dir.path()).with_speech_config(config);
    let control = AnnouncementControl::default();
    let source = ClipSource::Tts {
        text: "Cancel while a complete WAV and an active engine coexist".into(),
        voice: None,
    };
    let (result, started, elapsed) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| store.make_controlled(&source, &control));
        let (started, cancelled_at) = cancel_when_started(&control, &ready);
        let result = worker.join().unwrap();
        (result, started, cancelled_at.elapsed())
    });
    // Check and clean the known descendant before asserting on the result.
    let survived = started && descendant_survived(&pid_file, &proc_pid_file);
    assert!(started, "wrapper did not reach its cancellation checkpoint");
    assert!(matches!(result, Err(ClipError::Cancelled)), "{result:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "cancellation took {elapsed:?}"
    );
    assert!(!survived, "speech wrapper's engine survived cancellation");
    let wrapper_pid = fs::read_to_string(wrapper_proc_pid).unwrap();
    assert!(
        !Path::new("/proc").join(wrapper_pid).exists(),
        "cancelled speech wrapper was not reaped"
    );
    assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 0);
}

#[test]
fn uncancelled_speech_publishes_a_complete_clip_through_both_entrypoints() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("source.wav");
    let wav = encode_wav(&[0; 2400], 24_000);
    fs::write(&fixture, &wav).unwrap();
    let executable = script(dir.path(), "/bin/cat \"$1\" > \"$2\"");
    let config = custom_config(
        executable.to_str().unwrap(),
        &[fixture.to_str().unwrap(), "{output}"],
    );
    let store = MediaStore::new(dir.path()).with_speech_config(config);
    let control = AnnouncementControl::default();
    let source = ClipSource::Tts {
        text: "A complete announcement".into(),
        voice: None,
    };
    let made = store.make_controlled(&source, &control).unwrap();
    let spoken = store
        .speak_controlled("Still active", None, &control)
        .unwrap();
    for clip in [made, spoken] {
        assert_eq!(clip.duration, Duration::from_millis(100));
        let path = store.path(&format!("{}.wav", clip.id)).unwrap();
        assert_eq!(fs::read(path).unwrap(), wav);
    }
    assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 2);
}
