//! Act on one observed saved-playlist occurrence using speaker-owned metadata.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{actions, control, playlists};
use serde::{Deserialize, Serialize};

use super::{SonosPlaylistContentsRequest, Surface};
use crate::guard::Note;
use crate::plan::Command;
use crate::surface::{log_entry, note_unlogged};
use crate::{ErrorCode, Failure, OutcomeDto};

/// Immediate playback retains the existing queue. Queue modes never start it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SonosPlaylistTrackMode {
    Play,
    Next,
    Append,
}

impl std::str::FromStr for SonosPlaylistTrackMode {
    type Err = Failure;

    fn from_str(mode: &str) -> Result<Self, Self::Err> {
        match mode {
            "play" => Ok(Self::Play),
            "next" => Ok(Self::Next),
            "append" => Ok(Self::Append),
            _ => Err(Failure::invalid("mode must be play, next or append")),
        }
    }
}

/// A selection from `sonos_playlist_contents`, never a client-supplied URI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SonosPlaylistTrackRequest {
    pub zone: String,
    /// Exact household-local SQ: id from the observed contents page.
    pub playlist: String,
    /// The original one-based occurrence, including repeated tracks.
    pub position: u32,
    /// Opaque revision from the contents page where this occurrence was read.
    pub expected_revision: String,
    pub mode: SonosPlaylistTrackMode,
}

impl SonosPlaylistTrackRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        if self.position > fsonos_proto::content::MAX_BROWSE_OBJECTS {
            return Err(Failure::invalid(
                "position exceeds the saved playlist occurrence limit",
            ));
        }
        let offset = self.position.checked_sub(1).ok_or_else(|| {
            Failure::invalid("position must name a one-based saved playlist occurrence")
        })?;
        SonosPlaylistContentsRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            offset,
            limit: 1,
            expected_revision: Some(self.expected_revision.clone()),
        }
        .validate()
    }
}

/// The resolved selected track and the effect of its single requested action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SonosPlaylistTrackDto {
    pub done: String,
    pub zone: String,
    pub playlist: String,
    pub position: u32,
    pub title: String,
    pub mode: SonosPlaylistTrackMode,
    pub changed: bool,
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Confirmed insertion position; absent for direct playback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing_queue: Option<bool>,
}

impl SonosPlaylistTrackDto {
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!("{}\n", self.done);
        for note in &self.notes {
            text.push_str(&note.detail);
            text.push('\n');
        }
        text
    }
}

impl Surface {
    /// Recheck the exact saved occurrence before entering existing playback or
    /// native enqueue orchestration. No catalog/title resolution is repeated.
    pub fn sonos_playlist_track(
        &self,
        client: &Client,
        req: &SonosPlaylistTrackRequest,
    ) -> Result<SonosPlaylistTrackDto, Failure> {
        req.validate()?;
        let mut title = String::new();
        let (done, zone, changed, notes, first, count, playing_queue) = match req.mode {
            SonosPlaylistTrackMode::Play => {
                let (outcome, zone, selected_title) =
                    self.play_selected_sonos_track(client, req)?;
                title = selected_title;
                (
                    outcome.done,
                    zone,
                    outcome.changed,
                    outcome.notes,
                    None,
                    None,
                    None,
                )
            }
            SonosPlaylistTrackMode::Next | SonosPlaylistTrackMode::Append => {
                let selection = format!("{} occurrence {}", req.playlist.trim(), req.position);
                let queued = self.queue_native_selected(
                    client,
                    req.zone.trim(),
                    &selection,
                    req.mode == SonosPlaylistTrackMode::Next,
                    "saved playlist track",
                    |transport, households, coordinator| {
                        let favorite = playlists::select_track(
                            transport,
                            households,
                            coordinator,
                            req.playlist.trim(),
                            req.position,
                            &req.expected_revision,
                        )?;
                        title.clone_from(&favorite.title);
                        Ok(favorite)
                    },
                )?;
                (
                    queued.done,
                    queued.zone,
                    true,
                    Vec::new(),
                    Some(queued.first),
                    Some(queued.count),
                    Some(queued.playing_queue),
                )
            }
        };
        Ok(SonosPlaylistTrackDto {
            done,
            zone,
            playlist: req.playlist.trim().into(),
            position: req.position,
            title,
            mode: req.mode,
            changed,
            notes,
            first,
            count,
            playing_queue,
        })
    }

