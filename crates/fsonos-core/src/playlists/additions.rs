//! Compose an existing native playlist without changing the playing queue.
//! Each append uses its latest proved native revision. A failed batch stops
//! with its confirmed progress; no mutation is replayed or rolled back.

use super::contents::{self, CompletePlaylist, evidence_bytes};
use super::selection::native_item;
use super::{
    MAX_PLAYLIST_EDIT_BYTES, MAX_PLAYLIST_EDIT_TRACKS, PlaylistTrackChange, invalid, native_id,
};
use crate::favorites::Favorite;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::didl::DidlObject;
use fsonos_proto::{ProtoError, Transport, queue};
use fsonos_types::PlayerId;

/// Append 1–10 retained native tracks in the supplied order, including repeats.
/// The exact destination SQ: id and its current contents revision are required.
/// Callers obtain the tracks from a checked queue or saved-playlist selection,
/// or render explicit Spotify identities with [`super::prepare_spotify_tracks`].
/// No Spotify account lookup or renderer substitution takes place here.
///
/// The complete destination and incoming metadata must fit the saved-edit
/// bounds before any write. Each native append is confirmed against the full
/// preceding order, then its new revision authorizes the next append. A failure
/// after any confirmed addition is [`CoreError::Uncertain`], even when a later
/// write is definitely refused, so callers cannot replay a partially completed
/// selection. The error reports the confirmed and requested counts.
///
/// Existing queues, playback, volume and DJ ownership remain unchanged.
pub fn append_tracks<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    expected_revision: &str,
    tracks: &[Favorite],
) -> Result<PlaylistTrackChange, CoreError> {
    let incoming = validate(playlist_id, expected_revision, tracks)?;
    let mut current = contents::complete(
        transport,
        households,
        coordinator,
        playlist_id,
        Some(expected_revision),
    )?;
    let requested = u32::try_from(incoming.len()).expect("native selections are bounded");
    if current.total.saturating_add(requested) > MAX_PLAYLIST_EDIT_TRACKS {
        return Err(invalid(
            "the completed saved playlist would exceed 1000 tracks",
        ));
    }
    let bytes = current
        .items
        .iter()
        .chain(&incoming)
        .fold(0_usize, |bytes, item| {
            bytes.saturating_add(evidence_bytes(item))
        });
    if bytes > MAX_PLAYLIST_EDIT_BYTES {
        return Err(invalid(
            "the completed saved playlist evidence would exceed 4 MiB",
        ));
    }
    if current.update_id == 0 {
        return Err(invalid(
            "a zero native playlist revision cannot guard an append; refresh its contents",
        ));
    }
    let previous_total = current.total;
    for (index, (track, item)) in tracks.iter().zip(&incoming).enumerate() {
        let confirmed = u32::try_from(index).expect("native selections are bounded");
        current
            .group
            .confirm(transport, current.host, coordinator)
            .map_err(|error| after_progress(error, confirmed, requested))?;
        let uri = track.uri.as_deref().expect("native tracks have a resource");
        let reply = match queue::append_saved_track(
            transport,
            current.host,
            playlist_id,
            current.update_id,
            uri,
            &track.metadata,
        ) {
            Ok(reply) => reply,
            Err(ProtoError::SoapFault {
                code: queue::STALE_QUEUE_FAULT,
                ..
            }) => {
                return Err(after_progress(
                    CoreError::PlaylistChanged(
                        "the saved playlist changed before its native append; refresh its contents"
                            .into(),
                    ),
                    confirmed,
                    requested,
                ));
            }
            Err(error @ ProtoError::SoapFault { .. }) => {
                return Err(after_progress(error.into(), confirmed, requested));
            }
            Err(error) => return Err(uncertain(confirmed, requested, error)),
        };
        if reply.added != 1
            || reply.total != current.total + 1
            || reply.update_id == 0
            || reply.update_id == current.update_id
        {
            return Err(uncertain(
                confirmed,
                requested,
                "the native acknowledgement did not match one appended track",
            ));
        }
        let after = contents::complete(transport, households, coordinator, playlist_id, None)
            .map_err(|error| uncertain(confirmed, requested, error))?;
        if !appended(&current, &after, item, reply.update_id) {
            return Err(uncertain(
                confirmed,
                requested,
                "saved contents did not confirm the original prefix and appended track metadata",
            ));
        }
        current = after;
    }
    Ok(PlaylistTrackChange {
        playlist: current.playlist,
        revision: current.revision,
        previous_total,
        total: current.total,
        changed: true,
    })
}

