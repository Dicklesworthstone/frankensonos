use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};

use super::*;

const CHILD_PATH: &str = "FSONOS_TEST_PREFERENCE_EDIT_PATH";
const LOCK_OBSERVED: &str = "preference-edit-lock-observed";

fn path(name: &str) -> PathBuf {
    let directory = crate::fake_spotify::scratch_dir(name);
    fs::create_dir_all(&directory).unwrap();
    directory.join(super::super::PREFERENCES_FILE)
}

#[test]
fn no_op_edits_preserve_comments_and_an_absent_file() {
    let path = path("preference-no-op");
    let moods = Moods::builtin();
    let (preferences, changed) =
        Preferences::edit(&path, &moods, |prefs| prefs.remove("favor.genres", None)).unwrap();
    assert_eq!(preferences, Preferences::default());
    assert!(!changed);
    assert!(!path.exists());

    let text = "# Keep this note\nenergy = 30 # quiet\n[favor]\ngenres = ['jazz']\n";
    fs::write(&path, text).unwrap();
    let before = Preferences::load(&path, &moods).unwrap();
    let (after, ()) =
        Preferences::edit(&path, &moods, |prefs| prefs.add("favor.genres", "JAZZ", &moods))
            .unwrap();
    assert_eq!(after, before);
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn invalid_edits_and_unreadable_preferences_never_replace_existing_values() {
    let path = path("preference-invalid");
    let moods = Moods::builtin();
    let text = "# Keep this ban\n[ban]\nartists = ['Fixture Blocked']\n";
    fs::write(&path, text).unwrap();
    let result = Preferences::edit(&path, &moods, |prefs| {
        prefs.energy = Some(101);
        Ok(())
    });
    assert!(matches!(
        result,
        Err(PreferenceWriteError::Spotify(SpotifyError::Config(_)))
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), text);

    let invalid = "energy = 'unfinished edit'\n";
    fs::write(&path, invalid).unwrap();
    let mut attempted = false;
    assert!(
        Preferences::edit(&path, &moods, |_| {
            attempted = true;
            Ok(())
        })
        .is_err()
    );
    assert!(!attempted, "a bad file is not replaced by default preferences");
    assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
}

#[test]
fn cross_process_edits_read_the_last_accepted_values_under_the_same_lock() {
    let path = path("preference-processes");
    let moods = Moods::builtin();
    fs::write(&path, "energy = 30\n").unwrap();
    let (first, child) = Preferences::edit(&path, &moods, |prefs| {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "prefs::file::tests::waiting_process_edits_preferences",
                "--nocapture",
            ])
            .env(CHILD_PATH, &path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "the child must observe the held preference lock before publication"
            );
            if line.contains(LOCK_OBSERVED) {
                break;
            }
        }
        child.stdout = Some(output.into_inner());
        prefs.add("ban.artists", "Fixture Blocked", &moods)?;
        Ok(child)
    })
    .unwrap();
    assert_eq!(first.ban.artists, ["Fixture Blocked"]);
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let accepted = Preferences::load(&path, &moods).unwrap();
    assert_eq!(accepted.energy, Some(30));
    assert_eq!(accepted.ban.artists, ["Fixture Blocked"]);
    assert_eq!(accepted.favor.genres, ["jazz"]);
}

/// Run only when the parent above invokes this test in a separate process.
#[test]
fn waiting_process_edits_preferences() {
    let Some(path) = std::env::var_os(CHILD_PATH).map(PathBuf::from) else {
        return;
    };
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("toml.lock"))
        .unwrap();
    assert!(matches!(
        contender.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(contender);
    println!("{LOCK_OBSERVED}");
    io::stdout().flush().unwrap();

    let moods = Moods::builtin();
    let (preferences, ()) = Preferences::edit(&path, &moods, |prefs| {
        assert_eq!(
            prefs.ban.artists,
            ["Fixture Blocked"],
            "the waiting edit must read after the earlier writer publishes"
        );
        prefs.add("favor.genres", "jazz", &moods)
    })
    .unwrap();
    assert_eq!(preferences.ban.artists, ["Fixture Blocked"]);
    assert_eq!(preferences.favor.genres, ["jazz"]);
}

#[test]
fn snapshot_publication_does_not_reuse_an_unfinished_legacy_staging_file() {
    let path = path("preference-staging");
    let legacy = path.with_extension("toml.tmp");
    fs::write(&legacy, "another writer's unfinished document").unwrap();
    let mut preferences = Preferences::default();
    preferences.ban.artists.push("Fixture Blocked".into());
    preferences.save(&path).unwrap();
    assert_eq!(
        Preferences::load(&path, &Moods::builtin()).unwrap(),
        preferences
    );
    assert_eq!(
        fs::read_to_string(&legacy).unwrap(),
        "another writer's unfinished document"
    );
    let staging: Vec<_> = fs::read_dir(parent(&path))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .collect();
    assert_eq!(staging, [legacy]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn lock_and_publication_failures_leave_existing_files_untouched() {
    let path = path("preference-filesystem-errors");
    let blocked = path.with_file_name("not-a-directory");
    fs::write(&blocked, "keep this file").unwrap();
    assert!(matches!(
        Preferences::default().save(&blocked.join("preferences.toml")),
        Err(PreferenceWriteError::Lock { .. })
    ));
    assert_eq!(fs::read_to_string(&blocked).unwrap(), "keep this file");

    fs::create_dir(&path).unwrap();
    fs::write(path.join("retained"), "keep this child").unwrap();
    assert!(matches!(
        Preferences::default().save(&path),
        Err(PreferenceWriteError::Publish {
            published: false,
            ..
        })
    ));
    assert_eq!(
        fs::read_to_string(path.join("retained")).unwrap(),
        "keep this child"
    );
}
