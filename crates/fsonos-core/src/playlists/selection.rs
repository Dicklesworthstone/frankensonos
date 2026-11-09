//! Resolve one observed saved-playlist occurrence into a native track. The
//! caller supplies its position and revision, never a renderer URI or DIDL.

use super::contents;
use crate::favorites::{Favorite, FavoriteKind};
use crate::{CoreError, HouseholdState};
use fsonos_proto::content::MAX_BROWSE_OBJECTS;
use fsonos_proto::didl::{DidlKind, DidlObject, item_metadata, spotify_uri_from_renderer_uri};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;

/// Resolve exactly one track from a previously observed saved playlist.
///
/// `playlist_id` must be the native `SQ:<n>` id, `position` is one-based, and
/// `expected_revision` is mandatory even for the first track. The same
/// household, coordinator, boot, group and playlist must still be observed by
/// [`contents`]. Duplicate resources retain their separate occurrence IDs.
///
/// This performs only reads. The returned native track can use the existing
/// favorite playback and queue paths; it retains the speaker's resource and
/// metadata, without a Spotify catalog lookup or a guessed account descriptor.
/// Containers, streams and unavailable entries cannot become track actions.
/// A successful observation does not lock the speaker against later changes.
pub fn select_track<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    position: u32,
    expected_revision: &str,
) -> Result<Favorite, CoreError> {
    if position == 0 || position > MAX_BROWSE_OBJECTS {
        return Err(invalid(
            "saved playlist track position is outside the supported range",
        ));
    }
    if expected_revision.trim().is_empty() {
        return Err(invalid(
            "selecting a saved playlist track requires its observed revision",
        ));
    }
    let page = contents(
        transport,
        households,
        coordinator,
        playlist_id,
        position - 1,
        1,
        Some(expected_revision),
    )?;
    let [item] = page.items.as_slice() else {
        return Err(invalid(
            "the saved playlist has no track at the selected position",
        ));
    };
    track(item)
}

fn track(item: &DidlObject) -> Result<Favorite, CoreError> {
    const TRACK: &str = "object.item.audioItem.musicTrack";
    if item.kind != DidlKind::Item
        || !(item.class == TRACK
            || item
                .class
                .strip_prefix(TRACK)
                .is_some_and(|suffix| suffix.starts_with('.')))
    {
        return Err(invalid(
            "the selected saved playlist occurrence is not an audio track",
        ));
    }
    let uri = item
        .res
        .as_ref()
        .map(|resource| resource.uri.trim())
        .filter(|uri| track_resource(uri))
        .ok_or_else(|| invalid("the selected saved playlist track has no usable resource"))?;
    Ok(Favorite {
        id: item.id.clone(),
        title: item.title.clone(),
        kind: FavoriteKind::Track,
        uri: Some(uri.into()),
        metadata: item_metadata(item),
        description: item.creator.clone(),
        art_uri: item.album_art_uri.clone(),
    })
}

fn track_resource(uri: &str) -> bool {
    // Service schemes are extensible, so do not require a Spotify URI or a
    // service-account descriptor. Require an actual absolute resource, and
    // exclude sources known to select a group, stream, queue or container.
    let Some((scheme, resource)) = uri.split_once(':') else {
        return false;
    };
    if resource.is_empty()
        || !scheme
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
        || uri
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return false;
    }
    let scheme = scheme.to_ascii_lowercase();
    if matches!(
        scheme.as_str(),
        "x-rincon"
            | "x-rincon-queue"
            | "x-rincon-cpcontainer"
            | "x-rincon-stream"
            | "x-sonos-htastream"
            | "x-sonos-vli"
            | "x-rincon-mp3radio"
            | "x-sonosapi-stream"
            | "x-sonosapi-radio"
            | "x-sonosapi-hls"
            | "aac"
    ) {
        return false;
    }
    let spotify = match scheme.as_str() {
        "spotify" => Some(uri.to_owned()),
        "x-sonos-spotify" => spotify_uri_from_renderer_uri(uri),
        _ => return true,
    };
    spotify.as_deref().is_some_and(|uri| {
        ["spotify:track:", "spotify:episode:"]
            .iter()
            .any(|&prefix| uri.strip_prefix(prefix).is_some_and(|id| !id.is_empty()))
    })
}

