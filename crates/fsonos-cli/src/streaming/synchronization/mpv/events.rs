//! Track delivered playback discontinuities instead of discarding IPC events.

use std::collections::HashMap;

use anyhow::{Context as _, ensure};
use serde_json::{Value, json};

use super::Snapshot;

pub(super) const PROPERTIES: [&str; 5] = ["pause", "speed", "path", "audio-device", "audio-delay"];

#[derive(Default)]
pub(super) struct PlaybackEvents {
    revision: u64,
    identity: u64,
    properties: HashMap<String, Value>,
    overflowed: bool,
}

impl PlaybackEvents {
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn identity(&self) -> u64 {
        self.identity
    }

    pub(super) fn check(&self) -> anyhow::Result<()> {
        ensure!(
            !self.overflowed,
            "mpv lost playback events; synchronization stopped"
        );
        Ok(())
    }

    /// Late initial observer notifications must not look like owner changes.
    pub(super) fn seed(&mut self, original: &Snapshot) {
        for (name, value) in [
            ("pause", json!(original.paused)),
            ("speed", json!(1.0)),
            ("path", json!(original.media)),
            ("audio-device", json!(original.output)),
            ("audio-delay", json!(original.delay)),
        ] {
            self.properties.entry(name.into()).or_insert(value);
        }
    }

    pub(super) fn observe(&mut self, message: &Value) -> anyhow::Result<()> {
        let Some(event) = message.get("event").and_then(Value::as_str) else {
            return Ok(());
        };
        let identity_changed = match event {
            "queue-overflow" => {
                self.overflowed = true;
                return self.check();
            }
            "start-file" | "end-file" | "file-loaded" | "shutdown" => true,
            "seek" | "playback-restart" | "audio-reconfig" => false,
            "property-change" => {
                let Some(name) = message.get("name").and_then(Value::as_str) else {
                    return Ok(());
                };
                if !PROPERTIES.contains(&name) {
                    return Ok(());
                }
                let value = message.get("data").cloned().unwrap_or(Value::Null);
                if self.properties.get(name) == Some(&value) {
                    return Ok(());
                }
                let previous = self.properties.insert(name.into(), value);
                previous.is_some() && matches!(name, "path" | "audio-device")
            }
            _ => return Ok(()),
        };
        self.revision = self
            .revision
            .checked_add(1)
            .context("mpv event counter overflow")?;
        if identity_changed {
            self.identity = self
                .identity
                .checked_add(1)
                .context("mpv identity counter overflow")?;
        }
        self.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn original() -> Snapshot {
        Snapshot {
            media: "movie.mp4".into(),
            output: "auto".into(),
            delay: 0.1,
            paused: false,
        }
    }

    #[test]
    fn observer_initial_values_and_unrelated_events_do_not_invalidate_a_window() {
        let mut events = PlaybackEvents::default();
        events.seed(&original());
        for event in [
            json!({"event": "property-change", "name": "pause", "data": false}),
            json!({"event": "property-change", "name": "path", "data": "movie.mp4"}),
            json!({"event": "property-change", "name": "volume", "data": 50}),
            json!({"event": "tick"}),
            json!({"request_id": 3, "error": "success", "data": true}),
        ] {
            events.observe(&event).unwrap();
        }
        assert_eq!(events.revision(), 0);
        assert_eq!(events.identity(), 0);
    }

    #[test]
    fn pause_resume_and_seek_invalidate_without_relinquishing_the_media() {
        let mut events = PlaybackEvents::default();
        events.seed(&original());
        for event in [
            json!({"event": "property-change", "name": "pause", "data": true}),
            json!({"event": "property-change", "name": "pause", "data": false}),
            json!({"event": "seek"}),
            json!({"event": "playback-restart"}),
            json!({"event": "audio-reconfig"}),
        ] {
            events.observe(&event).unwrap();
        }
        assert_eq!(events.revision(), 5);
        assert_eq!(events.identity(), 0);
    }

    #[test]
    fn a_reload_or_transient_output_change_cannot_regain_old_ownership() {
        for event in ["start-file", "end-file", "file-loaded", "shutdown"] {
            let mut events = PlaybackEvents::default();
            events.observe(&json!({"event": event})).unwrap();
            assert_eq!(events.identity(), 1);
        }
        let mut events = PlaybackEvents::default();
        events.seed(&original());
        for output in ["other", "auto"] {
            events
                .observe(&json!({
                    "event": "property-change", "name": "audio-device", "data": output,
                }))
                .unwrap();
        }
        assert_eq!(events.identity(), 2);
    }

    #[test]
    fn lost_events_are_a_sticky_error_not_a_new_baseline() {
        let mut events = PlaybackEvents::default();
        assert!(events.observe(&json!({"event": "queue-overflow"})).is_err());
        events.seed(&original());
        assert!(events.check().is_err());
    }
}
