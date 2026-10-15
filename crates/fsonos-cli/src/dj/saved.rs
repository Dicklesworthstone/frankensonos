//! The daemon's running DJ feeds, kept across a restart: `dj-feeds.json` in
//! the data directory, rewritten whenever a feed or its group changes. Feed
//! snapshots and the last known group membership are replaced together so a
//! restart can recover a queue whose old coordinator has disappeared. Queue
//! ownership is still verified against the speakers before any handover.

use super::handover::{Origin, Tracker};
use fsonos_spotify::dj::DjConfig;
use fsonos_spotify::feed::{FeedSnapshot, QueueFeed};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// The file's name inside the data directory.
pub const FEEDS_FILE: &str = "dj-feeds.json";

static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
struct Saved {
    feeds: Vec<FeedSnapshot>,
    /// Older files contain only feeds; they still recover when the former
    /// coordinator remains listed in its successor's group.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    groups: BTreeMap<String, Origin>,
}

/// Restore feeds and their group evidence from the same file revision.
/// Missing or unreadable files give an empty state (failures are logged).
pub(super) fn load(path: &Path, seed: u64) -> (HashMap<PlayerId, QueueFeed>, Tracker) {
    let empty = || (HashMap::new(), Tracker::default());
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return empty(),
        Err(e) => {
            tracing::warn!(error = %e, "the DJ's saved feeds are unreadable; it starts afresh");
            return empty();
        }
    };
    match serde_json::from_str::<Saved>(&text) {
        Ok(saved) => {
            // A feed with none of its works on the queue owns nothing there
            // (a start that failed, saved before such a feed let go): taken
            // up, it would fill the queue whenever a pick became possible.
            let (feeds, empty): (Vec<_>, Vec<_>) = saved
                .feeds
                .into_iter()
                .partition(|f| !(f.queued.is_empty() && f.played.is_empty()));
            if !empty.is_empty() {
                tracing::info!(
                    feeds = empty.len(),
                    "the DJ lets go of saved feeds with nothing on the queue"
                );
            }
            let feeds = feeds
                .into_iter()
                .zip(std::iter::successors(Some(seed), |s| {
                    Some(s.wrapping_add(1))
                }))
                .map(|(snapshot, seed)| {
                    let feed = QueueFeed::restore(snapshot, DjConfig::default(), seed);
                    (feed.coordinator(), feed)
                })
                .collect();
            let tracker = Tracker::restore(saved.groups, &feeds);
            // The file says what runs: without them, so they are let go once.
            if !empty.is_empty()
                && let Err(e) = write(path, &encode(&feeds, &tracker))
            {
                tracing::warn!(error = %e, "the DJ's saved feeds were not rewritten");
            }
            (feeds, tracker)
        }
        Err(e) => {
            tracing::warn!(error = %e, "the DJ's saved feeds are malformed; it starts afresh");
            empty()
        }
    }
}

/// Running feeds and their group evidence, ordered by coordinator. Stopped
/// feeds and their former groups are omitted. Retry deadlines never persist.
pub(super) fn encode(feeds: &HashMap<PlayerId, QueueFeed>, tracker: &Tracker) -> String {
    let groups = tracker.snapshot(feeds);
    let mut feeds: Vec<FeedSnapshot> = feeds.values().filter_map(QueueFeed::snapshot).collect();
    feeds.sort_by(|a, b| a.zone.cmp(&b.zone));
    serde_json::to_string_pretty(&Saved { feeds, groups }).unwrap_or_default()
}

