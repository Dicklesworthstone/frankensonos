use super::*;
use fsonos_core::clock::FakeClock;
use fsonos_core::store::{DjSession, MemStore};
use fsonos_spotify::dj::DjConfig;
use fsonos_spotify::feed::QueueFeed;
use fsonos_types::PlayerId;
use serde_json::json;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fresh(name: &str) -> (State, PathBuf, MemStore, FakeClock) {
    let dir = std::env::temp_dir().join(format!(
        "fsonos-dj-config-{}-{name}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    (
        State {
            moods: Moods::builtin(),
            ..State::default()
        },
        dir.join("moods.toml"),
        MemStore::default(),
        FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-10T12:00:00Z").unwrap()),
    )
}

fn request(path: &PathBuf, config: serde_json::Value) -> DjConfigureRequest {
    DjConfigureRequest {
        expected_revision: show(Some(path)).unwrap().revision,
        config: serde_json::from_value(config).unwrap(),
    }
}

#[test]
fn full_definition_crosses_file_response_and_live_plan_without_losing_runtime_state() {
    let (mut state, file, store, clock) = fresh("full-definition");
    let player = PlayerId("fixture-config-player".into());
    state.feeds.insert(
        player.clone(),
        QueueFeed::new(&player, DjConfig::default(), 8),
    );
    state.held.insert(player.clone());
    state.listening.entry(player.clone()).or_default();
    state.saved = "retained feeds".into();
    state.planned = true;
    state.plan_inputs = Some(123);
    state.plan_check_at = Some(Instant::now() + Duration::from_secs(30));
    state.plan_retry_at = state.plan_check_at;
    let request = request(
        &file,
        json!({
            "moods": {"study": {
                "include_composers":["Bach"],"exclude_composers":["Wagner"],
                "include_artists":["Artist One"],"exclude_artists":["Artist Two"],
                "include_genres":["jazz"],"exclude_genres":["metal"],
                "periods":["baroque"],"exclude_periods":["romantic"],
                "song_keywords":["piano"],"decades":[1960],"exclude_decades":[1980],
                "include_keywords":["chamber"],"exclude_keywords":["opera"],
                "min_work_minutes":1,"max_work_minutes":60,
                "energy_bias":-1,"allow_long_works":true,"strict":true,
                "expires_at":2000000000
            }},
            "programs":[{"days":["mon","fri"],"from":"22:00","to":"02:00","mood":"study"}]
        }),
    );
    let saved = change(&mut state, Some(&file), &request, &store, &clock).unwrap();
    assert!(saved.changed);
    assert_ne!(saved.configuration.revision, request.expected_revision);
    assert_eq!(saved.configuration.config, request.config);
    assert_eq!(saved.configuration, show(Some(&file)).unwrap());
    assert_eq!(
        saved.configuration.moods["study"],
        request.config.moods["study"]
    );
    assert!(!saved.configuration.builtin_moods.contains_key("study"));
    assert!(!saved.configuration.builtin_programs.is_empty());
    assert_eq!(state.moods, Moods::load(&file).unwrap());
    assert!(state.feeds.contains_key(&player));
    assert!(state.held.contains(&player));
    assert!(state.listening.contains_key(&player));
    assert_eq!(state.saved, "retained feeds");
    assert!(!state.planned);
    assert_eq!(state.plan_inputs, None);
    assert_eq!(state.plan_retry_at, None);
    assert!(state.reload_due(Instant::now()));
}

#[test]
fn stale_revision_leaves_file_and_plan_untouched_and_returns_current_revision() {
    let (mut state, file, store, clock) = fresh("stale");
    let request = request(&file, json!({"moods":{"study":{"energy_bias":-1}}}));
    std::fs::write(&file, "# changed by another editor\n").unwrap();
    state.planned = true;
    let moods = state.moods.clone();
    let current = show(Some(&file)).unwrap();
    let failure = change(&mut state, Some(&file), &request, &store, &clock).unwrap_err();
    assert_eq!(failure.code, ErrorCode::ConfigChanged);
    assert!(failure.hint.contains(&current.revision));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "# changed by another editor\n"
    );
    assert_eq!(state.moods, moods);
    assert!(state.planned);
}

#[test]
fn no_op_preserves_comments_and_revision() {
    let (mut state, file, store, clock) = fresh("comments");
    let text = "# owner notes stay intact\n[moods.study]\nenergy_bias = -1\n";
    std::fs::write(&file, text).unwrap();
    let original = show(Some(&file)).unwrap();
    let request = DjConfigureRequest {
        expected_revision: original.revision.clone(),
        config: original.config.clone(),
    };
    let saved = change(&mut state, Some(&file), &request, &store, &clock).unwrap();
    assert!(!saved.changed);
    assert_eq!(saved.configuration, original);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
}

#[test]
fn no_op_on_the_current_plan_keeps_its_refill_backoff() {
    let (mut state, file, store, clock) = fresh("no-op-backoff");
    state
        .reload(&store, Some(&file), clock.now().timestamp())
        .unwrap();
    let player = PlayerId("fixture-backoff-player".into());
    let playback = fsonos_core::playback::PlayerPlayback::default();
    let now = Instant::now();
    state.refill_retries.insert(
        player.clone(),
        crate::dj::retry::Retry::failed(None, &playback, now),
    );
    let request = request(&file, json!({}));
    let saved = change(&mut state, Some(&file), &request, &store, &clock).unwrap();
    assert!(!saved.changed);
    assert!(state.planned);
    assert!(!state.refill_retries[&player].ready(&playback, now));
    assert!(!file.exists());
}

