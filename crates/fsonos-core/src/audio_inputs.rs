//! Find and play physical line-in sources within one Sonos household.
//!
//! The source is an exact player, independent of its room's coordinator. The
//! destination is a group: AVTransport selects that input's x-rincon-stream URI
//! there without regrouping rooms or changing either player's queue. Capability
//! is learned from a fresh device description, not a model-name guess.
//! Connection and signal state are unknown until AudioIn events are observed.
//!
//! Preparation retains the destination's source, membership and boot epoch.
//! Dispatch rechecks those observations, the input identity and its household
//! before each write. These checks do not make independent controllers atomic;
//! uncertain writes are never replayed or rolled back.

use crate::control::SourceDestination;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::audio_in;
use fsonos_proto::description::{DeviceDescription, parse_device_description};
use fsonos_proto::soap::{self, AUDIO_IN, AV_TRANSPORT};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport, ssdp};
use fsonos_types::{HouseholdId, Player, PlayerId};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

/// One physical input. Bonded players retain separate player identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub player: PlayerId,
    pub room: String,
    pub name: String,
    pub icon: String,
}

/// A player whose input capability could not be determined by this read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub player: PlayerId,
    pub room: String,
    pub detail: String,
}

/// An explicit, finite read of the destination household's discovered players.
/// Unsupported players are omitted; unreadable ones remain visible separately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub sources: Vec<Source>,
    pub unavailable: Vec<Unavailable>,
}

/// Inspect a known player without writing to it. None means its successfully
/// read description advertises no usable AudioIn service; failure is not absence.
pub fn probe<T: Transport + ?Sized>(
    transport: &T,
    player: &Player,
) -> Result<Option<Source>, CoreError> {
    if !capable(transport, player)? {
        return Ok(None);
    }
    let attributes = audio_in::attributes(transport, player.ip)?;
    Ok(Some(Source {
        player: player.id.clone(),
        room: player.room_name.clone(),
        name: attributes.name,
        icon: attributes.icon,
    }))
}

fn capable<T: Transport + ?Sized>(transport: &T, player: &Player) -> Result<bool, CoreError> {
    audio_in::source_uri(&player.id)?;
    let description = identified(transport, player)?;
    Ok(description.is_renderer() && description.has_service(AUDIO_IN.service_type))
}

fn identified<T: Transport + ?Sized>(
    transport: &T,
    player: &Player,
) -> Result<DeviceDescription, CoreError> {
    let description =
        parse_device_description(&transport.http_get(&ssdp::description_url(player.ip))?)?;
    if description.udn != player.id {
        return Err(changed(
            "a retained player address now belongs to another player",
        ));
    }
    Ok(description)
}

/// List only inputs in the destination's own household, including both players
/// of a stereo pair. No discovery, subscription, playback or volume write occurs.
pub fn list<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Catalog, CoreError> {
    let household = household(households, coordinator)?;
    let mut catalog = Catalog::default();
    for player in &household.players {
        match probe(transport, player) {
            Ok(Some(source)) => catalog.sources.push(source),
            Ok(None) => {}
            Err(error) => catalog.unavailable.push(Unavailable {
                player: player.id.clone(),
                room: player.room_name.clone(),
                detail: error.to_string(),
            }),
        }
    }
    catalog
        .sources
        .sort_by(|a, b| (&a.room, &a.player.0).cmp(&(&b.room, &b.player.0)));
    catalog
        .unavailable
        .sort_by(|a, b| (&a.room, &a.player.0).cmp(&(&b.room, &b.player.0)));
    Ok(catalog)
}

/// An input and destination validated before action snapshots and policy reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLineIn {
    pub source: Source,
    input: PlayerId,
    coordinator: PlayerId,
    household: Option<HouseholdId>,
    destination: SourceDestination,
    source_boot_seq: Option<u32>,
}

/// Read-only admission for one exact input and one destination coordinator.
pub fn prepare<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    source: &PlayerId,
) -> Result<PreparedLineIn, CoreError> {
    let (household, destination_player, input) = endpoints(households, coordinator, source)?;
    let destination = SourceDestination::read(transport, destination_player.ip, coordinator)?;
    let source_boot_seq = input_epoch(transport, destination_player.ip, source)?;
    let source = probe(transport, input)?.ok_or_else(|| {
        CoreError::LineInUnavailable(format!(
            "{} does not advertise a physical line-in input",
            input.room_name
        ))
    })?;
    let prepared = PreparedLineIn {
        input: source.player.clone(),
        source,
        coordinator: coordinator.clone(),
        household: household.id.clone(),
        destination,
        source_boot_seq,
    };
    prepared.confirm(transport, households, coordinator)?;
    Ok(prepared)
}

impl PreparedLineIn {
    /// Recheck the original source, group, both player identities and input
    /// capability. A new coordinator or a same-named replacement is not adopted.
    pub fn confirm<T: Transport + ?Sized>(
        &self,
        transport: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
    ) -> Result<(), CoreError> {
        self.confirm_at(transport, households, coordinator, false)
    }

