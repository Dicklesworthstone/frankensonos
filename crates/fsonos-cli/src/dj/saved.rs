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
use std::io::ErrorKind;
use std::path::Path;

/// The file's name inside the data directory.
pub const FEEDS_FILE: &str = "dj-feeds.json";

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
            let feeds = saved
                .feeds
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

/// Replace `path` with `text` whole: a crash mid-write leaves the old file.
pub(crate) fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, path)
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
        assert_eq!(encode(&read, &tracker), text, "read back, it saves the same");

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
}
