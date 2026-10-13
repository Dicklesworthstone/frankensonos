//! Read-back proofs for a prepared relocation. A successful SOAP response is
//! not enough to stop the source: the destination must actually hold and play
//! the requested source. Retries here repeat reads, never mutations.

use super::{Item, Prepared, Source, invalid};
use crate::moving::{JOIN_DEADLINE, MoveError};
use fsonos_proto::Transport;
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::time::{Duration, Instant};

fn identity(uri: &str) -> String {
    // Queue inserts use a bare encoded Spotify URI; reads include a renderer
    // scheme and account parameters. Compare the track, not that envelope.
    let rendered = if uri.to_ascii_lowercase().starts_with("spotify%3a") {
        format!("x-sonos-spotify:{uri}")
    } else {
        uri.to_string()
    };
    spotify_uri_from_renderer_uri(&rendered).unwrap_or(rendered)
}

pub(super) fn empty_queue<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<(), MoveError> {
    let page = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if page.total_matches != 0 || !page.objects.is_empty() || page.number_returned != 0 {
        return Err(invalid("the destination did not clear its queue"));
    }
    Ok(())
}

pub(super) fn queue<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    items: &[Item],
) -> Result<u32, MoveError> {
    let queue = content::browse_snapshot(t, host, QUEUE)?;
    if queue.objects.len() != items.len()
        || queue.objects.iter().zip(items).any(|(object, item)| {
            object
                .res
                .as_ref()
                .is_none_or(|res| identity(&res.uri) != identity(&item.uri))
        })
    {
        return Err(invalid(
            "the destination queue does not match the prepared tracks",
        ));
    }
    Ok(queue.update_id)
}

/// Confirm source identity and queue revision around a position read. This
/// is used both to finish preparation and before releasing the source player.
pub(super) fn source<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    source: &Source,
    revision: Option<u32>,
) -> Result<soap::PositionInfo, MoveError> {
    let media = soap::get_media_info(t, host)?;
    let position = soap::get_position_info(t, host)?;
    let matches = match source {
        Source::Nothing => media.uri.is_empty(),
        Source::Uri { item, .. } => identity(&media.uri) == identity(&item.uri),
        Source::Queue { items, track, .. } => {
            let item = track
                .checked_sub(1)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| items.get(index));
            media.uri == format!("x-rincon-queue:{}#0", coordinator.0)
                && usize::try_from(media.tracks).ok() == Some(items.len())
                && position.track == *track
                && item.is_some_and(|item| identity(&position.uri) == identity(&item.uri))
        }
    };
    if !matches {
        return Err(invalid("playback no longer matches the prepared source"));
    }
    if let Some(revision) = revision {
        let check = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
        if check.update_id != revision || check.total_matches != media.tracks {
            return Err(invalid("the queue changed during relocation verification"));
        }
    }
    Ok(position)
}

pub(super) fn transport<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    expected: TransportState,
) -> Result<(), MoveError> {
    let started = Instant::now();
    loop {
        if soap::get_transport_info(t, host)?.state == expected {
            return Ok(());
        }
        let remaining = JOIN_DEADLINE.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(invalid(
                "the destination did not reach the required transport state",
            ));
        }
        std::thread::sleep(Duration::from_millis(100).min(remaining));
    }
}

pub(super) fn playback<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    target: &PlayerId,
    prepared: &Prepared,
    revision: Option<u32>,
    elapsed: Duration,
) -> Result<(), MoveError> {
    let position = source(t, host, target, &prepared.source, revision)?;
    let wanted = match &prepared.source {
        Source::Nothing => None,
        Source::Uri { position, .. } => *position,
        Source::Queue { position, .. } => Some(*position),
    };
    if let Some(wanted) = wanted {
        let forward = if prepared.transport == TransportState::Playing {
            u32::try_from(elapsed.as_secs())
                .unwrap_or(u32::MAX)
                .saturating_add(2)
        } else {
            2
        };
        if position.position_secs.is_none_or(|actual| {
            actual < wanted.saturating_sub(2) || actual > wanted.saturating_add(forward)
        }) {
            return Err(invalid(
                "the destination did not reach the requested track position",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_identity_ignores_spotify_account_envelopes_not_track_ids() {
        let bare = "spotify%3atrack%3a0000000000000000000001";
        assert_eq!(
            identity(bare),
            identity(&format!("x-sonos-spotify:{bare}?sid=12&sn=2"))
        );
        assert_ne!(
            identity(bare),
            identity("x-sonos-spotify:spotify%3atrack%3a0000000000000000000002")
        );
        assert_ne!(
            identity("x-file-cifs://example.invalid/a.flac"),
            identity("x-file-cifs://example.invalid/b.flac")
        );
    }
}
