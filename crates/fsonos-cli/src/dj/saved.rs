//! The daemon's running DJ feeds, kept across a restart: `dj-feeds.json` in
//! the data directory, rewritten whenever a feed changes (a start, skip or
//! stop, a move, a new track) and read back when serve starts. A feed read
//! back checks the queue at its first playback event
//! (`QueueFeed::restore`), so one whose queue the owner cleared or replaced
//! meanwhile lets go.

use fsonos_spotify::dj::DjConfig;
use fsonos_spotify::feed::{FeedSnapshot, QueueFeed};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;

/// The file's name inside the data directory.
pub const FEEDS_FILE: &str = "dj-feeds.json";

#[derive(Serialize, Deserialize)]
struct Saved {
    feeds: Vec<FeedSnapshot>,
}

/// The feeds `path` holds, by coordinator; none when it is missing, or
/// unreadable (logged: the DJ then starts afresh).
pub(crate) fn load(path: &Path, seed: u64) -> HashMap<PlayerId, QueueFeed> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!(error = %e, "the DJ's saved feeds are unreadable; it starts afresh");
            return HashMap::new();
        }
    };
    match serde_json::from_str::<Saved>(&text) {
        Ok(saved) => saved
            .feeds
            .into_iter()
            .zip(std::iter::successors(Some(seed), |s| {
                Some(s.wrapping_add(1))
            }))
            .map(|(snapshot, seed)| {
                let feed = QueueFeed::restore(snapshot, DjConfig::default(), seed);
                (feed.coordinator(), feed)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "the DJ's saved feeds are malformed; it starts afresh");
            HashMap::new()
        }
    }
}

/// The running ones of `feeds` as the file holds them, in coordinator
/// order (so the same feeds always read the same).
pub(crate) fn encode(feeds: &HashMap<PlayerId, QueueFeed>) -> String {
    let mut feeds: Vec<FeedSnapshot> = feeds.values().filter_map(QueueFeed::snapshot).collect();
    feeds.sort_by(|a, b| a.zone.cmp(&b.zone));
    serde_json::to_string_pretty(&Saved { feeds }).unwrap_or_default()
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
    use fsonos_spotify::dj::PickReason;
    use fsonos_spotify::feed::{Movement, QueuedWork};

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
        }
    }

    #[test]
    fn running_feeds_are_saved_whole_and_read_back_by_coordinator() {
        let dir = std::env::temp_dir().join(format!("fsonos-dj-feeds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FEEDS_FILE);
        let _ = std::fs::remove_file(&path);
        assert!(load(&path, 1).is_empty(), "no file: nothing to take up");

        let kitchen = "RINCON_000000000000021400";
        let office = "RINCON_000000000000011400";
        let feeds: HashMap<PlayerId, QueueFeed> = [kitchen, office]
            .into_iter()
            .map(|zone| {
                let feed = QueueFeed::restore(snapshot(zone), DjConfig::default(), 1);
                (PlayerId(zone.into()), feed)
            })
            .collect();
        let text = encode(&feeds);
        assert!(
            text.find(office) < text.find(kitchen),
            "in coordinator order: {text}"
        );
        write(&path, &text).unwrap();
        assert!(!path.with_extension("json.partial").exists());

        let read = load(&path, 2);
        assert_eq!(read.len(), 2);
        let back = &read[&PlayerId(kitchen.into())];
        assert!(back.is_active());
        assert_eq!(back.snapshot(), Some(snapshot(kitchen)), "whole");
        assert_eq!(encode(&read), text, "read back, it saves the same");

        // A stopped feed isn't saved.
        let mut stopped = read;
        stopped.insert(
            PlayerId(kitchen.into()),
            QueueFeed::new(&PlayerId(kitchen.into()), DjConfig::default(), 3),
        );
        let only_office = encode(&stopped);
        assert!(only_office.contains(office) && !only_office.contains(kitchen));

        // A malformed file is ignored.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load(&path, 4).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