/// Replace `path` with a complete, durable revision. Each writer owns a
/// private staging file in the same directory: overlapping CLI/daemon writes
/// cannot truncate or rename one another's temporary file. The last rename
/// wins, but it always publishes one whole revision, never a mixture.
///
/// Sync the contents before publishing, then the directory entry on Unix.
/// An error after rename means publication succeeded but durability could not
/// be acknowledged; callers must retain their pending save and retry.
pub(crate) fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(ErrorKind::InvalidInput, "saved state needs a file name")
    })?;
    for _ in 0..128 {
        let mut temporary = std::ffi::OsString::from(".");
        temporary.push(name);
        temporary.push(format!(
            ".{}.{}.partial",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let partial = parent.join(temporary);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // These files contain household identities and listening state.
            options.mode(0o600);
        }
        let mut file = match options.open(&partial) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let flushed = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all());
        // Close before rename, including on platforms that deny renaming an
        // open file. A failed write never replaces the last good revision.
        drop(file);
        let result = flushed.and_then(|()| {
            std::fs::rename(&partial, path)?;
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        });
        if result.is_err() {
            // Only our create_new-owned staging file is eligible for cleanup.
            // A stale staging file from another process is never touched.
            let _ = std::fs::remove_file(&partial);
        }
        return result;
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a private saved-state staging file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::HouseholdState;
    use fsonos_spotify::dj::PickReason;
    use fsonos_spotify::feed::{Movement, QueuedWork};
    use fsonos_types::{HouseholdId, ZoneGroup};

    fn snapshot(zone: &str) -> FeedSnapshot {
        let movement = |n: u32| Movement {
            uri: format!("spotify:track:simsaved{n}0000000000000"),
            title: format!("Movement {n}"),
            position: Some(n),
            added: true,
        };
        FeedSnapshot {
            zone: zone.into(),
            earlier: vec!["RINCON_000000000000091400".into()],
            lookahead: 1,
            queued: vec![QueuedWork {
                title: "Symphony No. 5".into(),
                composer: "Ludwig van Beethoven".into(),
                work_key: "beethoven:symphony-5".into(),
                movements: vec![movement(1), movement(2)],
                reason: PickReason {
                    factors: Vec::new(),
                    relaxed: Vec::new(),
                    summary: "first of the evening".into(),
                },
            }],
            playing: Some(1),
            recorded: Some((1, "spotify:track:simsaved10000000000000".into())),
            played: Vec::new(),
        }
    }

    #[test]
    fn running_feeds_are_saved_whole_and_read_back_by_coordinator() {
        let dir = std::env::temp_dir().join(format!("fsonos-dj-feeds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FEEDS_FILE);
        let _ = std::fs::remove_file(&path);
        assert!(load(&path, 1).0.is_empty(), "no file: nothing to take up");

        let kitchen = "RINCON_000000000000021400";
        let office = "RINCON_000000000000011400";
        let feeds: HashMap<PlayerId, QueueFeed> = [kitchen, office]
            .into_iter()
            .map(|zone| {
                let feed = QueueFeed::restore(snapshot(zone), DjConfig::default(), 1);
                (PlayerId(zone.into()), feed)
            })
            .collect();
        let tracker = Tracker::default();
        let text = encode(&feeds, &tracker);
        assert!(
            text.find(office) < text.find(kitchen),
            "in coordinator order: {text}"
        );
        write(&path, &text).unwrap();
        assert!(!path.with_extension("json.partial").exists());

        let (read, tracker) = load(&path, 2);
        assert_eq!(read.len(), 2);
        let back = &read[&PlayerId(kitchen.into())];
        assert!(back.is_active());
        assert_eq!(back.snapshot(), Some(snapshot(kitchen)), "whole");
        assert_eq!(
            encode(&read, &tracker),
            text,
            "read back, it saves the same"
        );

        // A stopped feed isn't saved.
        let mut stopped = read;
        stopped.insert(
            PlayerId(kitchen.into()),
            QueueFeed::new(&PlayerId(kitchen.into()), DjConfig::default(), 3),
        );
        let only_office = encode(&stopped, &tracker);
        assert!(only_office.contains(office) && !only_office.contains(kitchen));

        // A malformed file is ignored.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path, 4).0.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn group_evidence_round_trips_with_feeds_and_old_files_remain_readable() {
        let dir = std::env::temp_dir().join(format!("fsonos-dj-groups-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FEEDS_FILE);
        let id = PlayerId("test-origin".into());
        let other = PlayerId("test-successor".into());
        let mut feeds = HashMap::from([(
            id.clone(),
            QueueFeed::restore(snapshot(&id.0), DjConfig::default(), 1),
        )]);
        let mut tracker = Tracker::default();
        let mut house = HouseholdState {
            id: Some(HouseholdId("test-household".into())),
            groups: vec![ZoneGroup {
                coordinator: id.clone(),
                members: vec![other, id.clone()],
            }],
            ..HouseholdState::default()
        };
        tracker.remember(std::slice::from_ref(&house), &id);
        let text = encode(&feeds, &tracker);
        write(&path, &text).unwrap();
        let (restored, known) = load(&path, 9);
        assert_eq!(encode(&restored, &known), text);
        assert_eq!(known.snapshot(&restored).len(), 1);

        // A differently ordered topology is the same durable evidence.
        house.groups[0].members.reverse();
        tracker.remember(&[house], &id);
        assert_eq!(encode(&feeds, &tracker), text);
        feeds.insert(id.clone(), QueueFeed::new(&id, DjConfig::default(), 1));
        let stopped: Saved = serde_json::from_str(&encode(&feeds, &tracker)).unwrap();
        assert!(stopped.feeds.is_empty() && stopped.groups.is_empty());

        // The original JSON shape has no groups field.
        let legacy = serde_json::json!({"feeds": [snapshot(&id.0)]});
        write(&path, &legacy.to_string()).unwrap();
        let (old, unknown) = load(&path, 9);
        assert_eq!(old[&id].snapshot(), Some(snapshot(&id.0)));
        assert!(unknown.snapshot(&old).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_feed_with_nothing_on_the_queue_is_not_taken_up() {
        let dir = scratch("empty");
        let path = dir.join(FEEDS_FILE);
        let kitchen = "RINCON_000000000000021400";
        let office = "RINCON_000000000000011400";
        // As found on a real Mac: a start that queued nothing, saved days
        // earlier, before such a feed let go of the queue.
        let empty = FeedSnapshot {
            queued: Vec::new(),
            recorded: None,
            ..snapshot(kitchen)
        };
        let text = serde_json::json!({"feeds": [empty, snapshot(office)]});
        write(&path, &text.to_string()).unwrap();
        let (feeds, tracker) = load(&path, 1);
        assert_eq!(feeds.keys().collect::<Vec<_>>(), [&PlayerId(office.into())]);
        assert!(!encode(&feeds, &tracker).contains(kitchen));
        // The file is rewritten without it, so a restart lets go once.
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert_eq!(rewritten, encode(&feeds, &tracker));
        assert!(!rewritten.contains(kitchen) && rewritten.contains(office));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsonos-durable-{label}-{}-{}",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn concurrent_saves_publish_whole_revisions_without_shared_staging() {
        let dir = scratch("concurrent");
        let path = dir.join(FEEDS_FILE);
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            for writer in 0..16 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    let text = serde_json::json!({
                        "writer": writer,
                        "payload": writer.to_string().repeat(8192)
                    })
                    .to_string();
                    barrier.wait();
                    write(path, &text).unwrap();
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
        let writer = saved["writer"].as_u64().unwrap();
        assert!(writer < 16);
        assert_eq!(saved["payload"], writer.to_string().repeat(8192));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn abandoned_legacy_staging_is_neither_reused_nor_deleted() {
        let dir = scratch("abandoned");
        let path = dir.join(FEEDS_FILE);
        let legacy = path.with_extension("json.partial");
        std::fs::write(&legacy, "another writer's unfinished revision").unwrap();
        write(&path, "old complete revision").unwrap();
        write(&path, "new complete revision").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new complete revision"
        );
        assert_eq!(
            std::fs::read_to_string(&legacy).unwrap(),
            "another writer's unfinished revision"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_publication_cleans_only_its_own_staging_file() {
        let dir = scratch("failure");
        let path = dir.join(FEEDS_FILE);
        std::fs::create_dir(&path).unwrap();
        let previous = path.join("previous");
        std::fs::write(&previous, "keep this").unwrap();
        assert!(write(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(previous).unwrap(), "keep this");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_household_state_is_private_on_creation_and_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("private");
        let path = dir.join(FEEDS_FILE);
        write(&path, "first").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write(&path, "second").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
