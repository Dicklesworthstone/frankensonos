use super::*;
use crate::announce::clip::wav_info;
#[cfg(unix)]
use crate::announce::clip::{MAX_SPEECH_CHARS, MediaStore, encode_wav};

fn discover(
    config: &SpeechConfig,
    voice: Option<&str>,
    macos: bool,
    available: &[&str],
) -> Result<SpeechBackend, ClipError> {
    config.discover_with(voice, macos, |program| {
        available.contains(&program).then(|| PathBuf::from(program))
    })
}

fn custom_config(program: &str, args: &[&str]) -> SpeechConfig {
    SpeechConfig {
        backend: BackendKind::Command,
        command: std::iter::once(program.to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect(),
        ..SpeechConfig::default()
    }
}

fn piper_model(dir: &Path, name: &str) -> PathBuf {
    let model = dir.join(format!("{name}.onnx"));
    fs::write(&model, b"model availability fixture").unwrap();
    fs::write(dir.join(format!("{name}.onnx.json")), b"{}").unwrap();
    model
}

#[test]
fn automatic_selection_preserves_macos_and_prefers_frankentts_on_linux() {
    let config = SpeechConfig::default();
    for (macos, available, expected_name, expected_program) in [
        (
            true,
            vec!["/usr/bin/say", "ftts", "franken_tts", "espeak-ng"],
            "say",
            "/usr/bin/say",
        ),
        (
            false,
            vec!["ftts", "franken_tts", "espeak-ng", "piper"],
            "frankentts",
            "ftts",
        ),
        (
            false,
            vec!["franken_tts", "espeak-ng"],
            "frankentts",
            "franken_tts",
        ),
        (false, vec!["espeak-ng", "piper"], "espeak-ng", "espeak-ng"),
    ] {
        let backend = discover(&config, None, macos, &available).unwrap();
        assert_eq!(backend.name(), expected_name);
        assert_eq!(backend.executable(), Path::new(expected_program));
    }
    // Preserve the macOS contract even when an unrelated engine is installed.
    assert!(matches!(
        discover(&config, None, true, &["ftts"]),
        Err(ClipError::SpeechUnavailable(_))
    ));
}

#[test]
fn explicit_selection_never_silently_falls_back_to_another_engine() {
    let config = SpeechConfig {
        backend: BackendKind::EspeakNg,
        ..SpeechConfig::default()
    };
    assert_eq!(
        discover(&config, None, false, &["ftts", "espeak-ng"])
            .unwrap()
            .name(),
        "espeak-ng"
    );
    assert!(matches!(
        discover(&config, None, false, &["ftts"]),
        Err(ClipError::SpeechUnavailable(_))
    ));

    let config = SpeechConfig {
        backend: BackendKind::Frankentts,
        ..SpeechConfig::default()
    };
    let error = discover(&config, None, false, &["espeak-ng"]).unwrap_err();
    assert!(matches!(&error, ClipError::SpeechUnavailable(_)));
    assert!(error.to_string().contains("ftts pull"));
}

#[test]
fn absent_engines_report_actionable_local_setup() {
    let error = discover(&SpeechConfig::default(), None, false, &[]).unwrap_err();
    assert!(matches!(&error, ClipError::SpeechUnavailable(_)));
    let detail = error.to_string();
    assert!(detail.contains("FrankenTTS"));
    assert!(detail.contains("espeak-ng"));
    assert!(detail.contains("speech.toml"));
}

#[test]
fn config_defaults_and_environment_precedence_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let defaults = SpeechConfig::load_with_env(dir.path(), |_| Ok(None)).unwrap();
    assert_eq!(defaults, SpeechConfig::default());

    fs::write(
        dir.path().join("speech.toml"),
        "backend = 'espeak-ng'\nvoice = 'en'\ntimeout_secs = 20\n",
    )
    .unwrap();
    let config = SpeechConfig::load_with_env(dir.path(), |key| {
        Ok(match key {
            "FSONOS_TTS_BACKEND" => Some("piper".into()),
            "FSONOS_TTS_COMMAND" => Some(
                serde_json::json!(["/local/engine", "{output}", "--voice={voice}"]).to_string(),
            ),
            "FSONOS_TTS_VOICE" => Some("voices/reader; literal.ftvoice".into()),
            "FSONOS_PIPER_MODEL" => Some("/models/reader.onnx".into()),
            "FSONOS_TTS_TIMEOUT_SECS" => Some("42".into()),
            _ => None,
        })
    })
    .unwrap();
    assert_eq!(config.backend, BackendKind::Command);
    assert_eq!(config.command[0], "/local/engine");
    assert_eq!(
        config.voice.as_deref(),
        Some("voices/reader; literal.ftvoice")
    );
    assert_eq!(
        config.piper_model,
        Some(PathBuf::from("/models/reader.onnx"))
    );
    assert_eq!(config.timeout_secs, 42);
}

