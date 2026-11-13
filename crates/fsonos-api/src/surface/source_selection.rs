//! Resolve catalog sources before caps, queue writes, fade cancellation or
//! action snapshots. Keep the original audience and source across the read.

use fsonos_core::control::TrackBatchDestination;
use fsonos_core::favorites::{self, Favorite, FavoriteError, FavoriteKind};
use fsonos_core::{HouseholdState, control, playlists};
use fsonos_proto::Transport;
use fsonos_proto::didl::SpotifyBare;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::Surface;
use crate::plan::{Rooms, resolve_set};
use crate::source::{SourceSelection, is_catalog_source};
use crate::{Command, ErrorCode, Failure};

/// A query is household-local, even when it looks like the same native id.
#[derive(Clone, Copy)]
pub(super) enum NativeSource<'a> {
    Favorite(&'a str),
    Playlist(&'a str),
}

/// Keep one owned selection per household and one command per distinct group.
/// All catalogs are read before the caller admits any playback mutation.
pub(super) fn native_sources(
    transport: &dyn Transport,
    rooms: &Rooms<'_>,
    zone: &str,
    source: NativeSource<'_>,
) -> Result<Vec<Command>, Failure> {
    let mut selected: HashMap<usize, Favorite> = HashMap::new();
    let mut commands = Vec::new();
    for target in resolve_set(rooms, zone)? {
        if commands
            .iter()
            .any(|command: &Command| command.group_coordinator() == Some(&target.coordinator.id))
        {
            continue;
        }
        let house = rooms
            .households
            .iter()
            .position(|house| house.player(&target.coordinator.id).is_some())
            .ok_or_else(|| Failure::new(ErrorCode::NotReady, "the source household disappeared"))?;
        let favorite = match selected.entry(house) {
            Entry::Occupied(known) => known.into_mut(),
            Entry::Vacant(slot) => {
                let favorite = match source {
                    NativeSource::Favorite(query) => {
                        let catalog =
                            favorites::list(transport, rooms.households, &target.coordinator.id)?;
                        favorites::find(&catalog, query)?.clone()
                    }
                    NativeSource::Playlist(query) => {
                        let catalog =
                            playlists::list(transport, rooms.households, &target.coordinator.id)?;
                        super::sonos_playlists::find(&catalog, query)?.clone()
                    }
                };
                playable_favorite(&favorite)?;
                slot.insert(favorite)
            }
        };
        commands.push(Command::PlayFavorite {
            member: target.player.id.clone(),
            coordinator: target.coordinator.id.clone(),
            favorite: favorite.clone(),
        });
    }
    Ok(commands)
}

fn playable_favorite(favorite: &Favorite) -> Result<(), Failure> {
    if favorite.kind == FavoriteKind::Unplayable
        || favorite
            .uri
            .as_deref()
            .is_none_or(|uri| uri.trim().is_empty())
    {
        return Err(FavoriteError::Unplayable {
            title: favorite.title.clone(),
        }
        .into());
    }
    Ok(())
}

impl Surface {
    /// Prepare the complete set before its first write. In particular, retain
    /// every catalog destination before a shared artist/show lookup, so a
    /// change to an earlier group cannot be hidden by a later observation.
    pub(super) fn prepare_sources(
        &self,
        households: &[HouseholdState],
        commands: Vec<Command>,
    ) -> Result<Vec<Command>, Failure> {
        let mut destinations = Vec::with_capacity(commands.len());
        for command in &commands {
            let destination = match command {
                Command::Play {
                    coordinator,
                    source_uri,
                    ..
                } if is_catalog_source(source_uri) => {
                    Some(self.catalog_destination(households, coordinator)?)
                }
                Command::Play {
                    coordinator,
                    source_uri,
                    ..
                } if source_uri.starts_with("spotify:") => {
                    control::check_spotify_source(
                        &*self.transport,
                        households,
                        coordinator,
                        source_uri,
                    )
                    .map_err(crate::execute::spotify_failure)?;
                    None
                }
                Command::PlayFavorite { favorite, .. } => {
                    playable_favorite(favorite)?;
                    None
                }
                _ => None,
            };
            destinations.push(destination);
        }
        let mut selections = HashMap::new();
        let mut prepared = Vec::with_capacity(commands.len());
        for (command, destination) in commands.into_iter().zip(destinations) {
            let command = match (command, destination) {
                (
                    Command::Play {
                        coordinator,
                        source_uri,
                        title,
                        ..
                    },
                    Some(destination),
                ) => {
                    let selection = match selections.entry(source_uri.clone()) {
                        Entry::Occupied(known) => known.into_mut(),
                        Entry::Vacant(slot) => slot.insert(self.catalog_selection(&source_uri)?),
                    };
                    Command::PlayTracks {
                        coordinator,
                        source_uri,
                        title,
                        selection: selection.clone(),
                        destination,
                    }
                }
                (command, _) => command,
            };
            prepared.push(command);
        }
        // A later lookup can outlast a change to any earlier destination.
        // Check the complete set now; execute checks each selection again.
        for command in &prepared {
            self.confirm_source_selection(households, command)?;
            if prepared.len() > 1
                && let Command::Play { coordinator, .. } | Command::PlayFavorite { coordinator, .. } =
                    command
            {
                // An alias's independent groups must not have merged while
                // their sources were read, or two commands could change the
                // same group and use its obsolete policy audience.
                self.confirm_source_audience(households, coordinator)?;
            }
        }
        Ok(prepared)
    }

    pub(super) fn prepare_source(
        &self,
        households: &[HouseholdState],
        command: Command,
    ) -> Result<Command, Failure> {
        let Command::Play {
            member,
            coordinator,
            source_uri,
            title,
        } = command
        else {
            return Ok(command);
        };
        if !is_catalog_source(&source_uri) {
            return Ok(Command::Play {
                member,
                coordinator,
                source_uri,
                title,
            });
        }
        let (selection, destination) =
            self.resolve_catalog_source(households, &coordinator, &source_uri)?;
        Ok(Command::PlayTracks {
            coordinator,
            source_uri,
            title,
            selection,
            destination,
        })
    }

    /// Read catalog metadata only. Scene preflight shares the same validation.
    pub(super) fn catalog_selection(&self, source_uri: &str) -> Result<SourceSelection, Failure> {
        let sources = self.spotify_sources.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "catalog playback needs the owner's Spotify session",
            )
            .with_hint("Sign in with fsonos setup, then play the artist or show link again.")
        })?;
        let selection = match SpotifyBare::of(source_uri) {
            Some(SpotifyBare::Artist) => sources.artist_tracks(source_uri)?,
            Some(SpotifyBare::Show) => sources.show_episodes(source_uri)?,
            _ => {
                return Err(Failure::invalid(
                    "this source does not need catalog resolution",
                ));
            }
        };
        selection.validate_for_source(source_uri)?;
        Ok(selection)
    }

    pub(super) fn resolve_catalog_source(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        source_uri: &str,
    ) -> Result<(SourceSelection, TrackBatchDestination), Failure> {
        let destination = self.catalog_destination(households, coordinator)?;
        let selection = self.catalog_selection(source_uri)?;
        self.confirm_source_renderer(households, coordinator)?;
        destination.confirm(
            &*self.transport,
            control::locate(households, coordinator)?.ip,
            coordinator,
        )?;
        self.confirm_source_audience(households, coordinator)?;
        Ok((selection, destination))
    }

    fn catalog_destination(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
    ) -> Result<TrackBatchDestination, Failure> {
        if self.spotify_sources.is_none() {
            return Err(Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "catalog playback needs the owner's Spotify session",
            )
            .with_hint("Sign in with fsonos setup, then play the artist or show link again."));
        }
        let host = control::locate(households, coordinator)?.ip;
        self.confirm_source_audience(households, coordinator)?;
        let destination = TrackBatchDestination::read(&*self.transport, host, coordinator)?;
        self.confirm_source_renderer(households, coordinator)?;
        Ok(destination)
    }

    pub(super) fn confirm_source_selection(
        &self,
        households: &[HouseholdState],
        command: &Command,
    ) -> Result<(), Failure> {
        if let Command::PlayLineIn {
            coordinator,
            prepared,
        } = command
        {
            prepared.confirm(&*self.transport, households, coordinator)?;
            self.confirm_source_audience(households, coordinator)?;
        }
        if let Command::PlayTracks {
            coordinator,
            destination,
            ..
        } = command
        {
            self.confirm_source_renderer(households, coordinator)?;
            destination.confirm(
                &*self.transport,
                control::locate(households, coordinator)?.ip,
                coordinator,
            )?;
            self.confirm_source_audience(households, coordinator)?;
        }
        Ok(())
    }

    fn confirm_source_renderer(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
    ) -> Result<(), Failure> {
        if control::spotify_params(&*self.transport, households, coordinator)?.is_none() {
            return Err(crate::execute::spotify_failure(
                fsonos_core::CoreError::NoSpotifyFavorite,
            ));
        }
        Ok(())
    }

    /// The rooms used for policy caps must describe the actual audience,
    /// including bonded players and room names, not an old cached grouping.
    pub(super) fn confirm_source_audience(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
    ) -> Result<(), Failure> {
        let household = households
            .iter()
            .find(|house| house.player(coordinator).is_some())
            .ok_or_else(|| Failure::new(ErrorCode::NotReady, "the source household disappeared"))?;
        let host = control::locate(households, coordinator)?.ip;
        let mut fresh = household.clone();
        fresh.apply_topology(&get_zone_group_state(&*self.transport, host)?);
        let audience = |house: &HouseholdState| {
            let mut rooms: Vec<_> = house
                .rooms
                .iter()
                .filter(|room| room.coordinator == *coordinator)
                .cloned()
                .collect();
            rooms.sort_by(|a, b| a.primary.0.cmp(&b.primary.0));
            for room in &mut rooms {
                room.players.sort_by(|a, b| a.0.cmp(&b.0));
                room.missing.sort_by(|a, b| a.0.cmp(&b.0));
            }
            rooms
        };
        let before = audience(household);
        if before.is_empty() || before != audience(&fresh) {
            return Err(Failure::new(
                ErrorCode::QueueChanged,
                "the catalog source destination changed; reread the rooms before playing or queueing",
            ));
        }
        Ok(())
    }
}
