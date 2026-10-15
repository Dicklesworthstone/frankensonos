//! Queue a household's native favorite without replacing or starting playback.

use crate::control::{self, Queued};
use crate::favorites::{Favorite, FavoriteKind};
use crate::{CoreError, HouseholdState};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

#[derive(Debug, PartialEq, Eq)]
struct Queue {
    revision: u32,
    uris: Vec<String>,
}

fn read<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<Queue, CoreError> {
    let snapshot = browse_snapshot(t, host, QUEUE)?;
    let uris = snapshot
        .objects
        .into_iter()
        .map(|item| {
            item.res
                .map(|res| res.uri)
                .filter(|uri| !uri.trim().is_empty())
                .ok_or_else(|| ProtoError::Malformed("queued track has no identity".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Queue {
        revision: snapshot.update_id,
        uris,
    })
}

fn same_track(a: &str, b: &str) -> bool {
    a == b
        || spotify_uri_from_renderer_uri(a)
            .zip(spotify_uri_from_renderer_uri(b))
            .is_some_and(|(a, b)| a == b)
}

/// An insertion is confirmed only with both original neighbors intact. The
/// returned identities are the speaker's, not reconstructed favorite metadata.
fn inserted<'a>(
    before: &Queue,
    after: &'a Queue,
    index: usize,
    count: usize,
) -> Option<&'a [String]> {
    let end = index.checked_add(count)?;
    if count == 0
        || before.revision == after.revision
        || index > before.uris.len()
        || before.uris.len().checked_add(count)? != after.uris.len()
        || before.uris[..index] != after.uris[..index]
        || before.uris[index..] != after.uris[end..]
    {
        return None;
    }
    Some(&after.uris[index..end])
}

fn uncertain(detail: &str) -> CoreError {
    ProtoError::Malformed(format!(
        "favorite enqueue outcome is uncertain: {detail}; read the queue before another add; \
         the request was not repeated"
    ))
    .into()
}

/// Append a native track, album or playlist favorite, or insert it after the
/// current queue track when `next`. Its URI and metadata are used unchanged,
/// with no Spotify Web API grant or separately learned render template.
///
/// The caller must obtain `favorite` from this coordinator's own household.
/// Streams and browse shortcuts are refused before any I/O. This never clears
/// the queue, changes its mode or source, seeks, or starts playback. A coherent
/// read verifies the insertion and supplies exact identities for undo.
///
/// The non-idempotent add is sent once. A lost track reply can be recovered by
/// exact read-back; a lost container reply cannot establish which tracks its
/// service expanded, so it remains uncertain. Reads are not a device-side lock.
pub fn enqueue<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    favorite: &Favorite,
    next: bool,
) -> Result<Queued, CoreError> {
    if !matches!(favorite.kind, FavoriteKind::Track | FavoriteKind::Container) {
        return Err(ProtoError::Malformed(
            "only track, album and playlist favorites can be queued; play a station directly".into(),
        )
        .into());
    }
    let uri = favorite
        .uri
        .as_deref()
        .filter(|uri| !uri.trim().is_empty())
        .ok_or_else(|| ProtoError::Malformed("favorite has no playable URI".into()))?;
    let host = control::locate(households, coordinator)?.ip;
    let playing_queue =
        soap::get_media_info(t, host)?.uri == format!("x-rincon-queue:{}#0", coordinator.0);
    let track = if next && playing_queue {
        Some(soap::get_position_info(t, host)?.track)
    } else {
        None
    };
    let before = read(t, host)?;
    let length = u32::try_from(before.uris.len())
        .map_err(|_| ProtoError::Malformed("queue length exceeds u32".into()))?;
    let at = match track {
        Some(track) if track > length => {
            return Err(ProtoError::Malformed(
                "playing queue track changed; read it again".into(),
            )
            .into());
        }
        Some(track) if track > 0 => track
            .checked_add(1)
            .ok_or_else(|| ProtoError::Malformed("queue position exceeds u32".into()))?,
        _ => 0,
    };
    let first = if at == 0 {
        length
            .checked_add(1)
            .ok_or_else(|| ProtoError::Malformed("queue position exceeds u32".into()))?
    } else {
        at
    };
    let index = usize::try_from(first - 1)
        .map_err(|_| ProtoError::Malformed("queue position exceeds usize".into()))?;
    let sent = soap::enqueue_uri(t, host, uri, &favorite.metadata, at, next && playing_queue);
    let after = read(t, host).map_err(|_| uncertain("the post-add queue could not be read"))?;
    let count = match sent {
        Ok(added) => {
            if added.first != first {
                return Err(uncertain("the speaker reported a different insertion position"));
            }
            usize::try_from(added.count)
                .map_err(|_| uncertain("the added track count exceeds usize"))?
        }
        Err(error) => {
            if matches!(error, ProtoError::SoapFault { .. }) && before == after {
                return Err(error.into());
            }
            if favorite.kind != FavoriteKind::Track {
                return Err(uncertain("the container add was not acknowledged"));
            }
            // An unchanged queue after a timeout cannot authorize a retry:
            // the original request might still be executing on the speaker.
            1
        }
    };
    let uris = inserted(&before, &after, index, count)
        .ok_or_else(|| uncertain("read-back did not confirm the insertion and its neighbors"))?;
    if favorite.kind == FavoriteKind::Track && (uris.len() != 1 || !same_track(uri, &uris[0])) {
        return Err(uncertain("read-back did not identify the requested track"));
    }
    Ok(Queued {
        first,
        uris: uris.to_vec(),
        playing_queue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(revision: u32, uris: &[&str]) -> Queue {
        Queue {
            revision,
            uris: uris.iter().map(|uri| (*uri).into()).collect(),
        }
    }

    #[test]
    fn a_whole_container_is_verified_between_unchanged_neighbors() {
        let before = queue(7, &["a", "b"]);
        let after = queue(8, &["a", "x", "y", "b"]);
        assert_eq!(inserted(&before, &after, 1, 2).unwrap(), ["x", "y"]);
        assert!(inserted(&before, &after, 0, 2).is_none());
        assert!(inserted(&before, &after, 1, 1).is_none());
    }

    #[test]
    fn duplicates_and_revision_wrap_are_not_lost_or_deduplicated() {
        let before = queue(u32::MAX, &["x"]);
        let after = queue(0, &["x", "x", "x"]);
        assert_eq!(inserted(&before, &after, 1, 2).unwrap(), ["x", "x"]);
        assert!(inserted(&before, &before, 1, 1).is_none());
    }

    #[test]
    fn invalid_ranges_and_concurrent_edits_are_not_acknowledged() {
        let before = queue(7, &["a", "b"]);
        for after in [queue(8, &["z", "b", "x"]), queue(7, &["a", "b", "x"])] {
            assert!(inserted(&before, &after, 2, 1).is_none());
        }
        for (index, count) in [(0, 0), (3, 1), (usize::MAX, 1), (1, usize::MAX)] {
            assert!(inserted(&before, &queue(8, &["a", "b", "x"]), index, count).is_none());
        }
    }

    #[test]
    fn native_and_spotify_identities_are_compared_without_case_folding_ids() {
        assert!(same_track("x-file:a", "x-file:a"));
        assert!(!same_track("x-file:a", "x-file:b"));
        assert!(same_track(
            "x-sonos-spotify:spotify%3atrack%3aAbc?sid=12&sn=1",
            "x-sonos-spotify:spotify%3atrack%3aAbc?sid=12&sn=2"
        ));
        assert!(!same_track(
            "x-sonos-spotify:spotify%3atrack%3aAbc",
            "x-sonos-spotify:spotify%3atrack%3aabc"
        ));
    }
}
