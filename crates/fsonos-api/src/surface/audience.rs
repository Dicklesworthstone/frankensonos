//! Keep a reviewed physical audience across slow preparation and every write.

use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{Discovered, HttpBody, ProtoError, Transport, mdns, soap, ssdp};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

use super::Surface;
use crate::failure::{ErrorCode, Failure};

impl Surface {
    /// The original coordinator, physical membership and addresses must all
    /// still agree. A reviewed action never heals onto an unreviewed group.
    pub(super) fn confirm_reviewed_group(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
    ) -> Result<(), Failure> {
        let changed = || {
            Failure::new(
                ErrorCode::NotCoordinator,
                format!(
                    "{}'s reviewed physical group changed before the command",
                    coordinator.0
                ),
            )
            .with_hint("Read the affected groups again; do not replay the whole request.")
        };
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
        let topology =
            get_zone_group_state(&*self.transport, player.ip).map_err(CoreError::from)?;
        fsonos_core::topology::validate_response(&topology, coordinator)?;
        let mut current = house.clone();
        current.apply_topology(&topology);
        let after = current
            .groups
            .iter()
            .find(|group| group.coordinator == *coordinator)
            .ok_or_else(changed)?;
        let members = |group: &fsonos_types::ZoneGroup| {
            let mut members: Vec<_> = group
                .members
                .iter()
                .map(|member| member.0.clone())
                .collect();
            members.sort_unstable();
            members
        };
        // Caps are selected by logical room name, and bonded members share a
        // room's policy audience. Equal UUIDs/IPs alone do not prove that the
        // retained cap view is still valid after a rename or rebond.
        let rooms = |house: &HouseholdState| {
            let mut rooms: Vec<_> = house
                .rooms
                .iter()
                .filter(|room| room.coordinator == *coordinator)
                .cloned()
                .collect();
            rooms.sort_by(|left, right| left.primary.0.cmp(&right.primary.0));
            for room in &mut rooms {
                room.players.sort_by(|left, right| left.0.cmp(&right.0));
                room.missing.sort_by(|left, right| left.0.cmp(&right.0));
            }
            rooms
        };
        let before_rooms = rooms(house);
        if before_rooms.is_empty()
            || before_rooms != rooms(&current)
            || members(before) != members(after)
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

/// A refusal is latched so a later stage cannot resume a partially sent
/// source replacement. Reads remain available for receipts and diagnostics.
pub(super) struct AudienceTransport<'a> {
    surface: &'a Surface,
    households: &'a [HouseholdState],
    coordinator: &'a PlayerId,
    refused: RefCell<Option<Failure>>,
    writes: Cell<usize>,
}

impl<'a> AudienceTransport<'a> {
    pub(super) fn new(
        surface: &'a Surface,
        households: &'a [HouseholdState],
        coordinator: &'a PlayerId,
    ) -> Self {
        Self {
            surface,
            households,
            coordinator,
            refused: RefCell::new(None),
            writes: Cell::new(0),
        }
    }

    pub(super) fn check(&self) -> Result<(), Failure> {
        if let Some(failure) = self.refused.borrow().clone() {
            return Err(failure);
        }
        let result = self
            .surface
            .confirm_reviewed_group(self.households, self.coordinator);
        if let Err(failure) = &result {
            *self.refused.borrow_mut() = Some(failure.clone());
        }
        result
    }

    /// Restore admission errors hidden by a synthetic SOAP fault. Once a
    /// speaker step went out, a failure cannot erase its possible effects or
    /// leave an apparently safe whole-request retry or aggregate undo.
    pub(super) fn finish<T>(&self, result: Result<T, Failure>) -> Result<T, Failure> {
        self.refused.borrow().clone().map_or(result, Err).map_err(|failure| {
            if self.writes.get() == 0 {
                return failure;
            }
            Failure::new(
                ErrorCode::OutcomeUncertain,
                format!("{}; earlier speaker changes may have succeeded", failure.detail),
            )
            .with_hint("Read this group's source, queue and volumes before another action; do not replay the whole request.")
        })
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
        let writing = !verb.starts_with("Get") && !matches!(verb, "Browse" | "Search");
        if writing {
            self.check().map_err(|failure| ProtoError::SoapFault {
                // A definite local refusal, never the Sonos 800 fault that
                // could trigger address/coordinator recovery.
                code: 403,
                reason: format!("Reviewed audience refused {verb}: {}", failure.detail),
            })?;
        }
        let reply = self.surface.transport.soap_post(host, path, action, body);
        if writing {
            let refused = match &reply {
                Err(ProtoError::SoapFault { code: 800, .. }) => true,
                Ok(xml) => matches!(
                    soap::parse_response(xml, verb),
                    Err(ProtoError::SoapFault { code: 800, .. })
                ),
                _ => false,
            };
            if !refused {
                self.writes.set(self.writes.get().saturating_add(1));
            }
        }
        reply
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