    fn confirm_at<T: Transport + ?Sized>(
        &self,
        transport: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        selected: bool,
    ) -> Result<(), CoreError> {
        if coordinator != &self.coordinator || self.source.player != self.input {
            return Err(changed(
                "the selected input or destination identity changed",
            ));
        }
        let (household, destination, input) = endpoints(households, coordinator, &self.input)?;
        if household.id != self.household {
            return Err(changed("the input's household identity changed"));
        }
        if !identified(transport, destination)?.is_renderer() {
            return Err(changed("the destination no longer advertises a renderer"));
        }
        if !capable(transport, input)? {
            return Err(CoreError::LineInUnavailable(
                "the selected player no longer advertises AudioIn; read the line-in sources again"
                    .into(),
            ));
        }
        let current =
            SourceDestination::read(transport, destination.ip, coordinator).map_err(|error| {
                changed(&format!(
                    "the destination could not be revalidated: {error}"
                ))
            })?;
        let unchanged = if selected {
            self.destination.same_group(&current)
                && current.has_source(&audio_in::source_uri(&self.input)?, "")
        } else {
            current == self.destination
        };
        if !unchanged {
            return Err(changed(
                "the destination group, boot epoch or selected source changed",
            ));
        }
        if input_epoch(transport, destination.ip, &self.input)? != self.source_boot_seq {
            return Err(changed("the input player restarted"));
        }
        Ok(())
    }
}

fn changed(detail: &str) -> CoreError {
    CoreError::LineInChanged(format!(
        "{detail}; read the line-in sources and destination again"
    ))
}

fn household<'a>(
    households: &'a [HouseholdState],
    coordinator: &PlayerId,
) -> Result<&'a HouseholdState, CoreError> {
    let mut matching = households
        .iter()
        .filter(|house| house.player(coordinator).is_some());
    let house = matching
        .next()
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    if matching.next().is_some() {
        return Err(changed("the destination household is ambiguous"));
    }
    Ok(house)
}

fn endpoints<'a>(
    households: &'a [HouseholdState],
    coordinator: &PlayerId,
    source: &PlayerId,
) -> Result<(&'a HouseholdState, &'a Player, &'a Player), CoreError> {
    let house = household(households, coordinator)?;
    let input = house.player(source).ok_or_else(|| {
        CoreError::LineInUnavailable(
            "the line-in source must be a known player in the destination household".into(),
        )
    })?;
    if households
        .iter()
        .filter(|other| other.player(source).is_some())
        .count()
        != 1
    {
        return Err(changed("the input household is ambiguous"));
    }
    let destination = house
        .player(coordinator)
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    Ok((house, destination, input))
}

/// Require a currently present member, not merely a stale channel-map reference.
fn input_epoch<T: Transport + ?Sized>(
    transport: &T,
    destination: IpAddr,
    source: &PlayerId,
) -> Result<Option<u32>, CoreError> {
    let topology = get_zone_group_state(transport, destination)?;
    let mut members = topology
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .flat_map(|member| std::iter::once(member).chain(&member.satellites))
        .filter(|member| member.uuid == *source);
    let input = members
        .next()
        .ok_or_else(|| changed("the input is no longer present in the destination household"))?;
    if members.next().is_some() {
        return Err(changed("the input has an ambiguous topology identity"));
    }
    Ok(input.boot_seq)
}

/// Start the prepared input, retaining its exact destination through both
/// source selection and Play. The queue is never read, cleared or rewritten.
pub fn play_prepared<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    prepared: &PreparedLineIn,
) -> Result<(), CoreError> {
    let checked = PlaybackTransport {
        transport,
        households,
        coordinator,
        prepared,
        may_have_changed: Cell::new(false),
        rejected: RefCell::new(None),
    };
    let uri = audio_in::source_uri(&prepared.input)?;
    let result = control::play_uri(&checked, households, coordinator, &uri, "");
    let result = if let Some(error) = checked.rejected.take() {
        Err(error)
    } else {
        result
    };
    match result {
        Err(error) if checked.may_have_changed.get() => Err(uncertain(error)),
        Err(error) => Err(error),
        Ok(()) => prepared
            .confirm_at(transport, households, coordinator, true)
            .map_err(uncertain),
    }
}

fn uncertain(error: CoreError) -> CoreError {
    CoreError::Uncertain(format!(
        "line-in playback was not confirmed: {error}; a source or playback write may have taken effect; \
         nothing was replayed; inspect the destination before trying again"
    ))
}

/// Keep the shared direct-source acknowledgement recovery, with an ownership
/// check at the actual write boundary after any caller-side cap reads/writes.
struct PlaybackTransport<'a, T: ?Sized> {
    transport: &'a T,
    households: &'a [HouseholdState],
    coordinator: &'a PlayerId,
    prepared: &'a PreparedLineIn,
    may_have_changed: Cell<bool>,
    rejected: RefCell<Option<CoreError>>,
}

impl<T: Transport + ?Sized> Transport for PlaybackTransport<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let mutation = if path == AV_TRANSPORT.control_path {
            if action == soap::soap_action_header(&AV_TRANSPORT, "SetAVTransportURI") {
                Some(("SetAVTransportURI", false))
            } else if action == soap::soap_action_header(&AV_TRANSPORT, "Play") {
                Some(("Play", true))
            } else {
                None
            }
        } else {
            None
        };
        let Some((verb, selected)) = mutation else {
            return self.transport.soap_post(host, path, action, body);
        };
        let not_rejected = self.rejected.borrow().is_none();
        if not_rejected
            && let Err(error) = self.prepared.confirm_at(
                self.transport,
                self.households,
                self.coordinator,
                selected,
            )
        {
            *self.rejected.borrow_mut() = Some(error);
        }
        if let Some(error) = self.rejected.borrow().as_ref() {
            return Err(ProtoError::Malformed(error.to_string()));
        }
        let result = self.transport.soap_post(host, path, action, body);
        let refused = matches!(&result, Err(ProtoError::SoapFault { .. }))
            || result.as_ref().is_ok_and(|body| {
                matches!(
                    soap::parse_response(body, verb),
                    Err(ProtoError::SoapFault { .. })
                )
            });
        if !refused {
            self.may_have_changed.set(true);
        }
        result
    }
}

#[cfg(test)]
mod tests;
