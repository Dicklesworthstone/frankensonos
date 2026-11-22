//! Prepare every resolved catalog or authored track selection before a scene
//! mutates the house. Retain destinations through lookup, policy and caps;
//! execution consumes each selection using the confirmed, regrouped audience.

use fsonos_core::control::{TrackBatchDestination, queue_removal};
use fsonos_core::scenes::{SceneOp, ScenePlan, SceneTrack};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::PlayerId;
use std::collections::BTreeMap;

use super::Surface;
use crate::source::{SourceSelection, is_catalog_source};
use crate::{ErrorCode, Failure};

#[derive(Default)]
pub(super) struct Prepared {
    selections: BTreeMap<String, SourceSelection>,
    destinations: Vec<Destination>,
}

struct Destination {
    coordinator: PlayerId,
    source: Selection,
    current_coordinator: PlayerId,
    current: TrackBatchDestination,
    members: Vec<PlayerId>,
    local: LocalState,
}

#[derive(PartialEq, Eq)]
enum Selection {
    Catalog(String),
    Tracks(Vec<SceneTrack>),
}

impl Selection {
    fn of(op: &SceneOp) -> Option<(&PlayerId, Self)> {
        match op {
            SceneOp::Uri {
                coordinator, uri, ..
            } if is_catalog_source(uri) => Some((coordinator, Self::Catalog(uri.clone()))),
            SceneOp::Tracks {
                coordinator,
                tracks,
                ..
            } => Some((coordinator, Self::Tracks(tracks.clone()))),
            _ => None,
        }
    }
}

/// Grouping does not authorize an independent edit to the destination's queue
/// or a player reboot. A target that already coordinates must also retain its
/// selected source. A future coordinator's self-join legitimately changes its
/// old member source, so only its local queue and boot are retained.
#[derive(PartialEq, Eq)]
struct LocalState {
    boot_seq: Option<u32>,
    queue: queue_removal::Snapshot,
    source: Option<(String, String)>,
}

impl LocalState {
    fn read(
        surface: &Surface,
        households: &[HouseholdState],
        player: &PlayerId,
        current_coordinator: &PlayerId,
        keep_source: bool,
    ) -> Result<Self, Failure> {
        let host = control::locate(households, player)?.ip;
        let topology = get_zone_group_state(&*surface.transport, host)?;
        fsonos_core::topology::validate_response(&topology, player)?;
        let mut matching = topology.groups.iter().flat_map(|group| {
            group.members.iter().filter(move |member| {
                group.coordinator == *current_coordinator
                    && member.uuid == *player
                    && member.ip() == Some(host)
                    && !member.is_zone_bridge
            })
        });
        let Some(member) = matching.next() else {
            return Err(changed());
        };
        if matching.next().is_some() {
            return Err(changed());
        }
        let source = if keep_source {
            let media = soap::call(
                &*surface.transport,
                host,
                &AV_TRANSPORT,
                "GetMediaInfo",
                &soap::args_xml(&[("InstanceID", "0")]),
            )?;
            let uri = media.require("CurrentURI")?;
            if uri == "NOT_IMPLEMENTED" {
                return Err(changed());
            }
            Some((
                uri.to_string(),
                media.get("CurrentURIMetaData").unwrap_or("").to_string(),
            ))
        } else {
            None
        };
        Ok(Self {
            boot_seq: member.boot_seq,
            queue: queue_removal::read(&*surface.transport, host)?,
            source,
        })
    }

    fn confirm(
        &self,
        surface: &Surface,
        households: &[HouseholdState],
        player: &PlayerId,
        current_coordinator: &PlayerId,
    ) -> Result<(), Failure> {
        if *self
            != Self::read(
                surface,
                households,
                player,
                current_coordinator,
                self.source.is_some(),
            )?
        {
            return Err(changed());
        }
        Ok(())
    }
}

fn members(households: &[HouseholdState], coordinator: &PlayerId) -> Vec<PlayerId> {
    let mut players: Vec<_> = households
        .iter()
        .flat_map(|house| &house.rooms)
        .filter(|room| room.coordinator == *coordinator)
        .flat_map(|room| room.players.iter().cloned())
        .collect();
    players.sort_by(|left, right| left.0.cmp(&right.0));
    players.dedup();
    players
}

fn changed() -> Failure {
    Failure::new(
        ErrorCode::QueueChanged,
        "the scene's selection destination changed while preparing its music; apply the scene again",
    )
}

fn renderer_ready(
    surface: &Surface,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), Failure> {
    control::spotify_params(&*surface.transport, households, coordinator)?
        .map(|_| ())
        .ok_or_else(|| crate::execute::spotify_failure(fsonos_core::CoreError::NoSpotifyFavorite))
}

impl Prepared {
    pub(super) fn read(
        surface: &Surface,
        households: &[HouseholdState],
        plan: &ScenePlan,
    ) -> Result<Self, Failure> {
        let mut prepared = Self::default();
        // Capture all destinations before the first potentially slow external
        // read. A later group's missing source must not partly apply a scene.
        for op in &plan.ops {
            let Some((coordinator, source)) = Selection::of(op) else {
                continue;
            };
            let room = households
                .iter()
                .flat_map(|house| &house.rooms)
                .find(|room| room.players.contains(coordinator))
                .ok_or_else(changed)?;
            let current_coordinator = room.coordinator.clone();
            surface.confirm_source_audience(households, &current_coordinator)?;
            let current = TrackBatchDestination::read(
                &*surface.transport,
                control::locate(households, &current_coordinator)?.ip,
                &current_coordinator,
            )?;
            let local = LocalState::read(
                surface,
                households,
                coordinator,
                &current_coordinator,
                coordinator == &current_coordinator,
            )?;
            let members = members(households, &current_coordinator);
            renderer_ready(surface, households, coordinator)?;
            prepared.destinations.push(Destination {
                coordinator: coordinator.clone(),
                source,
                current_coordinator,
                current,
                members,
                local,
            });
        }
        for destination in &prepared.destinations {
            let Selection::Catalog(uri) = &destination.source else {
                continue;
            };
            if !prepared.selections.contains_key(uri) {
                let selection = surface.catalog_selection(uri)?;
                selection.validate_for_source(uri)?;
                prepared.selections.insert(uri.clone(), selection);
            }
        }
        prepared.confirm(surface, households, plan)?;
        Ok(prepared)
    }