#[test]
fn malformed_files_and_environment_values_are_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    for contents in [
        "backend = [",
        "backend = 'unrecognized'",
        "timeout_sec = 30",
        "timeout_secs = 0",
        "timeout_secs = 601",
        "command = 'not an argv array'",
    ] {
        fs::write(dir.path().join("speech.toml"), contents).unwrap();
        assert!(
            matches!(
                SpeechConfig::load_with_env(dir.path(), |_| Ok(None)),
                Err(ClipError::SpeechConfig(_))
            ),
            "accepted invalid configuration: {contents}"
        );
    }
    fs::write(dir.path().join("speech.toml"), "").unwrap();
    for (name, value) in [
        ("FSONOS_TTS_COMMAND", "espeak-ng -w {output}"),
        ("FSONOS_TTS_COMMAND", "[1,2]"),
        ("FSONOS_TTS_BACKEND", "unknown-engine"),
        ("FSONOS_TTS_TIMEOUT_SECS", "forever"),
    ] {
        assert!(matches!(
            SpeechConfig::load_with_env(dir.path(), |key| {
                Ok((key == name).then(|| value.to_owned()))
            }),
            Err(ClipError::SpeechConfig(_))
        ));
    }
}

#[test]
fn custom_commands_require_exactly_one_output_and_only_supported_placeholders() {
    for command in [
        vec![],
        vec!["", "{output}"],
        vec!["engine"],
        vec!["engine", "out.wav"],
        vec!["engine", "{output}", "--again={output}"],
        vec!["engine", "{output}", "{text}"],
        vec!["{voice}", "{output}"],
        vec!["engine", "{output}", "nul\0argument"],
    ] {
        let config = SpeechConfig {
            backend: BackendKind::Command,
            command: command.iter().map(|arg| (*arg).to_owned()).collect(),
            ..SpeechConfig::default()
        };
        assert!(matches!(config.validate(), Err(ClipError::SpeechConfig(_))));
    }
    assert!(
        custom_config("/local/engine", &["--output={output}"])
            .validate()
            .is_ok()
    );
}

#[test]
fn configured_command_takes_precedence_and_voice_support_is_explicit() {
    let mut config = custom_config("engine", &["{output}"]);
    config.backend = BackendKind::Auto;
    assert_eq!(
        discover(&config, None, false, &["engine", "ftts"])
            .unwrap()
            .name(),
        "command"
    );
    assert!(matches!(
        discover(&config, Some("aria"), false, &["engine"]),
        Err(ClipError::SpeechConfig(_))
    ));
    config.command.push("--voice={voice}".into());
    assert!(matches!(
        discover(&config, None, false, &["engine"]),
        Err(ClipError::SpeechConfig(_))
    ));
    config.voice = Some("default voice".into());
    let backend = discover(&config, Some("request voice"), false, &["engine"]).unwrap();
    assert_eq!(backend.voice.as_deref(), Some("request voice"));
}

#[test]
fn voice_validation_accepts_literal_paths_and_rejects_options_and_controls() {
    let config = SpeechConfig {
        backend: BackendKind::Frankentts,
        ..SpeechConfig::default()
    };
    for voice in [
        "aria",
        "voices/my own voice.ftvoice",
        "voice; $(literal) & 'quotes' {output}.ftvoice",
    ] {
        let backend = discover(&config, Some(voice), false, &["ftts"]).unwrap();
        assert_eq!(backend.voice.as_deref(), Some(voice));
    }
    for voice in [
        "",
        "-o injected.wav",
        "line\nbreak",
        "tab\tvoice",
        "nul\0voice",
    ] {
        assert!(matches!(
            discover(&config, Some(voice), false, &["ftts"]),
            Err(ClipError::BadVoice(_))
        ));
    }
    assert!(matches!(
        discover(&config, Some(&"a".repeat(4097)), false, &["ftts"]),
        Err(ClipError::BadVoice(_))
    ));
}

