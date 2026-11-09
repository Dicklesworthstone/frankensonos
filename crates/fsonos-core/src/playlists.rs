//! Native saved Sonos playlists, separate from My Sonos favorites and the
//! Spotify library. The speaker's `SQ:` catalog supplies both the resource
//! and its container metadata; callers select a fresh catalog entry rather
//! than inventing or accepting a saved-queue file URI.
//!
//! The resulting [`Favorite`] uses the existing container playback and
//! queueing paths, including staged replacement and uncertain-write handling.
//! [`save_queue`] creates a new native playlist from the current queue without
//! changing playback; its confirmed catalog entry is immediately recallable.

use crate::favorites::{Favorite, FavoriteKind};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, QUEUE, SONOS_PLAYLISTS};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{DidlObject, item_metadata};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::net::IpAddr;

mod contents;
mod selection;

pub use contents::{MAX_CONTENTS_PAGE, PlaylistContents, contents};
pub use selection::select_track;

/// The native `SaveQueue` Title argument holds at most 1023 UTF-8 bytes.
/// The S1 handler has a 1024-byte argument buffer (docs/RE_COPY_AUDIT.md).
pub const MAX_PLAYLIST_TITLE_BYTES: usize = 1023;

/// Validate a new playlist title before any I/O, returning its trimmed form.
pub fn validate_title(title: &str) -> Result<&str, &'static str> {
    let title = title.trim();
    if title.is_empty() {
        return Err("a saved Sonos playlist needs a nonempty title");
    }
    if title.chars().any(char::is_control) {
        return Err("a saved Sonos playlist title cannot contain control characters");
    }
    if title.len() > MAX_PLAYLIST_TITLE_BYTES {
        return Err("a saved Sonos playlist title cannot exceed 1023 UTF-8 bytes");
    }
    Ok(title)
}

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

/// Save the selected group's current queue as a new native Sonos playlist.
/// The source, playhead, queue and DJ ownership are left alone; a queue can
/// be saved even while a different source is playing. Equal titles create
/// separate playlists, never overwrite an existing one.
///
/// The target must still lead the observed group, and its queue must be
/// nonempty. `SaveQueue` is sent exactly once with an empty `ObjectID`.
/// Success returns the new identity and metadata read back from the native
/// catalog. A lost/malformed reply or failed read-back is uncertain, not a
/// reason to retry a write that could create another playlist.
///
/// `SaveQueue` has no queue-revision argument: Sonos saves the queue at the
/// time it handles the request. These reads do not lock out other controllers.
pub fn save_queue<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    title: &str,
) -> Result<Favorite, CoreError> {
    let title = validate_title(title).map_err(invalid)?;
    let mut owning = households
        .iter()
        .filter(|house| house.player(coordinator).is_some());
    let house = owning
        .next()
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    if owning.next().is_some() {
        return Err(invalid(
            "the playlist target belongs to more than one household",
        ));
    }
    let mut groups = house
        .groups
        .iter()
        .filter(|group| group.coordinator == *coordinator);
    let group = groups
        .next()
        .ok_or_else(|| invalid("the playlist target is not the group's coordinator"))?;
    if groups.next().is_some() {
        return Err(invalid("the playlist target's group is ambiguous"));
    }
    let host = control::locate(households, coordinator)?.ip;
    let destination = GroupProof::read(transport, host, coordinator)?;
    let mut expected_members = group.members.clone();
    expected_members.sort_by(|left, right| left.0.cmp(&right.0));
    expected_members.dedup();
    if destination.members != expected_members {
        return Err(invalid(
            "the playlist target's group membership changed; refresh the group",
        ));
    }
    let before = list(transport, households, coordinator)?;
    let queue = content::browse_range_snapshot(transport, host, QUEUE, 0, 1)?;
    if queue.objects.is_empty() {
        return Err(invalid(
            "the selected group's queue is empty; nothing was saved",
        ));
    }
    destination.confirm(transport, host, coordinator)?;
    let id = match soap::save_queue(transport, host, title) {
        Ok(id) => id,
        Err(error @ ProtoError::SoapFault { .. }) => return Err(error.into()),
        Err(error) => return Err(uncertain(error)),
    };
    if !native_id(&id) || before.iter().any(|item| item.id == id) {
        return Err(uncertain(
            "the reply did not identify a new native saved playlist",
        ));
    }
    destination
        .confirm(transport, host, coordinator)
        .map_err(uncertain)?;
    list(transport, households, coordinator)
        .map_err(uncertain)?
        .into_iter()
        .find(|item| item.id == id && item.title == title)
        .ok_or_else(|| uncertain("the new playlist and title were not found in the saved catalog"))
}

