//! Native saved Sonos playlists, separate from My Sonos favorites and the
//! Spotify library. The speaker's `SQ:` catalog supplies both the resource
//! and its container metadata; callers select a fresh catalog entry rather
//! than inventing or accepting a saved-queue file URI.
//!
//! The resulting [`Favorite`] uses the existing container playback and
//! queueing paths, including staged replacement and uncertain-write handling.

use crate::favorites::{Favorite, FavoriteKind};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, SONOS_PLAYLISTS};
use fsonos_proto::didl::{DidlObject, item_metadata};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashSet;

/// Read every saved playlist in the coordinator's own household. Catalog
/// paging must complete at a consistent revision before anything is returned.
/// This only browses: it never reads local files or changes a speaker.
pub fn list<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Vec<Favorite>, CoreError> {
    let host = control::locate(households, coordinator)?.ip;
    let objects = content::browse_all(transport, host, SONOS_PLAYLISTS)?;
    let mut ids = HashSet::new();
    objects
        .iter()
        .map(|object| {
            if !ids.insert(&object.id) {
                return Err(ProtoError::Malformed(
                    "saved Sonos playlist catalog contains duplicate identities".into(),
                )
                .into());
            }
            playlist(object).map_err(Into::into)
        })
        .collect()
}

fn playlist(object: &DidlObject) -> Result<Favorite, ProtoError> {
    // Sonos returns native playlists as both <item> and <container>. The
    // native id and playlist class identify them; replay the observed tag.
    if object.parent_id != SONOS_PLAYLISTS
        || !object
            .id
            .strip_prefix(SONOS_PLAYLISTS)
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        || !(object.class == "object.container.playlistContainer"
            || object
                .class
                .starts_with("object.container.playlistContainer."))
    {
        return Err(ProtoError::Malformed(
            "saved Sonos playlist catalog contains an invalid playlist container".into(),
        ));
    }
    let uri = object
        .res
        .as_ref()
        .map(|resource| resource.uri.trim())
        .filter(|uri| !uri.is_empty())
        .ok_or_else(|| ProtoError::Malformed("saved Sonos playlist has no resource".into()))?;
    Ok(Favorite {
        id: object.id.clone(),
        title: object.title.clone(),
        kind: FavoriteKind::Container,
        uri: Some(uri.into()),
        metadata: item_metadata(object),
        description: Some("Saved Sonos playlist".into()),
        art_uri: object.album_art_uri.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::{DidlKind, parse_didl};

    fn object() -> DidlObject {
        parse_didl(
            r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/"
                xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
              <container id="SQ:7" parentID="SQ:" restricted="true">
                <dc:title>Evening &amp; Friends</dc:title>
                <upnp:class>object.container.playlistContainer</upnp:class>
                <res protocolInfo="file:*:audio/mpegurl:*">file:///jffs/settings/savedqueues.rsq#7</res>
                <desc id="cdudn" nameSpace="urn:example">RINCON_SYNTHETIC</desc>
              </container></DIDL-Lite>"#,
        )
        .unwrap()
        .remove(0)
    }

    #[test]
    fn saved_playlist_keeps_its_container_resource_and_metadata() {
        let object = object();
        let saved = playlist(&object).unwrap();
        assert_eq!(saved.kind, FavoriteKind::Container);
        assert_eq!(saved.id, "SQ:7");
        assert_eq!(saved.title, "Evening & Friends");
        assert_eq!(
            saved.uri.as_deref(),
            Some("file:///jffs/settings/savedqueues.rsq#7")
        );
        assert_eq!(parse_didl(&saved.metadata).unwrap(), [object]);
    }

    #[test]
    fn native_playlists_can_use_the_sonos_item_tag_and_a_playlist_subclass() {
        let mut object = object();
        object.kind = DidlKind::Item;
        object.class.push_str(".sonos");
        let saved = playlist(&object).unwrap();
        assert_eq!(saved.kind, FavoriteKind::Container);
        assert_eq!(parse_didl(&saved.metadata).unwrap(), [object]);
    }

    #[test]
    fn malformed_or_non_playlist_objects_cannot_become_playable_selections() {
        let original = object();
        let mut invalid = Vec::new();
        for id in ["SQ:", "SQ:7/1", "FV:2/7", "SQ:../7"] {
            let mut item = original.clone();
            item.id = id.into();
            invalid.push(item);
        }
        let mut item = original.clone();
        item.parent_id = "Q:0".into();
        invalid.push(item);
        let mut item = original.clone();
        item.class = "object.container.album.musicAlbum".into();
        invalid.push(item);
        let mut item = original;
        item.res = None;
        invalid.push(item);
        for item in invalid {
            assert!(playlist(&item).is_err(), "{item:?}");
        }
    }
}