#[test]
fn native_engine_arguments_keep_voices_separate_and_text_on_stdin() {
    let out = Path::new("/private/media with spaces/speech.wav");
    let voice = "voices/reader; literal.ftvoice";
    let ftts = discover(
        &SpeechConfig {
            backend: BackendKind::Frankentts,
            ..SpeechConfig::default()
        },
        Some(voice),
        false,
        &["ftts"],
    )
    .unwrap();
    assert_eq!(
        ftts.args(out),
        vec![
            OsString::from("say"),
            "--file".into(),
            "-".into(),
            "--output".into(),
            out.as_os_str().to_owned(),
            "--no-resident".into(),
            format!("--voice={voice}").into(),
        ]
    );

    for (kind, executable, expected) in [
        (
            BackendKind::EspeakNg,
            "espeak-ng",
            vec!["--stdin", "-w", out.to_str().unwrap(), "-v", voice],
        ),
        (
            BackendKind::Say,
            "/usr/bin/say",
            vec![
                "-o",
                out.to_str().unwrap(),
                "--file-format=WAVE",
                "--data-format=LEI16@44100",
                "-f",
                "-",
                "-v",
                voice,
            ],
        ),
    ] {
        let backend = discover(
            &SpeechConfig {
                backend: kind,
                ..SpeechConfig::default()
            },
            Some(voice),
            false,
            &[executable],
        )
        .unwrap();
        assert_eq!(
            backend.args(out),
            expected.into_iter().map(OsString::from).collect::<Vec<_>>()
        );
    }
}

#[test]
fn configured_piper_requires_local_model_and_sidecar_and_accepts_voice_override() {
    let dir = tempfile::tempdir().unwrap();
    let default_model = piper_model(dir.path(), "default");
    let requested_model = piper_model(dir.path(), "requested voice");
    let config = SpeechConfig {
        piper_model: Some(default_model.clone()),
        ..SpeechConfig::default()
    };
    let backend = discover(&config, None, false, &["ftts", "piper", "espeak-ng"]).unwrap();
    assert_eq!(backend.name(), "piper");
    assert_eq!(
        backend.piper_model,
        Some(std::path::absolute(&default_model).unwrap())
    );

    let requested = discover(&config, requested_model.to_str(), false, &["ftts", "piper"]).unwrap();
    let out = dir.path().join("speech.wav");
    assert_eq!(
        requested.args(&out),
        vec![
            OsString::from("-m"),
            std::path::absolute(&requested_model)
                .unwrap()
                .into_os_string(),
            "-f".into(),
            out.into_os_string(),
        ]
    );

    let incomplete = dir.path().join("incomplete.onnx");
    fs::write(&incomplete, b"model without adjacent configuration").unwrap();
    let error = discover(
        &config,
        incomplete.to_str(),
        false,
        &["ftts", "piper", "espeak-ng"],
    )
    .unwrap_err();
    assert!(matches!(&error, ClipError::SpeechUnavailable(_)));
    assert!(error.to_string().contains(".onnx.json"));

    let explicit = SpeechConfig {
        backend: BackendKind::Piper,
        ..SpeechConfig::default()
    };
    assert!(matches!(
        discover(&explicit, None, false, &["piper"]),
        Err(ClipError::SpeechUnavailable(_))
    ));

    let missing = SpeechConfig {
        piper_model: Some(dir.path().join("absent.onnx")),
        ..SpeechConfig::default()
    };
    assert!(matches!(
        discover(&missing, None, false, &["ftts", "piper", "espeak-ng"]),
        Err(ClipError::SpeechUnavailable(_))
    ));
    assert_eq!(
        discover(
            &SpeechConfig::default(),
            requested_model.to_str(),
            false,
            &["ftts", "piper"],
        )
        .unwrap()
        .name(),
        "piper"
    );
}

