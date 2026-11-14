//! One DJ request over independent groups. Readiness precedes every batch
//! mutation; each execution still checks the originally selected audience.

use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{Discovered, HttpBody, ProtoError, Transport, mdns, ssdp};
use std::cell::RefCell;
use std::net::IpAddr;
use std::time::Duration;

use super::{Surface, affected, no_dj_store};
use crate::dj::DjSpeakers;
use crate::failure::{ErrorCode, Failure};
use crate::guard::Guard;
use crate::OutcomeDto;
use crate::plan::{Command, DjAction};

pub(super) fn is_set(commands: &[Command]) -> bool {
    commands.len() > 1
        && commands
            .iter()
            .all(|command| matches!(command, Command::Dj { .. } | Command::DjSteer { .. }))
}

pub(super) fn label(households: &[HouseholdState], command: &Command) -> String {
    let Some(coordinator) = command.addressed() else {
        return "unknown group".into();
    };
    let labels = fsonos_core::rooms::household_labels(households);
    households
        .iter()
        .enumerate()
        .find_map(|(index, house)| {
            house
                .player(coordinator)
                .map(|player| format!("{}@{}", player.room_name, labels[index]))
        })
        .unwrap_or_else(|| coordinator.0.clone())
}

impl Surface {
    /// A batch keeps its audience through cap reads and engine lock waits.
    /// The same cap and DJ bookkeeping paths run over a fenced transport;
    /// session-only mutations also recheck after the engine takes its lock.
    pub(super) fn execute_dj_member(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
    ) -> Result<OutcomeDto, Failure> {
        let fence = AudienceTransport {
            surface: self, households, command, refused: RefCell::new(None),
        };
        let result = (|| {
            fence.check()?;
            if let Some(sleep) = &self.sleep {
                for coordinator in affected(households, command) {
                    sleep.playback_changing(&coordinator);
                }
            }
            let mut lowered = self.lower_before_playing_on(&fence, households, guard, command)?;
            let at = DjSpeakers {
                transport: &fence,
                households,
                coordinator: command.addressed().expect("a DJ command has a group"),
            };
            let mut outcome = self.carry_out_dj(at, guard, command, &|| fence.check())?;
            lowered.notes.append(&mut outcome.notes);
            outcome.notes = lowered.notes;
            Ok(outcome)
        })();
        // The transport returns a definite local refusal to the lower layers,
        // while the caller receives the original group-specific failure.
        fence.refused.into_inner().map_or(result, Err)
    }

    /// The real engine validates library, renderer and exact prospective
    /// direction without changing its feed, RNG, store or speakers. Check all
    /// selected groups and report every result if any prerequisite fails.
    pub(super) fn prepare_dj_set(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        commands: &[Command],
    ) -> Result<(), Failure> {
        if !is_set(commands) {
            return Ok(());
        }
        let dj = self.dj_engine()?;
        // Stop and skip need no selectable library or renderer refresh. Their
        // per-group execution can continue even when another group is idle.
        if !commands.iter().any(|command| {
            matches!(
                command,
                Command::Dj { action: DjAction::Start, .. } | Command::DjSteer { .. }
            )
        }) {
            return Ok(());
        }
        let results = self
            .with_dj_store(|store| {
                Ok(commands
                    .iter()
                    .map(|command| {
                        let ready = self.confirm_dj_group(households, command).and_then(|()| {
                            // Caps are part of readiness: a known unreadable
                            // room must not let other groups start first.
                            for (room, cap, _) in guard.playback_caps(households, command) {
                                fsonos_core::control::volume(
                                    &*self.transport, households, &room.primary,
                                ).map_err(|error| Failure::new(
                                    ErrorCode::PlayerUnreachable,
                                    format!("could not read {}'s volume to hold it to its cap of {cap}: {error}", room.name),
                                ))?;
                            }
                            let at = DjSpeakers {
                                transport: &*self.transport,
                                households,
                                coordinator: command.addressed().expect("a DJ command has a group"),
                            };
                            match command {
                                Command::Dj { action: DjAction::Start, .. } => {
                                    dj.start_ready(at, store, None, &*self.clock)
                                }
                                Command::DjSteer { steer, start: true, .. } => {
                                    dj.start_ready(at, store, Some(steer), &*self.clock)
                                }
                                Command::DjSteer { steer, start: false, .. } => {
                                    dj.steer_ready(at, store, steer, &*self.clock)
                                }
                                _ => Ok(()),
                            }
                        });
                        (label(households, command), ready)
                    })
                    .collect::<Vec<_>>())
            })?
            .ok_or_else(no_dj_store)?;
        let Some(failure) = results.iter().find_map(|(_, result)| result.as_ref().err()) else {
            return Ok(());
        };
        let detail = results
            .iter()
            .map(|(group, result)| match result {
                Ok(()) => format!("{group}: ready"),
                Err(failure) => format!("{group}: {} ({})", failure.detail, failure.code.as_str()),
            })
            .collect::<Vec<_>>()
            .join("; ");
        Err(Failure::new(
            failure.code,
            format!("No DJ commands started; batch readiness failed. {detail}"),
        )
        .with_hint("Resolve the reported prerequisites and read the selected groups before a new request."))
    }

