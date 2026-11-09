use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{NaiveTime, Weekday};

use super::*;

fn path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "fsonos-mood-config-{}-{name}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory.join("moods.toml")
}

fn custom(name: &str) -> MoodsConfig {
    MoodsConfig {
        moods: BTreeMap::from([(
            name.to_owned(),
            DjConstraints {
                include_genres: vec!["jazz".to_owned()],
                strict: true,
                ..DjConstraints::default()
            },
        )]),
        programs: None,
    }
}

fn all_day(mood: &str) -> ProgramConfig {
    ProgramConfig {
        days: every_day(),
        from: "00:00".into(),
        to: "00:00".into(),
        mood: mood.into(),
    }
}

fn at(hour: u32, minute: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
}

#[test]
fn absent_configuration_follows_defaults_and_no_op_does_not_create_it() {
    let path = path("missing");
    let loaded = MoodsConfig::load(&path).unwrap();
    assert_eq!(loaded.config, MoodsConfig::default());
    assert_eq!(loaded.config.resolve().unwrap(), Moods::builtin());
    assert_eq!(loaded.revision.len(), 64);
    assert!(loaded.revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let saved = loaded.config.save(&path, &loaded.revision).unwrap();
    assert!(!saved.changed);
    assert_eq!(saved.snapshot, loaded);
    assert!(!path.exists());

    fs::write(&path, "").unwrap();
    let empty = MoodsConfig::load(&path).unwrap();
    assert_eq!(empty.config, loaded.config);
    assert_ne!(empty.revision, loaded.revision);
}

#[test]
fn existing_toml_preserves_every_constraint_and_only_owner_overrides() {
    let text = r#"
[moods.Late-Night]
include_artists = ["Fixture Artist"]
periods = ["baroque"]
song_keywords = ["gospel", "folk"]
strict = true
expires_at = 1900000000
energy_bias = -1

[moods.focus]
energy_bias = 0

[[programs]]
days = "weekends"
from = "8:0"
to = "24:00"
mood = "Late-Night"
"#;
    let config = MoodsConfig::parse(text, "legacy.toml").unwrap();
    assert_eq!(config.moods.len(), 2);
    assert!(!config.moods.contains_key("calm"));
    let late = &config.moods["late-night"];
    assert_eq!(late.song_keywords, ["gospel", "folk"]);
    assert_eq!(late.expires_at, Some(1_900_000_000));
    assert!(late.strict);
    let programs = config.programs.as_ref().unwrap();
    assert_eq!(programs[0].days, ["sat", "sun"]);
    assert_eq!(programs[0].from, "08:00");
    assert_eq!(programs[0].to, "24:00");
    assert_eq!(programs[0].mood, "late-night");
    let effective = config.resolve().unwrap();
    assert_eq!(effective, Moods::parse(text, "legacy.toml").unwrap());
    assert_eq!(effective.get("focus"), Some(&DjConstraints::default()));
    assert!(effective.get("calm").is_some());
    let roundtrip = MoodsConfig::parse(&config.to_toml().unwrap(), "saved.toml").unwrap();
    assert_eq!(roundtrip, config);
}

#[test]
fn program_editor_retains_overnight_start_day_and_first_match_precedence() {
    let mut config = custom("late-night");
    config.programs = Some(vec![
        ProgramConfig {
            days: vec!["fri".into()],
            from: "21:00".into(),
            to: "06:00".into(),
            mood: "late-night".into(),
        },
        all_day("bright"),
    ]);
    let effective = config.resolve().unwrap();
    for (day, time, expected) in [
        (Weekday::Fri, at(22, 0), "late-night"),
        (Weekday::Sat, at(3, 0), "late-night"),
        (Weekday::Fri, at(3, 0), "bright"),
        (Weekday::Sat, at(6, 0), "bright"),
    ] {
        assert_eq!(effective.program_at(day, time), Some(expected));
    }
    config.programs.as_mut().unwrap().reverse();
    assert_eq!(
        config
            .resolve()
            .unwrap()
            .program_at(Weekday::Fri, at(22, 0)),
        Some("bright")
    );
}

#[test]
fn save_load_disable_and_restore_are_the_same_configuration_playback_reads() {
    let path = path("save");
    let original = MoodsConfig::load(&path).unwrap();
    let mut config = custom("Study");
    config.programs = Some(vec![all_day("Study")]);
    let saved = config.save(&path, &original.revision).unwrap();
    assert!(saved.changed);
    assert!(saved.snapshot.config.moods.contains_key("study"));
    assert_eq!(MoodsConfig::load(&path).unwrap(), saved.snapshot);
    assert_eq!(
        Moods::load(&path)
            .unwrap()
            .program_at(Weekday::Mon, at(12, 0)),
        Some("study")
    );
    let mut disabled = saved.snapshot.config;
    disabled.programs = Some(vec![]);
    let saved = disabled.save(&path, &saved.snapshot.revision).unwrap();
    assert!(Moods::load(&path).unwrap().programs().is_empty());
    assert_eq!(saved.snapshot.config.programs, Some(vec![]));

    let reset = MoodsConfig::default()
        .save(&path, &saved.snapshot.revision)
        .unwrap();
    assert!(reset.changed);
    assert_eq!(Moods::load(&path).unwrap(), Moods::builtin());
    assert_eq!(reset.snapshot.config, MoodsConfig::default());
}

#[test]
fn unchanged_document_preserves_comments_spelling_and_revision() {
    let path = path("comments");
    let text = "# Owner's note\n[moods.Study]\nenergy_bias = -1 # quiet\n\n";
    fs::write(&path, text).unwrap();
    let current = MoodsConfig::load(&path).unwrap();
    let saved = current.config.save(&path, &current.revision).unwrap();
    assert!(!saved.changed);
    assert_eq!(saved.snapshot, current);
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn stale_revision_includes_comment_only_edits_and_does_not_replace_them() {
    let path = path("stale");
    let original = MoodsConfig::load(&path).unwrap();
    let saved = custom("study").save(&path, &original.revision).unwrap();
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\n# Edited outside the daemon\n");
    fs::write(&path, &text).unwrap();
    let fresh = MoodsConfig::load(&path).unwrap();
    let result = custom("evening").save(&path, &saved.snapshot.revision);
    assert!(
        matches!(result, Err(ConfigWriteError::Conflict { current_revision }) if current_revision == fresh.revision)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    assert_eq!(fresh.config, saved.snapshot.config);
}

#[test]
fn invalid_configuration_changes_neither_the_file_nor_its_revision() {
    let path = path("invalid");
    let original = MoodsConfig::load(&path).unwrap();
    let saved = custom("study").save(&path, &original.revision).unwrap();
    let before = fs::read(&path).unwrap();
    let missing_mood = MoodsConfig {
        programs: Some(vec![all_day("not-defined")]),
        ..MoodsConfig::default()
    };
    let mut bad_energy = custom("study");
    bad_energy.moods.get_mut("study").unwrap().energy_bias = 3;
    for config in [missing_mood, bad_energy] {
        assert!(matches!(
            config.save(&path, &saved.snapshot.revision),
            Err(ConfigWriteError::Spotify(SpotifyError::Config(_)))
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(MoodsConfig::load(&path).unwrap(), saved.snapshot);
    }
}

#[test]
fn editor_bounds_do_not_truncate_existing_local_configuration_reads() {
    let too_many_moods = MoodsConfig {
        moods: (0..=MAX_CUSTOM_MOODS)
            .map(|number| (format!("mood-{number}"), DjConstraints::default()))
            .collect(),
        programs: None,
    };
    assert!(too_many_moods.to_toml().is_err());
    let too_many_programs = MoodsConfig {
        moods: BTreeMap::new(),
        programs: Some(vec![all_day("calm"); MAX_PROGRAMS + 1]),
    };
    assert!(too_many_programs.to_toml().is_err());
    let mut oversized = custom("study");
    oversized.moods.get_mut("study").unwrap().include_keywords = vec!["a".repeat(MAX_CONFIG_BYTES)];
    assert!(oversized.to_toml().is_err());

    for (name, config) in [
        ("many-moods", too_many_moods),
        ("many-programs", too_many_programs),
        ("large", oversized),
    ] {
        let path = path(name);
        fs::write(&path, config.encode().unwrap()).unwrap();
        assert_eq!(MoodsConfig::load(&path).unwrap().config, config);
        assert_eq!(Moods::load(&path).unwrap(), config.resolve().unwrap());
    }
}

#[test]
fn two_cooperative_writers_cannot_both_replace_the_same_revision() {
    let path = path("concurrent");
    let revision = MoodsConfig::load(&path).unwrap().revision;
    let ready = Arc::new(Barrier::new(2));
    let workers: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|name| {
            let path = path.clone();
            let revision = revision.clone();
            let ready = Arc::clone(&ready);
            std::thread::spawn(move || {
                ready.wait();
                custom(name).save(&path, &revision)
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ConfigWriteError::Conflict { .. })))
            .count(),
        1
    );
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    assert_eq!(MoodsConfig::load(&path).unwrap(), winner.snapshot);
}

#[test]
fn filesystem_errors_identify_lock_and_unpublished_rename_failures() {
    let path = path("errors");
    let blocked = path.with_file_name("not-a-directory");
    fs::write(&blocked, "retained").unwrap();
    let target = blocked.join("moods.toml");
    assert!(matches!(
        custom("study").save(&target, &revision(None)),
        Err(ConfigWriteError::Lock { .. })
    ));
    assert_eq!(fs::read_to_string(&blocked).unwrap(), "retained");

    let directory = path.with_file_name("existing-directory");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("retained"), "retained").unwrap();
    assert!(matches!(
        publish(&directory, b"replacement"),
        Err(ConfigWriteError::Publish {
            published: false,
            ..
        })
    ));
    assert_eq!(
        fs::read_to_string(directory.join("retained")).unwrap(),
        "retained"
    );
}