#[test]
fn a_mood_referenced_by_preferences_cannot_be_removed_until_preferences_change() {
    let (mut state, file, store, clock) = fresh("preferences");
    std::fs::write(&file, "[moods.study]\nenergy_bias = -1\n").unwrap();
    std::fs::write(
        file.with_file_name("preferences.toml"),
        "[favor]\nmoods = [\"study\"]\n",
    )
    .unwrap();
    state
        .reload(&store, Some(&file), clock.now().timestamp())
        .unwrap();
    let previous = state.moods.clone();
    let before = std::fs::read(&file).unwrap();
    let request = request(&file, json!({}));
    let failure = change(&mut state, Some(&file), &request, &store, &clock).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidArgument);
    assert!(failure.detail.contains("preferences"));
    assert!(failure.detail.contains("study"));
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(state.moods, previous);
    assert!(state.planned);
    std::fs::write(file.with_file_name("preferences.toml"), "").unwrap();
    let saved = change(&mut state, Some(&file), &request, &store, &clock).unwrap();
    assert!(!saved.configuration.moods.contains_key("study"));
}

#[test]
fn editing_a_favored_mood_recomputes_cached_work_preferences_for_the_next_pick() {
    use fsonos_spotify::classical::Period;
    use fsonos_spotify::dj::WorkPool;
    use fsonos_spotify::works::{Completeness, Work};

    let (mut state, file, store, clock) = fresh("preference-verdict");
    std::fs::write(&file, "[moods.study]\ninclude_keywords = [\"violin\"]\n").unwrap();
    std::fs::write(
        file.with_file_name("preferences.toml"),
        "[favor]\nmoods = [\"study\"]\n",
    )
    .unwrap();
    state.moods = Moods::load(&file).unwrap();
    state.pool = WorkPool::from_works(vec![Work {
        work_key: "fixture-work".into(),
        composer: "Fixture composer".into(),
        title: "Piano miniature".into(),
        period: Period::Unknown,
        album_key: "fixture-album".into(),
        album_uri: None,
        movements: Vec::new(),
        total_secs: 180,
        completeness: Completeness::Complete,
    }]);
    prefs::apply(&mut state, Some(&file)).unwrap();
    assert!(!state.pool.verdict(0).favored);
    let request = request(
        &file,
        json!({"moods":{"study":{"include_keywords":["piano"]}}}),
    );
    change(&mut state, Some(&file), &request, &store, &clock).unwrap();
    assert_eq!(state.pool.len(), 1);
    assert!(state.pool.verdict(0).favored);
}

#[test]
fn the_adapter_rejects_removing_an_active_session_mood_and_allows_its_expiry() {
    let (mut state, file, mut store, clock) = fresh("sessions");
    std::fs::write(&file, "[moods.study]\nenergy_bias = -1\n").unwrap();
    let now = clock.now().timestamp();
    store
        .save_dj_session(&DjSession {
            coordinator: "fixture-coordinator".into(),
            mood: Some("study".into()),
            constraints: None,
            expires: now + 60,
        })
        .unwrap();
    let request = request(&file, json!({}));
    let failure = change(&mut state, Some(&file), &request, &store, &clock).unwrap_err();
    assert!(failure.detail.contains("session"));
    assert!(show(Some(&file)).unwrap().moods.contains_key("study"));
    clock.advance(chrono::TimeDelta::seconds(60));
    assert!(
        change(&mut state, Some(&file), &request, &store, &clock)
            .unwrap()
            .changed
    );
}

#[test]
fn invalid_candidate_cannot_publish_or_replace_the_cached_plan() {
    let (mut state, file, store, clock) = fresh("invalid");
    let request = request(
        &file,
        json!({
            "programs":[{"days":["mon"],"from":"08:00","to":"10:00","mood":"missing"}]
        }),
    );
    state.planned = true;
    let failure = change(&mut state, Some(&file), &request, &store, &clock).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidArgument);
    assert!(!file.exists());
    assert!(state.planned);
    assert_eq!(state.moods, Moods::builtin());
}

#[test]
fn an_unknown_stored_period_is_invalid_input_without_publishing_a_file() {
    let (mut state, file, store, clock) = fresh("unknown-period");
    let request = request(&file, json!({"moods":{"study":{"periods":["rococo"]}}}));
    state.planned = true;
    let failure = change(&mut state, Some(&file), &request, &store, &clock).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidArgument);
    assert!(failure.detail.contains("rococo"));
    assert!(!file.exists());
    assert!(state.planned);
    assert_eq!(state.moods, Moods::builtin());
}

#[test]
fn apply_reads_only_the_bounded_editable_document_and_requires_a_valid_revision() {
    let (_, moods, _, _) = fresh("file-input");
    let file = moods.with_file_name("configuration.json");
    let revision = show(Some(&moods)).unwrap().revision;
    std::fs::write(&file, "{\"moods\":{},\"programs\":[]}").unwrap();
    let request = read_request(&file, &revision).unwrap();
    assert_eq!(request.config.programs, Some(Vec::new()));
    assert!(read_request(&file, "guessed-revision").is_err());
    std::fs::write(
        &file,
        serde_json::to_vec(&show(Some(&moods)).unwrap()).unwrap(),
    )
    .unwrap();
    assert!(read_request(&file, &revision).is_err());
    std::fs::write(&file, vec![b' '; MAX_CONFIG_BYTES + 1]).unwrap();
    assert!(
        read_request(&file, &revision)
            .unwrap_err()
            .detail
            .contains("limited")
    );
}