fn invalid(detail: &str) -> CoreError {
    ProtoError::Malformed(detail.into()).into()
}

fn uncertain(detail: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "Sonos playlist save could not be confirmed: {detail}; the playlist may have been saved; \
         refresh the saved-playlist catalog before trying again"
    ))
}

#[derive(Debug, PartialEq, Eq)]
struct GroupProof {
    members: Vec<PlayerId>,
    boot_seq: Option<u32>,
}

impl GroupProof {
    fn read<T: Transport + ?Sized>(
        transport: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, CoreError> {
        let topology = get_zone_group_state(transport, host)?;
        let mut groups = topology
            .groups
            .iter()
            .filter(|group| group.coordinator == *coordinator);
        let group = groups.next().ok_or_else(|| {
            invalid("the playlist target no longer coordinates its group; refresh the group")
        })?;
        if groups.next().is_some() {
            return Err(invalid("the playlist target's current group is ambiguous"));
        }
        let own = group
            .members
            .iter()
            .find(|member| {
                member.uuid == *coordinator && member.ip() == Some(host) && !member.is_zone_bridge
            })
            .ok_or_else(|| {
                invalid("the playlist target's address no longer identifies its coordinator")
            })?;
        let mut members: Vec<PlayerId> = group
            .members
            .iter()
            .filter(|member| !member.is_zone_bridge)
            .flat_map(|member| std::iter::once(member).chain(&member.satellites))
            .map(|member| member.uuid.clone())
            .collect();
        members.sort_by(|left, right| left.0.cmp(&right.0));
        members.dedup();
        Ok(Self {
            members,
            boot_seq: own.boot_seq,
        })
    }

    fn confirm<T: Transport + ?Sized>(
        &self,
        transport: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), CoreError> {
        if *self != Self::read(transport, host, coordinator)? {
            return Err(invalid(
                "the playlist target's group or boot identity changed while saving",
            ));
        }
        Ok(())
    }
}

/// Whether an identifier names one native saved Sonos playlist, not its
/// catalog, a queue, or a caller-supplied file resource.
#[must_use]
pub fn native_id(id: &str) -> bool {
    id.strip_prefix(SONOS_PLAYLISTS)
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

fn playlist(object: &DidlObject) -> Result<Favorite, ProtoError> {
    // Sonos returns native playlists as both <item> and <container>. The
    // native id and playlist class identify them; replay the observed tag.
    if object.parent_id != SONOS_PLAYLISTS
        || !native_id(&object.id)
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

    #[test]
    fn saved_playlist_titles_are_trimmed_and_bounded_in_utf8_bytes() {
        assert_eq!(
            validate_title("  Evening & Friends  ").unwrap(),
            "Evening & Friends"
        );
        assert!(validate_title(&"a".repeat(MAX_PLAYLIST_TITLE_BYTES)).is_ok());
        assert!(validate_title(&"a".repeat(MAX_PLAYLIST_TITLE_BYTES + 1)).is_err());
        assert!(validate_title(&"é".repeat(511)).is_ok());
        assert!(validate_title(&"é".repeat(512)).is_err());
        for title in ["", "  \t  ", "Evening\nFriends", "Evening\0Friends"] {
            assert!(validate_title(title).is_err(), "{title:?}");
        }
    }

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
