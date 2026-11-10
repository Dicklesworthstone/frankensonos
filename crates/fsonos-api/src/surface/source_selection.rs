//! Resolve catalog sources before caps, queue writes, fade cancellation or
//! action snapshots. Keep the original audience and source across the read.

use fsonos_core::control::TrackBatchDestination;
use fsonos_core::{HouseholdState, control};
use fsonos_proto::didl::SpotifyBare;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::PlayerId;

use super::Surface;
use crate::source::ArtistSelection;
use crate::{Command, ErrorCode, Failure};

impl Surface {
    pub(super) fn prepare_source(
        &self,
        households: &[HouseholdState],
        command: Command,
    ) -> Result<Command, Failure> {
        let Command::Play {
            coordinator,
            source_uri,
            title,
        } = command
        else {
            return Ok(command);
        };
        if !matches!(SpotifyBare::of(&source_uri), Some(SpotifyBare::Artist)) {
            return Ok(Command::Play {
                coordinator,
                source_uri,
                title,
            });
        }
        let (selection, destination) =
            self.resolve_artist(households, &coordinator, &source_uri)?;
        Ok(Command::PlayTracks {
            coordinator,
            source_uri,
            title,
            selection,
            destination,
        })
    }

    pub(super) fn resolve_artist(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        source_uri: &str,
    ) -> Result<(ArtistSelection, TrackBatchDestination), Failure> {
        let sources = self.spotify_sources.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "artist playback needs the owner's Spotify catalog session",
            )
            .with_hint("Sign in with fsonos setup, then play the artist link again.")
        })?;
        let host = control::locate(households, coordinator)?.ip;
        self.confirm_source_audience(households, coordinator)?;
        let destination = TrackBatchDestination::read(&*self.transport, host, coordinator)?;
        self.confirm_source_renderer(households, coordinator)?;
        let selection = sources.artist_tracks(source_uri)?;
        selection.validate()?;
        self.confirm_source_renderer(households, coordinator)?;
        destination.confirm(&*self.transport, host, coordinator)?;
        self.confirm_source_audience(households, coordinator)?;
        Ok((selection, destination))
    }

    pub(super) fn confirm_source_selection(
        &self,
        households: &[HouseholdState],
        command: &Command,
    ) -> Result<(), Failure> {
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
                "the artist destination changed; reread the rooms before playing or queueing",
            ));
        }
        Ok(())
    }
}