    pub(super) fn confirm(
        &self,
        surface: &Surface,
        households: &[HouseholdState],
        plan: &ScenePlan,
    ) -> Result<(), Failure> {
        for destination in &self.destinations {
            if !plan.ops.iter().any(|op| {
                Selection::of(op).is_some_and(|(coordinator, source)| {
                    coordinator == &destination.coordinator && source == destination.source
                })
            }) {
                continue;
            }
            // The complete selection was validated without rendering it as a
            // bare artist/show. Tracks and episodes use this household's own
            // renderer parameters, including after a long catalog lookup.
            renderer_ready(surface, households, &destination.coordinator)?;
            destination.current.confirm(
                &*surface.transport,
                control::locate(households, &destination.current_coordinator)?.ip,
                &destination.current_coordinator,
            )?;
            surface.confirm_source_audience(households, &destination.current_coordinator)?;
            destination.local.confirm(
                surface,
                households,
                &destination.coordinator,
                &destination.current_coordinator,
            )?;
        }
        Ok(())
    }

    pub(super) fn load(
        &self,
        surface: &Surface,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        uri: &str,
        start: bool,
        retain_receipt: bool,
    ) -> Result<Option<control::QueueReplacement>, String> {
        self.load_selection(
            surface,
            households,
            coordinator,
            &Selection::Catalog(uri.into()),
            start,
            retain_receipt,
        )
    }

    pub(super) fn load_tracks(
        &self,
        surface: &Surface,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        tracks: &[SceneTrack],
        start: bool,
        retain_receipt: bool,
    ) -> Result<Option<control::QueueReplacement>, String> {
        self.load_selection(
            surface,
            households,
            coordinator,
            &Selection::Tracks(tracks.to_vec()),
            start,
            retain_receipt,
        )
    }

    fn load_selection(
        &self,
        surface: &Surface,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        source: &Selection,
        start: bool,
        retain_receipt: bool,
    ) -> Result<Option<control::QueueReplacement>, String> {
        let prepared = self
            .destinations
            .iter()
            .find(|destination| {
                destination.coordinator == *coordinator && destination.source == *source
            })
            .ok_or("the scene has no prepared selection for this destination")?;
        surface
            .confirm_source_audience(households, coordinator)
            .map_err(|error| error.detail)?;
        let host = control::locate(households, coordinator)
            .map_err(|error| error.to_string())?
            .ip;
        // A cap or another scene step must not turn a newer queue or source
        // into our replacement baseline. Preserve the full original proof
        // whenever this group did not change, even if another group did.
        let regrouped;
        let destination = if prepared.current_coordinator == *coordinator
            && prepared.members == members(households, coordinator)
        {
            &prepared.current
        } else {
            // Only the intended audience/source transition is relaxed after
            // grouping. The target's queue and boot must still be the ones
            // observed before lookup; existing coordinators keep their source.
            prepared
                .local
                .confirm(surface, households, coordinator, coordinator)
                .map_err(|error| error.detail)?;
            regrouped = TrackBatchDestination::read(&*surface.transport, host, coordinator)
                .map_err(|error| error.to_string())?;
            prepared
                .local
                .confirm(surface, households, coordinator, coordinator)
                .map_err(|error| error.detail)?;
            &regrouped
        };
        // The permitted regrouping can require a fresh destination proof.
        // Bracket that capture with the complete capped audience: a room
        // joining during its reads must not become a new playback baseline.
        surface
            .confirm_source_audience(households, coordinator)
            .map_err(|error| error.detail)?;
        let tracks: Vec<_> = match &prepared.source {
            Selection::Catalog(uri) => self
                .selections
                .get(uri)
                .ok_or("the scene's Spotify catalog selection is unavailable")?
                .tracks
                .iter()
                .map(|(uri, title)| (uri.as_str(), title.as_str()))
                .collect(),
            Selection::Tracks(tracks) => tracks
                .iter()
                .map(|track| {
                    (
                        track.source_uri.as_str(),
                        track.title.as_deref().unwrap_or(&track.source_uri),
                    )
                })
                .collect(),
        };
        if retain_receipt {
            let replaced = if start {
                control::play_spotify_tracks_bound_with_receipt(
                    &*surface.transport,
                    households,
                    coordinator,
                    &tracks,
                    destination,
                )
            } else {
                control::load_spotify_tracks_bound_with_receipt(
                    &*surface.transport,
                    households,
                    coordinator,
                    &tracks,
                    destination,
                )
            };
            return replaced.map(Some).map_err(|error| error.to_string());
        }
        let loaded = if start {
            control::play_spotify_tracks_bound(
                &*surface.transport,
                households,
                coordinator,
                &tracks,
                destination,
            )
        } else {
            control::load_spotify_tracks_bound(
                &*surface.transport,
                households,
                coordinator,
                &tracks,
                destination,
            )
        };
        loaded.map(|()| None).map_err(|error| error.to_string())
    }
}