fn invalid(detail: &str) -> CoreError {
    ProtoError::Malformed(detail.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::{DidlRes, parse_didl};
    use std::net::IpAddr;

    fn item() -> DidlObject {
        parse_didl(concat!(
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" ",
            "xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">",
            "<item id=\"SQ:7/3\" parentID=\"SQ:7\" restricted=\"true\">",
            "<dc:title>Native &amp; unchanged</dc:title>",
            "<dc:creator>Library artist</dc:creator><upnp:album>Album</upnp:album>",
            "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
            "<upnp:albumArtURI>https://example.invalid/art.jpg</upnp:albumArtURI>",
            "<res protocolInfo=\"http-get:*:audio/flac:*\" duration=\"0:03:00\">",
            "https://example.invalid/track.flac</res></item></DIDL-Lite>",
        ))
        .unwrap()
        .remove(0)
    }

    #[test]
    fn local_tracks_keep_their_complete_metadata_without_an_account_descriptor() {
        for uri in [
            "https://example.invalid/track.flac",
            "http://example.invalid/track.mp3",
            "x-file-cifs://example.invalid/music/track.flac",
            "file:///music/track.flac",
        ] {
            let mut item = item();
            item.res.as_mut().unwrap().uri = uri.into();
            let selected = track(&item).unwrap();
            assert_eq!(selected.id, "SQ:7/3");
            assert_eq!(selected.kind, FavoriteKind::Track);
            assert_eq!(selected.uri.as_deref(), Some(uri));
            assert_eq!(parse_didl(&selected.metadata).unwrap(), [item]);
        }
        let mut subclass = item();
        subclass.class.push_str(".vendor");
        assert!(track(&subclass).is_ok());
    }

    #[test]
    fn non_tracks_and_missing_metadata_do_not_become_playable_items() {
        let mut container = item();
        container.kind = DidlKind::Container;
        assert!(track(&container).is_err());
        for class in [
            "",
            "object.item",
            "object.item.audioItem",
            "object.item.videoItem",
            "object.item.audioItem.musicTrackExtra",
            "object.item.audioItem.audioBroadcast",
            "object.container.playlistContainer",
        ] {
            let mut item = item();
            item.class = class.into();
            assert!(track(&item).is_err(), "{class}");
        }
        let mut unavailable = item();
        unavailable.res = None;
        assert!(track(&unavailable).is_err());
    }

    #[test]
    fn sentinel_and_non_track_resources_are_rejected_even_with_a_track_class() {
        for uri in [
            "",
            " ",
            "NOT_IMPLEMENTED",
            "NOT_AVAILABLE",
            "0",
            "relative/track.mp3",
            ":missing-scheme",
            "http:",
            "https://example.invalid/has space.mp3",
            "https://example.invalid/track\n.mp3",
            "x-rincon:another-player",
            "x-rincon-queue:player#0",
            "x-rincon-cpcontainer:playlist",
            "x-rincon-stream:player",
            "x-sonos-htastream:player:spdif",
            "x-sonos-vli:player:2,spotify:session",
            "x-rincon-mp3radio://station.invalid/live",
            "x-sonosapi-stream:station",
            "x-sonosapi-radio:station",
            "x-sonosapi-hls:station",
            "aac://station.invalid/live",
            "spotify:album:Album",
            "spotify:track:",
            "x-sonos-spotify:spotify%3aplaylist%3aList?sid=1",
            "x-sonos-spotify:unknown",
        ] {
            let mut item = item();
            item.res = Some(DidlRes {
                uri: uri.into(),
                protocol_info: None,
                duration: None,
            });
            assert!(track(&item).is_err(), "{uri:?}");
        }
    }

    struct NoIo;

    impl Transport for NoIo {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            panic!("an invalid selection must not perform I/O")
        }
    }

    #[test]
    fn invalid_selection_arguments_fail_before_any_io() {
        let coordinator = PlayerId("synthetic-player".into());
        for (id, position, revision) in [
            ("SQ:7", 0, "observed"),
            ("SQ:7", MAX_BROWSE_OBJECTS + 1, "observed"),
            ("SQ:7", u32::MAX, "observed"),
            ("Evening", 1, "observed"),
            ("SQ:7/3", 1, "observed"),
            ("SQ:7", 1, ""),
            ("SQ:7", 1, "  "),
            ("SQ:7", 1, "invalid\nrevision"),
        ] {
            assert!(matches!(
                select_track(&NoIo, &[], &coordinator, id, position, revision),
                Err(CoreError::Proto(ProtoError::Malformed(_)))
            ));
        }
        assert!(select_track(&NoIo, &[], &coordinator, "SQ:7", 1, &"x".repeat(4097)).is_err());
    }
}
