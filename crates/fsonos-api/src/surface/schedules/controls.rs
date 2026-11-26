//! Retain an ordinary schedule's physical targets from admission through every
//! write, including each frame of a configured volume fade.

use fsonos_core::HouseholdState;
use fsonos_core::schedule::ScheduleAction;
use fsonos_proto::{HttpBody, ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use super::Surface;
use crate::execute::{self, OutcomeDto};
use crate::failure::{ErrorCode, Failure};
use crate::guard::Guard;
use crate::plan::{self, Command, Rooms, TransportAction, VolumeScope};
use crate::request::{VolumeChange, VolumeRequest, ZoneRequest};

#[derive(Debug)]
pub(super) struct PreparedControl {
    pub(super) households: Vec<HouseholdState>,
    pub(super) commands: Vec<Command>,
}

pub(super) fn prepare(
    rooms: &Rooms<'_>,
    action: &ScheduleAction,
) -> Result<PreparedControl, Failure> {
    let commands = match action {
        ScheduleAction::Pause { room, fade_secs: 0 } => plan::plan_transport_set(
            rooms,
            &ZoneRequest { zone: room.clone() },
            TransportAction::Pause,
        )?,
        ScheduleAction::Volume { room, level } => plan::plan_volume_set(
            rooms,
            &VolumeRequest {
                zone: room.clone(),
                volume: Some(i64::from(*level)),
                delta: None,
                group: false,
            },
        )?,
        _ => {
            return Err(Failure::new(
                ErrorCode::Internal,
                "not an ordinary schedule control",
            ));
        }
    };
    Ok(PreparedControl {
        households: rooms.households.to_vec(),
        commands,
    })
}

impl Surface {
    pub(in crate::surface) fn execute_scheduled_control(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
    ) -> Result<OutcomeDto, Failure> {
        let audience = Audience::new(self, households, std::slice::from_ref(command))?;
        let result = (|| {
            audience.check_all()?;
            if let Some(sleep) = &self.sleep {
                for coordinator in super::super::affected(households, command) {
                    sleep.playback_changing(&coordinator);
                }
            }
            let (bounded, mut notes) = guard.bound(&audience, households, command.clone())?;
            let mut outcome = execute::execute(&audience, households, &bounded)?;
            notes.append(&mut outcome.notes);
            outcome.notes = notes;
            Ok(outcome)
        })();
        audience.finish(result)
    }
}

/// Only the retained endpoints may receive writes. A room volume may address
/// a member rather than its coordinator; both retain the whole original group
/// so a regroup or rename cannot silently change the policy audience.
pub(super) struct Audience<'a> {
    surface: &'a Surface,
    households: &'a [HouseholdState],
    targets: Vec<(IpAddr, &'a PlayerId)>,
    refused: Mutex<Option<Failure>>,
    wrote: AtomicBool,
}

impl<'a> Audience<'a> {
    pub(super) fn new(
        surface: &'a Surface,
        households: &'a [HouseholdState],
        commands: &[Command],
    ) -> Result<Self, Failure> {
        let mut targets = Vec::new();
        for command in commands {
            let target = match command {
                Command::Transport {
                    coordinator,
                    action: TransportAction::Pause,
                    ..
                } => coordinator,
                Command::Volume {
                    target,
                    scope: VolumeScope::Room,
                    change: VolumeChange::Set(_),
                    ..
                } => target,
                _ => {
                    return Err(Failure::new(
                        ErrorCode::Internal,
                        "an ordinary schedule retained an unsupported command",
                    ));
                }
            };
            let (host, coordinator) = households
                .iter()
                .find_map(|house| Some((house.player(target)?.ip, house.coordinator_of(target)?)))
                .ok_or_else(|| {
                    Failure::new(ErrorCode::NotReady, "scheduled target is unavailable")
                })?;
            targets.push((host, coordinator));
        }
        Ok(Self {
            surface,
            households,
            targets,
            refused: Mutex::new(None),
            wrote: AtomicBool::new(false),
        })
    }

    fn check(&self, host: IpAddr) -> Result<(), Failure> {
        if let Some(failure) = self
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Err(failure);
        }
        let result = self.targets.iter().find(|(ip, _)| *ip == host).map_or_else(
            || {
                Err(Failure::new(
                    ErrorCode::Internal,
                    "write outside the admitted schedule audience",
                ))
            },
            |(_, coordinator)| {
                self.surface
                    .confirm_reviewed_group(self.households, coordinator)
            },
        );
        if let Err(failure) = &result {
            *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure.clone());
        }
        result
    }

    pub(super) fn check_all(&self) -> Result<(), Failure> {
        for (host, _) in &self.targets {
            self.check(*host)?;
        }
        Ok(())
    }

    pub(super) fn finish<T>(&self, result: Result<T, Failure>) -> Result<T, Failure> {
        let refused = self
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let audience_lost = refused.is_some();
        refused.map_or(result, Err).map_err(|failure| {
            let unacknowledged = failure.upnp_code.is_none()
                && matches!(
                    failure.code,
                    ErrorCode::PlayerUnreachable | ErrorCode::UpnpFault
                );
            if !self.wrote.load(Ordering::Acquire) || !(audience_lost || unacknowledged) {
                return failure;
            }
            Failure::new(
                ErrorCode::OutcomeUncertain,
                format!(
                    "{}; scheduled speaker changes may have succeeded",
                    failure.detail
                ),
            )
            .with_hint(
                "Inspect the original rooms before another action; this occurrence will not be retried.",
            )
        })
    }
}

impl Transport for Audience<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let verb = action.trim_matches('"').rsplit('#').next().unwrap_or("");
        let writing = !verb.starts_with("Get") && !matches!(verb, "Browse" | "Search");
        if writing {
            self.check(host).map_err(|failure| ProtoError::SoapFault {
                code: 403,
                reason: format!("Scheduled audience refused {verb}: {}", failure.detail),
            })?;
            // Sending a write is enough to make later audience loss uncertain;
            // a missing acknowledgement cannot make a whole occurrence safe to retry.
            self.wrote.store(true, Ordering::Release);
        }
        self.surface.transport.soap_post(host, path, action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.surface.transport.http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        self.surface.transport.http_get_bytes(url)
    }
}