#[cfg(unix)]
#[test]
fn piper_model_symlinks_keep_the_validated_adjacent_sidecar() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let weights = dir.path().join("shared-weights.onnx");
    fs::write(&weights, b"model availability fixture").unwrap();
    let voices = dir.path().join("voices");
    fs::create_dir(&voices).unwrap();
    let model = voices.join("current.onnx");
    symlink("../shared-weights.onnx", &model).unwrap();
    fs::write(voices.join("current.onnx.json"), b"{}").unwrap();
    // The sidecar belongs to the selected voice, not the shared weights.
    assert!(!dir.path().join("shared-weights.onnx.json").exists());
    let config = SpeechConfig {
        piper_model: Some(model.clone()),
        ..SpeechConfig::default()
    };
    let backend = discover(&config, None, false, &["piper"]).unwrap();
    let out = dir.path().join("speech.wav");
    let args = backend.args(&out);
    assert_eq!(args[0], OsStr::new("-m"));
    assert_eq!(args[1], std::path::absolute(&model).unwrap().as_os_str());
    let mut sidecar = args[1].clone();
    sidecar.push(".json");
    assert!(Path::new(&sidecar).is_file());
}

#[test]
fn command_expansion_is_one_pass_and_does_not_split_literal_voices() {
    let out = Path::new("/media with spaces/output.wav");
    let voice = "voice/{output}; $(literal) 'quoted'";
    let backend = discover(
        &custom_config("engine", &["--out={output}", "--voice={voice}"]),
        Some(voice),
        false,
        &["engine"],
    )
    .unwrap();
    assert_eq!(
        backend.args(out),
        vec![
            OsString::from("--out=/media with spaces/output.wav"),
            OsString::from("--voice=voice/{output}; $(literal) 'quoted'"),
        ]
    );
}

#[cfg(unix)]
#[test]
fn output_template_preserves_non_utf8_path_bytes() {
    use std::os::unix::ffi::OsStringExt as _;

    let out = PathBuf::from(OsString::from_vec(b"/media/clip-\xff.wav".to_vec()));
    let mut expected = OsString::from("--out=");
    expected.push(&out);
    assert_eq!(expand_arg("--out={output}", &out, ""), expected);
}

#[cfg(unix)]
fn script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let path = dir.join("speech engine;literal");
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[cfg(unix)]
fn scripted_backend(dir: &Path, body: &str, args: &[&str], voice: Option<&str>) -> SpeechBackend {
    let executable = script(dir, body);
    let mut config = custom_config(executable.to_str().unwrap(), args);
    config.voice = voice.map(str::to_owned);
    config.discover(None).unwrap()
}

#[cfg(unix)]
#[test]
fn generated_speech_preserves_stdin_and_voice_literals_and_enters_the_media_store() {
    let dir = tempfile::tempdir().unwrap();
    let wav = encode_wav(&[0, 1200, -1200, 100, -100].repeat(480), 24_000);
    let fixture = dir.path().join("source.wav");
    let recorded_text = dir.path().join("text.txt");
    let recorded_voice = dir.path().join("voice.txt");
    let marker = dir.path().join("must-not-be-created");
    fs::write(&fixture, &wav).unwrap();
    let voice = format!(
        "voices/{{output}}; $(touch {}) 'literal'.ftvoice",
        marker.display()
    );
    let text = format!(
        "-n --output=injected; $(touch {})\nUTF-8: café. 'quoted' & literal",
        marker.display()
    );
    let backend = scripted_backend(
        dir.path(),
        "/bin/cat > \"$3\"\nprintf '%s' \"$5\" > \"$4\"\n/bin/cat \"$1\" > \"$2\"",
        &[
            fixture.to_str().unwrap(),
            "{output}",
            recorded_text.to_str().unwrap(),
            recorded_voice.to_str().unwrap(),
            "{voice}",
        ],
        Some(&voice),
    );
    let data_dir = dir.path().join("data with spaces ' ;");
    let media_dir = data_dir.join("media");
    let rendered = backend.render(&text, &media_dir).unwrap();
    assert_eq!(rendered, wav);
    assert_eq!(fs::read_to_string(recorded_text).unwrap(), text);
    assert_eq!(fs::read_to_string(recorded_voice).unwrap(), voice);
    assert!(!marker.exists());
    assert_eq!(fs::read_dir(&media_dir).unwrap().count(), 0);

    let store = MediaStore::new(&data_dir);
    let clip = store.put(&rendered).unwrap();
    assert_eq!(clip.duration, Duration::from_millis(100));
    assert_eq!(
        fs::read(store.path(&format!("{}.wav", clip.id)).unwrap()).unwrap(),
        wav
    );
    let info = wav_info(&rendered).unwrap();
    assert_eq!(
        (info.sample_rate, info.channels, info.bits),
        (24_000, 1, 16)
    );
}

