//! Queue ordering controls, without a source reset, seek, or Play. The DJ
//! owns sequential order while it feeds a group; manual shuffle/repeat must
//! not fight that worker. A mode write is sent once and confirmed by reads.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::CoreError;
use fsonos_core::actions;
use fsonos_core::control;
use fsonos_core::policy::Client;
use fsonos_proto::ProtoError;
use fsonos_proto::control::get_media_info;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::ZoneGroupInfo;
use fsonos_types::{PlayMode, PlayerId};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use super::super::Surface;
use crate::failure::{ErrorCode, Failure};

pub const MODE_READ: &str = "get_play_mode";
pub const MODE_WRITE: &str = "set_play_mode";

/// A known coordinator setting. Unknown/unsupported reads return an error,
/// not an assumed NORMAL mode. `queue_active` includes a paused queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlayModeDto {
    pub zone: String,
    /// NORMAL, REPEAT_ALL, REPEAT_ONE, SHUFFLE, SHUFFLE_NOREPEAT,
    /// or SHUFFLE_REPEAT_ONE. SHUFFLE repeats the queue as well.
    pub mode: String,
    pub queue_active: bool,
    pub dj_active: bool,
}

impl PlayModeDto {
    #[must_use]
    pub fn text(&self) -> String {
        let source = if self.queue_active {
            "queue selected"
        } else {
            "another source selected"
        };
        let dj = if self.dj_active {
            "; DJ controls queue order"
        } else {
            ""
        };
        format!("{}: {} ({source}{dj}).\n", self.zone, self.mode)
    }
}

/// Set a complete shuffle/repeat combination, not a toggle. The optional
/// expected setting rejects a stale read before any mutation is attempted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayModeRequest {
    pub zone: String,
    /// One of the six exact wire mode names documented by `PlayModeDto`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_mode: Option<String>,
}

impl PlayModeRequest {
    fn checked(&self) -> Result<(PlayMode, Option<PlayMode>), Failure> {
        if self.zone.trim().is_empty() {
            return Err(Failure::invalid(
                "give a room whose queue mode should change",
            ));
        }
        Ok((
            mode(&self.mode)?,
            self.expected_mode.as_deref().map(mode).transpose()?,
        ))
    }
}