    /// A slow preparation cannot turn an old coordinator into permission to
    /// act on an unrelated new group. Do not redirect or retry a batch member.
    pub(super) fn confirm_dj_group(
        &self,
        households: &[HouseholdState],
        command: &Command,
    ) -> Result<(), Failure> {
        let changed = || {
            Failure::new(
                ErrorCode::NotCoordinator,
                format!("{}'s selected group changed before the DJ command", label(households, command)),
            )
            .with_hint("Read the groups again; do not replay the whole batch.")
        };
        let coordinator = command.addressed().ok_or_else(changed)?;
        let house = households
            .iter()
            .find(|house| house.player(coordinator).is_some())
            .ok_or_else(changed)?;
        let player = house.player(coordinator).ok_or_else(changed)?;
        let before = house
            .groups
            .iter()
            .find(|group| group.coordinator == *coordinator)
            .ok_or_else(changed)?;
        let topology = get_zone_group_state(&*self.transport, player.ip)
            .map_err(CoreError::from)?;
        let mut current = house.clone();
        current.apply_topology(&topology);
        let after = current
            .groups
            .iter()
            .find(|group| group.coordinator == *coordinator)
            .ok_or_else(changed)?;
        let members = |group: &fsonos_types::ZoneGroup| {
            let mut members: Vec<_> = group.members.iter().map(|member| member.0.clone()).collect();
            members.sort_unstable();
            members
        };
        if members(before) != members(after)
            || before.members.iter().any(|id| {
                house.player(id).map(|player| player.ip)
                    != current.player(id).map(|player| player.ip)
            })
        {
            return Err(changed());
        }
        Ok(())
    }
}

/// The original group remains the target even if an engine waited for a
/// mutex, loaded a large catalog, or read several queue pages. A failed
/// proof is latched: no later write in this command may resume on that group.
struct AudienceTransport<'a> {
    surface: &'a Surface,
    households: &'a [HouseholdState],
    command: &'a Command,
    refused: RefCell<Option<Failure>>,
}

impl AudienceTransport<'_> {
    fn check(&self) -> Result<(), Failure> {
        if let Some(failure) = self.refused.borrow().clone() {
            return Err(failure);
        }
        let result = self.surface.confirm_dj_group(self.households, self.command);
        if let Err(failure) = &result {
            *self.refused.borrow_mut() = Some(failure.clone());
        }
        result
    }
}

impl Transport for AudienceTransport<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let verb = action.trim_matches('"').rsplit('#').next().unwrap_or("");
        if !verb.starts_with("Get") && !matches!(verb, "Browse" | "Search") {
            self.check().map_err(|failure| ProtoError::SoapFault {
                // A definite local refusal, never a lost reply or the Sonos
                // 800 fault that can trigger address/coordinator recovery.
                code: 403,
                reason: format!("DJ audience check refused before sending {verb}: {}", failure.detail),
            })?;
        }
        self.surface.transport.soap_post(host, path, action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.surface.transport.http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        self.surface.transport.http_get_bytes(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        self.surface.transport.ssdp_search(mx_secs, wait)
    }

    fn mdns_search(&self, wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
        self.surface.transport.mdns_search(wait)
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        self.surface.transport.discover(mx_secs, wait)
    }
}
