//! Prepare concrete Spotify selections for a native saved playlist. Renderer
//! metadata comes from the destination household; the playing queue is never
//! used as scratch space.

use super::{GroupProof, MAX_PLAYLIST_EDIT_BYTES, invalid, selection};
use crate::favorites::Favorite;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_proto::didl::{
    DidlRes, item_metadata, parse_didl, spotify_bare_didl, spotify_bare_uri, spotify_track_didl,
    spotify_track_uri,
};
use fsonos_types::PlayerId;

/// Render 1–10 canonical Spotify tracks or episodes for a saved playlist.
/// Input order, repeated identities and titles are retained. Music tracks use
/// this household's learned account descriptor; episodes use its sid-only form.
///
/// This only reads topology and household favorites. It does not read or alter
/// the live queue, playback, volume, saved playlists or Spotify account. The
/// caller must retain the destination's contents revision across preparation
/// and pass it to append_tracks; returned metadata alone is not permission
/// to edit a newer playlist.
pub fn prepare_spotify_tracks<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
) -> Result<Vec<Favorite>, CoreError> {
    validate(tracks)?;
    let host = control::locate(households, coordinator)?.ip;
    let group = GroupProof::read(transport, host, coordinator)?;
    let params = control::spotify_params(transport, households, coordinator)?
        .ok_or(CoreError::NoSpotifyFavorite)?;
    let mut selected = Vec::with_capacity(tracks.len());
    let mut bytes = 0_usize;
    for (source, title) in tracks {
        let (uri, metadata) = if source.starts_with("spotify:episode:") {
            (
                spotify_bare_uri(source, params.sid),
                spotify_bare_didl(source, title, params.sid),
            )
        } else {
            (
                spotify_track_uri(source, &params),
                spotify_track_didl(source, title, &params),
            )
        };
        let mut objects = parse_didl(&metadata)?;
        let [item] = objects.as_mut_slice() else {
            return Err(invalid("Spotify rendering did not produce one track"));
        };
        // Track playback DIDL omits its resource because SetAVTransportURI
        // supplies it separately. Saved tracks need the matching absolute
        // resource in their metadata as well as in AddURIToSavedQueue.
        item.res = Some(DidlRes {
            uri,
            protocol_info: None,
            duration: None,
        });
        let metadata = item_metadata(item);
        bytes = bytes.saturating_add(metadata.len());
        if bytes > MAX_PLAYLIST_EDIT_BYTES {
            return Err(invalid("selected Spotify track metadata exceeds 4 MiB"));
        }
        selected.push(selection::track(item, &metadata)?);
    }
    group.confirm(transport, host, coordinator)?;
    Ok(selected)
}

fn validate(tracks: &[(&str, &str)]) -> Result<(), CoreError> {
    if tracks.is_empty() || tracks.len() > control::MAX_TRACK_BATCH {
        return Err(invalid(
            "a saved playlist selection needs between 1 and 10 Spotify tracks or episodes",
        ));
    }
    let mut bytes = 0_usize;
    for (uri, title) in tracks {
        if !uri
            .strip_prefix("spotify:track:")
            .or_else(|| uri.strip_prefix("spotify:episode:"))
            .is_some_and(|id| id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        {
            return Err(invalid(
                "saved playlist selections require canonical Spotify tracks or episodes with 22-character ids",
            ));
        }
        if title
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
        {
            return Err(invalid(
                "a selected track title contains invalid XML characters",
            ));
        }
        bytes = bytes.saturating_add(title.len());
        if bytes > MAX_PLAYLIST_EDIT_BYTES {
            return Err(invalid("selected Spotify track titles exceed 4 MiB"));
        }
    }
    Ok(())
}