fn validate(
    playlist_id: &str,
    expected_revision: &str,
    tracks: &[Favorite],
) -> Result<Vec<DidlObject>, CoreError> {
    if !native_id(playlist_id)
        || playlist_id.len() > 1024
        || expected_revision.trim().is_empty()
        || expected_revision.len() > 4096
        || expected_revision.chars().any(char::is_control)
    {
        return Err(invalid(
            "appending saved tracks requires an exact SQ: id and observed revision",
        ));
    }
    if tracks.is_empty() || tracks.len() > control::MAX_TRACK_BATCH {
        return Err(invalid(
            "a saved playlist append needs between 1 and 10 native tracks",
        ));
    }
    let metadata_bytes = tracks.iter().fold(0_usize, |bytes, track| {
        bytes.saturating_add(track.metadata.len())
    });
    if metadata_bytes > MAX_PLAYLIST_EDIT_BYTES {
        return Err(invalid("selected native track metadata exceeds 4 MiB"));
    }
    tracks
        .iter()
        .map(|track| {
            let item = native_item(track)?;
            let uri = track.uri.as_deref().unwrap_or_default();
            if uri.len() > queue::MAX_SAVED_TRACK_URI_BYTES
                || item.res.as_ref().map(|resource| resource.uri.as_str()) != Some(uri)
            {
                return Err(invalid(
                    "selected native track resource does not match its bounded metadata",
                ));
            }
            Ok(item)
        })
        .collect()
}

fn appended(
    before: &CompletePlaylist,
    after: &CompletePlaylist,
    incoming: &DidlObject,
    update_id: u32,
) -> bool {
    before.host == after.host
        && before.group == after.group
        && before.playlist == after.playlist
        && after.update_id == update_id
        && after.total == before.total + 1
        && after.items.len() == before.items.len() + 1
        && before
            .items
            .iter()
            .zip(&after.items)
            .all(|(expected, actual)| expected == actual)
        && after
            .items
            .last()
            .is_some_and(|actual| appended_track(incoming, actual, &before.playlist.id))
}

/// A newly rendered Spotify item has no catalog duration or artwork yet.
/// Sonos can enrich those unknown fields when it saves the track. Retain every
/// supplied value, the exact resource and account descriptor, while allowing
/// only absent descriptive/resource fields to acquire observed values.
fn appended_track(incoming: &DidlObject, actual: &DidlObject, parent: &str) -> bool {
    let mut expected = incoming.clone();
    expected.id.clone_from(&actual.id);
    parent.clone_into(&mut expected.parent_id);
    if expected.creator.is_none() {
        expected.creator.clone_from(&actual.creator);
    }
    if expected.album.is_none() {
        expected.album.clone_from(&actual.album);
    }
    if expected.album_art_uri.is_none() {
        expected.album_art_uri.clone_from(&actual.album_art_uri);
    }
    if let (Some(resource), Some(observed)) = (&mut expected.res, &actual.res) {
        if resource.protocol_info.is_none() {
            resource.protocol_info.clone_from(&observed.protocol_info);
        }
        if resource.duration.is_none() {
            resource.duration.clone_from(&observed.duration);
        }
    }
    expected == *actual
}

fn after_progress(error: CoreError, confirmed: u32, requested: u32) -> CoreError {
    if confirmed == 0 {
        error
    } else {
        uncertain(confirmed, requested, error)
    }
}