#[cfg(unix)]
#[test]
fn nonzero_exit_reports_diagnostics_and_rejects_even_a_valid_partial_wav() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("partial.wav");
    fs::write(&fixture, encode_wav(&[0; 2400], 24_000)).unwrap();
    let backend = scripted_backend(
        dir.path(),
        "/bin/cat \"$1\" > \"$2\"\nprintf '%s\\n' 'deliberate engine failure' >&2\nexit 23",
        &[fixture.to_str().unwrap(), "{output}"],
        None,
    );
    let media = dir.path().join("media");
    let error = backend.render("hello", &media).unwrap_err();
    assert!(matches!(&error, ClipError::Speech(_)));
    let detail = error.to_string();
    assert!(detail.contains("23"));
    assert!(detail.contains("deliberate engine failure"));
    assert_eq!(fs::read_dir(media).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn zero_exit_without_output_is_not_success() {
    let dir = tempfile::tempdir().unwrap();
    let backend = scripted_backend(dir.path(), "exit 0", &["{output}"], None);
    let error = backend
        .render("hello", &dir.path().join("media"))
        .unwrap_err();
    assert!(matches!(&error, ClipError::Speech(_)));
    assert!(error.to_string().contains("did not write a readable WAV"));
}

#[cfg(unix)]
#[test]
fn malformed_or_unsupported_engine_output_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("output-fixture");
    let valid = encode_wav(&[0; 2400], 24_000);
    let mut float = valid.clone();
    float[20..22].copy_from_slice(&3u16.to_le_bytes());
    let backend = scripted_backend(
        dir.path(),
        "/bin/cat \"$1\" > \"$2\"",
        &[fixture.to_str().unwrap(), "{output}"],
        None,
    );
    for output in [b"not a WAV".to_vec(), valid[..40].to_vec(), float] {
        fs::write(&fixture, output).unwrap();
        let media = dir.path().join("media");
        assert!(matches!(
            backend.render("hello", &media),
            Err(ClipError::NotWav(_))
        ));
        assert_eq!(fs::read_dir(&media).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[test]
fn spawn_failure_is_reported_after_executable_discovery() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("unstartable-engine");
    fs::write(&executable, b"#!/nonexistent-fsonos-test-interpreter\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let backend = custom_config(executable.to_str().unwrap(), &["{output}"])
        .discover(None)
        .unwrap();
    let error = backend
        .render("hello", &dir.path().join("media"))
        .unwrap_err();
    assert!(matches!(&error, ClipError::Speech(_)));
    assert!(error.to_string().contains("could not start command"));
}

#[cfg(unix)]
#[test]
fn an_engine_that_never_reads_stdin_times_out_and_is_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("child-proc.pid");
    let body = if cfg!(target_os = "linux") {
        "read -r proc_pid rest < /proc/self/stat\nprintf '%s' \"$proc_pid\" > \"$2\"\nexec /bin/sleep 5"
    } else {
        "exec /bin/sleep 5"
    };
    let mut backend = scripted_backend(
        dir.path(),
        body,
        &["{output}", pid_file.to_str().unwrap()],
        None,
    );
    backend.timeout = Duration::from_millis(200);
    let start = Instant::now();
    let error = backend
        .render(
            &"\u{1f5e3}".repeat(MAX_SPEECH_CHARS),
            &dir.path().join("media"),
        )
        .unwrap_err();
    assert!(matches!(error, ClipError::SpeechTimeout { .. }));
    assert!(start.elapsed() < Duration::from_secs(3));
    #[cfg(target_os = "linux")]
    {
        let pid = fs::read_to_string(pid_file).unwrap();
        assert!(
            !Path::new("/proc").join(pid).exists(),
            "timed-out engine survived"
        );
    }
}

#[cfg(target_os = "linux")]
const START_SLEEPING_DESCENDANT: &str = "\
/bin/sh -c 'read -r proc_pid rest < /proc/self/stat; printf \"%s\" \"$proc_pid\" > \"$1\"; exec /bin/sleep 30' speech-child \"$3\" &
engine_pid=$!
printf '%s' \"$engine_pid\" > \"$2\"
while [ ! -s \"$3\" ]; do /bin/sleep 0.01; done";

#[cfg(target_os = "linux")]
fn descendant_survived(pid_file: &Path, proc_pid_file: &Path) -> bool {
    let pid = fs::read_to_string(pid_file)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    // /proc can be mounted from an outer PID namespace. The child records
    // the PID visible through /proc/self; $! is only valid for local signals.
    let proc_pid = fs::read_to_string(proc_pid_file)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let proc_stat = Path::new("/proc").join(proc_pid.to_string()).join("stat");
    let is_alive = || {
        fs::read_to_string(&proc_stat)
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(") ")
                    .and_then(|(_, fields)| fields.chars().next())
            })
            .is_some_and(|state| state != 'Z' && state != 'X')
    };
    // Group signals are asynchronous; a killed descendant may briefly be
    // runnable, or remain a zombie until its new parent reaps it.
    let deadline = Instant::now() + Duration::from_secs(1);
    while is_alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let survived = is_alive();
    if survived {
        // Always clean up our known descendant before failing the regression.
        use rustix::process::{Pid, Signal, kill_process};
        let _ = kill_process(Pid::from_raw(pid).unwrap(), Signal::KILL);
    }
    survived
}

#[cfg(target_os = "linux")]
#[test]
fn timeout_terminates_an_engine_spawned_by_a_waiting_wrapper() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let proc_pid_file = dir.path().join("descendant-proc.pid");
    let body = format!("{START_SLEEPING_DESCENDANT}\nwait \"$engine_pid\"");
    let mut backend = scripted_backend(
        dir.path(),
        &body,
        &[
            "{output}",
            pid_file.to_str().unwrap(),
            proc_pid_file.to_str().unwrap(),
        ],
        None,
    );
    backend.timeout = Duration::from_secs(1);
    let media = dir.path().join("media");
    let result = backend.render("Stop the engine as well as its wrapper", &media);
    let survived = descendant_survived(&pid_file, &proc_pid_file);
    assert!(matches!(result, Err(ClipError::SpeechTimeout { .. })));
    assert!(
        !survived,
        "speech wrapper's engine survived synthesis timeout"
    );
    assert_eq!(fs::read_dir(media).unwrap().count(), 0);
}