    /// A revision-bound occurrence may not follow generic coordinator healing.
    /// Reconfirm it around snapshot and cap reads, then send native stages once.
    fn play_selected_sonos_track(
        &self,
        client: &Client,
        req: &SonosPlaylistTrackRequest,
    ) -> Result<(OutcomeDto, String, String), Failure> {
        self.authorize_logged(client, "play_favorite")?;
        let guard = self.guard(client);
        let (households, group) = self.queue_households(client, req.zone.trim())?;
        let coordinator = group.coordinator.clone();
        let player = control::locate(&households, &coordinator)?;
        let zone = player.room_name.clone();
        let select = || -> Result<_, Failure> {
            Ok(playlists::select_track(
                &*self.transport,
                &households,
                &coordinator,
                req.playlist.trim(),
                req.position,
                &req.expected_revision,
            )?)
        };
        let selected = select().inspect_err(|failure| self.notice(failure))?;
        let title = selected.title.clone();
        let command = Command::PlayFavorite {
            member: coordinator.clone(),
            coordinator: coordinator.clone(),
            favorite: selected.clone(),
        };
        let (snaps, missed) = if self.log.is_some() {
            actions::capture_zones(
                &*self.transport,
                &households,
                std::slice::from_ref(&coordinator),
                self.now(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let sessions = self.sessions_before(&command);
        let check = || -> Result<(), Failure> {
            if select()? != selected {
                return Err(Failure::new(
                    ErrorCode::QueueChanged,
                    "the selected saved playlist track changed while playback was prepared",
                )
                .with_hint(
                    "Read the saved playlist again and select its current occurrence and revision.",
                ));
            }
            self.queue_group_unchanged(player.ip, &group).map_err(|failure| {
                if failure.code == ErrorCode::NotCoordinator {
                    Failure::new(ErrorCode::QueueChanged, "the saved playlist target's group changed while playback was prepared")
                        .with_hint("Read the saved playlist in the current group before selecting a track.")
                } else {
                    failure
                }
            })
        };
        let result = (|| {
            // A stale selection must not lower volumes or cancel a fade.
            check()?;
            let mut notes = self.lower_before_playing(&households, &guard, &command)?;
            // Policy and snapshot reads cannot authorize an old occurrence on
            // another group. No readdressing or coordinator retry follows.
            check()?;
            if let Some(sleep) = &self.sleep {
                sleep.playback_changing(&coordinator);
            }
            let mut outcome = crate::execute::execute_guarded(
                &*self.transport,
                &households,
                &guard,
                command.clone(),
            )?;
            self.remember_play(&households, &command);
            notes.append(&mut outcome.notes);
            outcome.notes = notes;
            outcome.done = format!(
                "playing saved playlist track {} {:?} in {}'s group",
                req.position, title, zone
            );
            Ok(outcome)
        })();
        if let Err(failure) = &result {
            self.notice(failure);
        }
        let before = actions::before_state_with(&snaps, &sessions);
        let (decision, text, before) = log_entry(&result, before, missed.len());
        let before = self.with_after(before, &snaps, &[]);
        let logged = self.record(
            client,
            format!(
                "play_favorite: {zone} saved playlist {} occurrence {}",
                req.playlist.trim(),
                req.position
            ),
            decision,
            text,
            before,
        );
        note_unlogged(result, logged).map(|outcome| (outcome, zone, title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> SonosPlaylistTrackRequest {
        SonosPlaylistTrackRequest {
            zone: "Den@S1".into(),
            playlist: "SQ:7".into(),
            position: 3,
            expected_revision: "sonos-playlist-v1-observed".into(),
            mode: SonosPlaylistTrackMode::Next,
        }
    }

    #[test]
    fn selected_tracks_require_exact_identity_occurrence_and_observation() {
        request().validate().unwrap();
        let mut invalid = request();
        invalid.position = 0;
        assert!(invalid.validate().is_err());
        invalid.position = fsonos_proto::content::MAX_BROWSE_OBJECTS + 1;
        assert!(invalid.validate().is_err());
        for id in ["Evening", "7", "SQ:", "SQ:7/3", "FV:2/7", "SQ:-1"] {
            assert!(
                SonosPlaylistTrackRequest {
                    playlist: id.into(),
                    ..request()
                }
                .validate()
                .is_err()
            );
        }
        for revision in [
            String::new(),
            " ".into(),
            "old\nnew".into(),
            "x".repeat(4097),
        ] {
            assert!(
                SonosPlaylistTrackRequest {
                    expected_revision: revision,
                    ..request()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            SonosPlaylistTrackRequest {
                zone: " ".into(),
                ..request()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn request_refuses_raw_metadata_missing_proof_and_ambiguous_modes() {
        let valid = serde_json::to_value(request()).unwrap();
        for field in ["mode", "position", "playlist", "expected_revision"] {
            let mut value = valid.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<SonosPlaylistTrackRequest>(value).is_err());
        }
        for field in ["metadata", "uri", "source_uri", "title"] {
            let mut value = valid.clone();
            value[field] = json!("untrusted");
            assert!(serde_json::from_value::<SonosPlaylistTrackRequest>(value).is_err());
        }
        for mode in ["", "PLAY", "replace", "queue", "next "] {
            let mut value = valid.clone();
            value["mode"] = json!(mode);
            assert!(serde_json::from_value::<SonosPlaylistTrackRequest>(value).is_err());
            assert!(mode.parse::<SonosPlaylistTrackMode>().is_err());
        }
        for (text, mode) in [
            ("play", SonosPlaylistTrackMode::Play),
            ("next", SonosPlaylistTrackMode::Next),
            ("append", SonosPlaylistTrackMode::Append),
        ] {
            assert_eq!(text.parse::<SonosPlaylistTrackMode>().unwrap(), mode);
        }
    }
}