fn mode(value: &str) -> Result<PlayMode, Failure> {
    PlayMode::from_wire(value).ok_or_else(|| {
        Failure::invalid(format!("unrecognized queue play mode: {value:?}"))
            .with_suggestions(PlayMode::ALL.map(PlayMode::as_str))
            .with_hint("Choose a complete mode; SHUFFLE also repeats, SHUFFLE_NOREPEAT does not.")
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlayModeSetDto {
    pub done: String,
    pub state: PlayModeDto,
    pub changed: bool,
    /// The response to SetPlayMode failed, but the requested state was read
    /// back on the same queue/group. This is observation, not causal proof.
    pub recovered: bool,
}

impl PlayModeSetDto {
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}\n", self.done)
    }
}

fn protocol(error: ProtoError) -> Failure {
    Failure::from(CoreError::from(error))
}

fn changed() -> Failure {
    Failure::invalid("the queue source or play mode changed before the write; nothing was set")
        .with_hint("Read get_play_mode again before choosing the current group's setting.")
}

fn uncertain(error: impl std::fmt::Display) -> Failure {
    Failure::new(
        ErrorCode::UpnpFault,
        format!("play-mode outcome is uncertain: {error}; SetPlayMode was not replayed"),
    )
    .with_hint("Read get_play_mode again before deciding on another change; do not retry blindly.")
}

fn dj_order(wanted: PlayMode, active: bool) -> Result<(), Failure> {
    if active && wanted != PlayMode::Normal {
        return Err(
            Failure::invalid("the running DJ requires NORMAL queue order")
                .with_hint("Stop the DJ in this group before choosing shuffle or repeat."),
        );
    }
    Ok(())
}

impl Surface {
    pub fn get_play_mode(&self, client: &Client, zone: &str) -> Result<PlayModeDto, Failure> {
        self.guard(client).authorize(MODE_READ, true)?;
        let result = (|| {
            let (households, group) = self.queue_households(client, zone)?;
            let player = control::locate(&households, &group.coordinator)?;
            let mode = get_play_mode(&*self.transport, player.ip).map_err(protocol)?;
            let media = get_media_info(&*self.transport, player.ip).map_err(protocol)?;
            self.queue_group_unchanged(player.ip, &group)?;
            Ok(PlayModeDto {
                zone: player.room_name.clone(),
                mode: mode.as_str().into(),
                queue_active: media.uri == format!("x-rincon-queue:{}#0", player.id.0),
                dj_active: self.dj.as_ref().is_some_and(|dj| dj.feeds(&player.id)),
            })
        })();
        result.inspect_err(|failure| self.notice(failure))
    }

    /// Set only the ordering mode, never start playback to make it writable.
    /// A logged mutation keeps the usual playback snapshot for explicit undo;
    /// validation failures and no-ops carry no undo state.
    pub fn set_play_mode(
        &self,
        client: &Client,
        req: &PlayModeRequest,
    ) -> Result<PlayModeSetDto, Failure> {
        self.authorize_logged(client, MODE_WRITE)?;
        let mut undo = None;
        let result = self.change_play_mode(client, req, &mut undo);
        let (decision, text) = match &result {
            Ok(done) => ("allow".to_string(), done.done.clone()),
            Err(failure) => {
                self.notice(failure);
                ("allow".to_string(), format!("failed: {}", failure.detail))
            }
        };
        self.record(
            client,
            format!("{MODE_WRITE}: {} {}", req.zone, req.mode),
            decision,
            text,
            undo,
        );
        result
    }

    fn check_dj_mode(&self, coordinator: &PlayerId, wanted: PlayMode) -> Result<(), Failure> {
        dj_order(
            wanted,
            self.dj.as_ref().is_some_and(|dj| dj.feeds(coordinator)),
        )
    }

    fn mode_context(&self, host: IpAddr, group: &ZoneGroupInfo) -> Result<(), Failure> {
        self.queue_group_unchanged(host, group)?;
        let media = get_media_info(&*self.transport, host).map_err(protocol)?;
        if media.uri != format!("x-rincon-queue:{}#0", group.coordinator.0) {
            return Err(
                Failure::invalid("the group does not have its own queue selected").with_hint(
                    "Select the queue before changing shuffle/repeat; no source is started here.",
                ),
            );
        }
        Ok(())
    }

    fn change_play_mode(
        &self,
        client: &Client,
        req: &PlayModeRequest,
        undo: &mut Option<String>,
    ) -> Result<PlayModeSetDto, Failure> {
        let (wanted, expected) = req.checked()?;
        let (households, group) = self.queue_households(client, &req.zone)?;
        let player = control::locate(&households, &group.coordinator)?;
        let host = player.ip;
        let before = get_play_mode(&*self.transport, host).map_err(protocol)?;
        if expected.is_some_and(|expected| expected != before) {
            return Err(changed());
        }
        self.check_dj_mode(&player.id, wanted)?;
        self.mode_context(host, &group)?;
        if before == wanted {
            return Ok(PlayModeSetDto {
                done: format!(
                    "{}'s queue already uses {}",
                    player.room_name,
                    wanted.as_str()
                ),
                state: PlayModeDto {
                    zone: player.room_name.clone(),
                    mode: wanted.as_str().into(),
                    queue_active: true,
                    dj_active: self.dj.as_ref().is_some_and(|dj| dj.feeds(&player.id)),
                },
                changed: false,
                recovered: false,
            });
        }
        let (mut snaps, _) = if self.log.is_some() {
            actions::capture_zones(
                &*self.transport,
                &households,
                std::slice::from_ref(&player.id),
                self.now(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        // Capturing undo can involve several reads. Revalidate afterwards;
        // neither these checks nor expected_mode form a device transaction.
        self.mode_context(host, &group)?;
        if get_play_mode(&*self.transport, host).map_err(protocol)? != before {
            return Err(changed());
        }
        self.check_dj_mode(&player.id, wanted)?;
        // A transient failure of the snapshot's optional settings read must
        // not discard the mode that our mandatory reads just agreed on.
        for snap in &mut snaps {
            if matches!(
                &snap.source,
                fsonos_core::snapshot::SnapshotSource::Queue { .. }
            ) {
                snap.play_mode = Some(before);
            }
        }
        *undo = actions::before_state(&snaps);
        let sent = set_play_mode(&*self.transport, host, wanted);
        self.mode_context(host, &group).map_err(uncertain)?;
        let observed = get_play_mode(&*self.transport, host).map_err(uncertain)?;
        if observed != wanted {
            if observed == before
                && let Err(error @ ProtoError::SoapFault { .. }) = sent
            {
                *undo = None;
                return Err(protocol(error));
            }
            return Err(uncertain("the requested setting was not confirmed"));
        }
        let recovered = sent.is_err();
        // Undo compares the rooms with how this change left them.
        *undo = self.with_after(undo.take(), &snaps, &[]);
        let detail = if recovered {
            " (confirmed by read-back)"
        } else {
            ""
        };
        Ok(PlayModeSetDto {
            done: format!(
                "{}'s queue uses {}{detail}",
                player.room_name,
                wanted.as_str()
            ),
            state: PlayModeDto {
                zone: player.room_name.clone(),
                mode: wanted.as_str().into(),
                queue_active: true,
                dj_active: self.dj.as_ref().is_some_and(|dj| dj.feeds(&player.id)),
            },
            changed: true,
            recovered,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_all_modes_and_optional_expected_setting() {
        for wanted in PlayMode::ALL {
            let req = PlayModeRequest {
                zone: "Room".into(),
                mode: wanted.as_str().into(),
                expected_mode: Some("NORMAL".into()),
            };
            assert_eq!(req.checked().unwrap(), (wanted, Some(PlayMode::Normal)));
        }
        for invalid in ["", "NOT_IMPLEMENTED", "shuffle", "SHUFFLE_NO_REPEAT"] {
            assert!(mode(invalid).is_err());
        }
    }

    #[test]
    fn json_does_not_default_a_missing_mode_or_accept_unknown_preconditions() {
        let req: PlayModeRequest =
            serde_json::from_str(r#"{"zone":"Room","mode":"REPEAT_ONE"}"#).unwrap();
        assert_eq!(req.checked().unwrap(), (PlayMode::RepeatOne, None));
        for input in [
            r#"{"zone":"Room"}"#,
            r#"{"zone":"Room","mode":"NORMAL","expect_mode":"NORMAL"}"#,
        ] {
            assert!(serde_json::from_str::<PlayModeRequest>(input).is_err());
        }
        let blank = PlayModeRequest {
            zone: " ".into(),
            ..req
        };
        assert!(blank.checked().is_err());
    }

    #[test]
    fn a_running_dj_allows_only_sequential_order_but_manual_queues_allow_every_mode() {
        for mode in PlayMode::ALL {
            assert!(dj_order(mode, false).is_ok());
            assert_eq!(dj_order(mode, true).is_ok(), mode == PlayMode::Normal);
        }
    }

    #[test]
    fn unconfirmed_writes_are_not_retryable() {
        assert!(!uncertain("response lost").retryable());
        assert!(
            uncertain("response lost")
                .hint
                .contains("do not retry blindly")
        );
    }
}