#[cfg(target_os = "linux")]
fn completed_wrapper_cleans_up_descendant(exit_code: u8) {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("descendant.pid");
    let proc_pid_file = dir.path().join("descendant-proc.pid");
    let fixture = dir.path().join("valid.wav");
    let wav = encode_wav(&[0; 2400], 24_000);
    fs::write(&fixture, &wav).unwrap();
    let body = format!("{START_SLEEPING_DESCENDANT}\n/bin/cat \"$4\" > \"$1\"\nexit {exit_code}");
    let backend = scripted_backend(
        dir.path(),
        &body,
        &[
            "{output}",
            pid_file.to_str().unwrap(),
            proc_pid_file.to_str().unwrap(),
            fixture.to_str().unwrap(),
        ],
        None,
    );
    let media = dir.path().join("media");
    let result = backend.render("A completed wrapper must leave no running engine", &media);
    let survived = descendant_survived(&pid_file, &proc_pid_file);
    assert!(
        !survived,
        "engine survived its wrapper's exit code {exit_code}"
    );
    if exit_code == 0 {
        assert_eq!(result.unwrap(), wav);
    } else {
        let error = result.unwrap_err();
        assert!(matches!(&error, ClipError::Speech(_)));
        assert!(error.to_string().contains(&exit_code.to_string()));
    }
    assert_eq!(fs::read_dir(media).unwrap().count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn successful_wrapper_with_valid_wav_cannot_leave_its_engine_running() {
    completed_wrapper_cleans_up_descendant(0);
}

#[cfg(target_os = "linux")]
#[test]
fn failed_wrapper_cannot_leave_its_engine_running() {
    completed_wrapper_cleans_up_descendant(7);
}

#[cfg(unix)]
#[test]
fn excessive_engine_diagnostics_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let diagnostics = dir.path().join("diagnostics.txt");
    fs::write(&diagnostics, vec![b'x'; 70_000]).unwrap();
    let mut backend = scripted_backend(
        dir.path(),
        "/bin/cat \"$2\" >&2\nexec /bin/sleep 5",
        &["{output}", diagnostics.to_str().unwrap()],
        None,
    );
    backend.timeout = Duration::from_secs(1);
    let error = backend
        .render("hello", &dir.path().join("media"))
        .unwrap_err();
    assert!(matches!(&error, ClipError::Speech(_)));
    assert!(error.to_string().contains("more than 64 KiB"));
}

#[cfg(unix)]
#[test]
fn verbose_stdout_cannot_block_generation_or_become_audio() {
    let dir = tempfile::tempdir().unwrap();
    let chatter = dir.path().join("progress.txt");
    let fixture = dir.path().join("speech.wav");
    let wav = encode_wav(&[0; 2400], 24_000);
    fs::write(&chatter, vec![b'x'; 1024 * 1024]).unwrap();
    fs::write(&fixture, &wav).unwrap();
    let mut backend = scripted_backend(
        dir.path(),
        "/bin/cat \"$2\"\n/bin/cat \"$3\" > \"$1\"",
        &[
            "{output}",
            chatter.to_str().unwrap(),
            fixture.to_str().unwrap(),
        ],
        None,
    );
    backend.timeout = Duration::from_secs(3);
    let rendered = backend.render("hello", &dir.path().join("media")).unwrap();
    assert_eq!(rendered, wav);
}

#[cfg(target_os = "linux")]
#[test]
fn real_espeak_ng_generates_an_announcement_wav_when_opted_in() {
    if std::env::var("FSONOS_TEST_ESPEAK_NG").as_deref() != Ok("1") {
        eprintln!("SKIPPED real Linux speech: set FSONOS_TEST_ESPEAK_NG=1 with espeak-ng on PATH");
        return;
    }
    let backend = SpeechConfig {
        backend: BackendKind::EspeakNg,
        ..SpeechConfig::default()
    }
    .discover(Some("en"))
    .expect("FSONOS_TEST_ESPEAK_NG=1 requires an installed espeak-ng");
    let dir = tempfile::tempdir().unwrap();
    let wav = backend
        .render("-n FrankenSonos offline speech check.", dir.path())
        .unwrap();
    let info = wav_info(&wav).unwrap();
    assert_eq!((info.channels, info.bits), (1, 16));
    assert!(info.sample_rate > 0);
    assert!(info.duration > Duration::from_millis(100));
    eprintln!(
        "real espeak-ng {}: {info:?}",
        backend.executable().display()
    );
}

#[test]
fn real_frankentts_generates_an_announcement_wav_when_opted_in() {
    if std::env::var("FSONOS_TEST_FTTS").as_deref() != Ok("1") {
        eprintln!(
            "SKIPPED real FrankenTTS speech: set FSONOS_TEST_FTTS=1 with ftts and local models installed"
        );
        return;
    }
    let backend = SpeechConfig {
        backend: BackendKind::Frankentts,
        ..SpeechConfig::default()
    }
    .discover(Some("matt"))
    .expect("FSONOS_TEST_FTTS=1 requires an installed FrankenTTS executable");
    let dir = tempfile::tempdir().unwrap();
    let wav = backend
        .render("FrankenSonos offline speech check.", dir.path())
        .unwrap();
    let info = wav_info(&wav).unwrap();
    assert_eq!(
        (info.sample_rate, info.channels, info.bits),
        (24_000, 1, 16)
    );
    assert!(info.duration > Duration::from_millis(100));
    eprintln!(
        "real FrankenTTS {}: {info:?}",
        backend.executable().display()
    );
}