fn uncertain(confirmed: u32, requested: u32, detail: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "saved playlist append stopped with {confirmed} of {requested} additions confirmed: {detail}; \
         an unconfirmed append may also have applied; inspect the original saved playlist before \
         another edit; no append was replayed"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::{DidlDesc, parse_didl};

    fn track() -> DidlObject {
        parse_didl(concat!(
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" ",
            "xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">",
            "<item id=\"source\" parentID=\"-1\" restricted=\"true\">",
            "<dc:title>Selected track</dc:title>",
            "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
            "<res>https://example.invalid/selected.flac</res></item></DIDL-Lite>",
        ))
        .unwrap()
        .remove(0)
    }

    fn enriched(source: &DidlObject) -> DidlObject {
        let mut actual = source.clone();
        actual.id = "SQ:7/4".into();
        actual.parent_id = "SQ:7".into();
        actual.creator = Some("Catalog artist".into());
        actual.album = Some("Catalog album".into());
        actual.album_art_uri = Some("https://example.invalid/art.jpg".into());
        let resource = actual.res.as_mut().unwrap();
        resource.protocol_info = Some("http-get:*:audio/flac:*".into());
        resource.duration = Some("0:03:00".into());
        actual
    }

    #[test]
    fn a_saved_append_accepts_enrichment_of_unknown_metadata() {
        let incoming = track();
        let actual = enriched(&incoming);
        assert!(appended_track(&incoming, &actual, "SQ:7"));
        assert!(appended_track(&actual, &actual, "SQ:7"));
        assert_eq!(incoming, track(), "confirmation does not alter its input");
    }

    #[test]
    fn enrichment_never_replaces_supplied_metadata() {
        let incoming = enriched(&track());
        for field in 0..5 {
            let mut actual = incoming.clone();
            match field {
                0 => actual.creator = Some("Different artist".into()),
                1 => actual.album = Some("Different album".into()),
                2 => actual.album_art_uri = Some("https://example.invalid/other.jpg".into()),
                3 => {
                    actual.res.as_mut().unwrap().protocol_info =
                        Some("http-get:*:audio/mpeg:*".into());
                }
                _ => actual.res.as_mut().unwrap().duration = Some("0:04:00".into()),
            }
            assert!(!appended_track(&incoming, &actual, "SQ:7"), "field {field}");
        }
        let mut missing = incoming.clone();
        missing.res.as_mut().unwrap().duration = None;
        assert!(!appended_track(&incoming, &missing, "SQ:7"));
    }

    #[test]
    fn enrichment_cannot_hide_a_changed_source_account_or_saved_parent() {
        let incoming = track();
        let actual = enriched(&incoming);
        assert!(!appended_track(&incoming, &actual, "SQ:8"));
        for field in 0..6 {
            let mut changed = actual.clone();
            match field {
                0 => {
                    changed.res.as_mut().unwrap().uri = "https://example.invalid/other.flac".into();
                }
                1 => changed.res = None,
                2 => changed.title = "Different track".into(),
                3 => changed.class = "object.item.audioItem.audioBroadcast".into(),
                4 => changed.restricted = false,
                _ => {
                    changed.desc = Some(DidlDesc {
                        id: "cdudn".into(),
                        name_space: "urn:schemas-rinconnetworks-com:metadata-1-0/".into(),
                        value: "different-service-account".into(),
                    });
                }
            }
            assert!(
                !appended_track(&incoming, &changed, "SQ:7"),
                "field {field}"
            );
        }
        let mut supplied = incoming;
        supplied.desc = Some(DidlDesc {
            id: "cdudn".into(),
            name_space: "urn:schemas-rinconnetworks-com:metadata-1-0/".into(),
            value: "synthetic-service-account".into(),
        });
        assert!(!appended_track(&supplied, &actual, "SQ:7"));
        let mut retained = actual;
        retained.desc.clone_from(&supplied.desc);
        assert!(appended_track(&supplied, &retained, "SQ:7"));
    }
}
