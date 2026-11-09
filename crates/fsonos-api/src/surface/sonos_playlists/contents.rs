//! Ordered native playlist contents without replacing the playing queue.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{control, playlists};
use fsonos_proto::didl::DidlObject;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{SonosPlaylistDto, Surface, find, playlist_name};
use crate::Failure;
use crate::request::zone_name;

const fn default_limit() -> u32 {
    100
}

/// Read one bounded page of a native saved playlist. Continuations retain
/// both the native identity and the exact revision of the first page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SonosPlaylistContentsRequest {
    pub zone: String,
    /// A native SQ: id, current catalog number or unambiguous title. After
    /// the first page use the returned playlist's exact native id.
    pub playlist: String,
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

impl SonosPlaylistContentsRequest {
    /// Reject malformed ranges and unbound continuations before network I/O.
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        playlist_name(&self.playlist)?;
        if !(1..=100).contains(&self.limit) || self.offset.checked_add(self.limit).is_none() {
            return Err(Failure::invalid(
                "playlist contents limit must be 1 to 100 and offset plus limit must fit",
            ));
        }
        if self.expected_revision.as_deref().is_some_and(|revision| {
            revision.trim().is_empty()
                || revision.len() > 4096
                || revision.chars().any(char::is_control)
        }) {
            return Err(Failure::invalid(
                "expected_revision must be nonempty and at most 4096 bytes without control characters",
            ));
        }
        if self.offset > 0 && self.expected_revision.is_none() {
            return Err(Failure::invalid(
                "a saved playlist continuation requires expected_revision from the preceding page",
            ));
        }
        if self.expected_revision.is_some() && !playlists::native_id(self.playlist.trim()) {
            return Err(Failure::invalid(
                "a revision-bound saved playlist read requires the preceding page's exact SQ: id",
            ));
        }
        Ok(())
    }

    /// Encode the same selected-daemon request for CLI and MCP.
    pub fn path(&self) -> Result<String, Failure> {
        self.validate()?;
        let mut path = format!(
            "/sonos/playlists/contents?zone={}&playlist={}&offset={}&limit={}",
            segment(self.zone.trim()),
            segment(self.playlist.trim()),
            self.offset,
            self.limit,
        );
        if let Some(revision) = &self.expected_revision {
            let _ = write!(path, "&expected_revision={}", segment(revision));
        }
        Ok(path)
    }
}

fn segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// A native saved playlist occurrence. Duplicate track URIs retain their
/// separate one-based positions; absent metadata stays unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SonosPlaylistItemDto {
    pub position: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    /// The observed renderer identity, when supplied by the speaker. Reading
    /// an occurrence does not select it or authorize replaying raw metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

impl SonosPlaylistItemDto {
    fn from_object(position: u32, object: DidlObject) -> Self {
        let duration_secs = object.res.as_ref().and_then(|res| res.duration_secs());
        Self {
            position,
            title: object.title,
            creator: object.creator,
            album: object.album,
            art_uri: object.album_art_uri,
            duration_secs,
            uri: object
                .res
                .map(|res| res.uri)
                .filter(|uri| !uri.trim().is_empty() && uri != "NOT_IMPLEMENTED"),
        }
    }
}

/// One coherent page of the saved catalog's contents. This is independent
/// of the group's active queue and never changes its playback or DJ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SonosPlaylistContentsDto {
    pub zone: String,
    pub playlist: SonosPlaylistDto,
    pub revision: String,
    pub total: u32,
    pub offset: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
    pub items: Vec<SonosPlaylistItemDto>,
}

fn one_line(value: &str) -> String {
    value.chars().flat_map(char::escape_debug).collect()
}

impl SonosPlaylistContentsDto {
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!(
            "{}: saved Sonos playlist {:?} ({}), {} track(s).\n",
            one_line(&self.zone),
            self.playlist.title,
            one_line(&self.playlist.id),
            self.total,
        );
        if self.items.is_empty() {
            let _ = writeln!(text, "No entries at offset {}.", self.offset);
        }
        for item in &self.items {
            let _ = writeln!(text, "{:>5}. {}", item.position, one_line(&item.title));
            for (label, value) in [("Artist", &item.creator), ("Album", &item.album)] {
                if let Some(value) = value {
                    let _ = writeln!(text, "       {label}: {}", one_line(value));
                }
            }
            if let Some(duration) = item.duration_secs {
                let _ = writeln!(
                    text,
                    "       Duration: {}:{:02}",
                    duration / 60,
                    duration % 60
                );
            }
        }
        let _ = writeln!(text, "Revision: {}", one_line(&self.revision));
        if let Some(offset) = self.next_offset {
            let _ = writeln!(
                text,
                "Continue with playlist={}, offset={offset} and this expected_revision.",
                one_line(&self.playlist.id),
            );
        } else {
            text.push_str("End of saved playlist reached.\n");
        }
        text.push_str("The playing queue and playback were not changed.\n");
        text
    }
}

impl Surface {
    /// Browse native saved contents under the catalog's read permission.
    pub fn sonos_playlist_contents(
        &self,
        client: &Client,
        req: &SonosPlaylistContentsRequest,
    ) -> Result<SonosPlaylistContentsDto, Failure> {
        self.guard(client).authorize("list_favorites", true)?;
        req.validate()?;
        let (households, group) = self.queue_households(client, req.zone.trim())?;
        let selected = req.playlist.trim();
        let id = if playlists::native_id(selected) {
            selected.to_owned()
        } else {
            let catalog = playlists::list(&*self.transport, &households, &group.coordinator)?;
            find(&catalog, selected)?.id.clone()
        };
        let observed = playlists::contents(
            &*self.transport,
            &households,
            &group.coordinator,
            &id,
            req.offset,
            req.limit,
            req.expected_revision.as_deref(),
        )
        .map_err(Failure::from)
        .inspect_err(|failure| self.notice(failure))?;
        let zone = control::locate(&households, &group.coordinator)?
            .room_name
            .clone();
        let items = (observed.offset + 1..)
            .zip(observed.items)
            .map(|(position, object)| SonosPlaylistItemDto::from_object(position, object))
            .collect();
        Ok(SonosPlaylistContentsDto {
            zone,
            playlist: SonosPlaylistDto::from(&observed.playlist),
            revision: observed.revision,
            total: observed.total,
            offset: observed.offset,
            next_offset: observed.next_offset,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::parse_didl;

    fn request() -> SonosPlaylistContentsRequest {
        SonosPlaylistContentsRequest {
            zone: "Den@S1".into(),
            playlist: "SQ:7".into(),
            offset: 0,
            limit: 100,
            expected_revision: None,
        }
    }

    #[test]
    fn contents_validate_ranges_and_keep_native_identity_on_continuation() {
        for limit in [0, 101, u32::MAX] {
            assert!(
                SonosPlaylistContentsRequest { limit, ..request() }
                    .validate()
                    .is_err()
            );
        }
        assert!(
            SonosPlaylistContentsRequest {
                offset: u32::MAX,
                ..request()
            }
            .validate()
            .is_err()
        );
        assert!(
            SonosPlaylistContentsRequest {
                offset: 1,
                ..request()
            }
            .validate()
            .is_err()
        );
        for playlist in ["Evening", "1", "SQ:", "SQ:7/1"] {
            assert!(
                SonosPlaylistContentsRequest {
                    playlist: playlist.into(),
                    expected_revision: Some("revision".into()),
                    ..request()
                }
                .validate()
                .is_err()
            );
        }
        for revision in [" ".into(), "x\ny".into(), "x".repeat(4097)] {
            assert!(
                SonosPlaylistContentsRequest {
                    expected_revision: Some(revision),
                    ..request()
                }
                .validate()
                .is_err()
            );
        }
        SonosPlaylistContentsRequest {
            offset: 1,
            expected_revision: Some("revision".into()),
            ..request()
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn initial_names_and_tokens_are_encoded_without_becoming_query_syntax() {
        let mut initial = request();
        initial.zone = "Den & Study@S2".into();
        initial.playlist = "Dvořák & Friends?".into();
        assert_eq!(
            initial.path().unwrap(),
            "/sonos/playlists/contents?zone=Den%20%26%20Study%40S2&playlist=Dvo%C5%99%C3%A1k%20%26%20Friends%3F&offset=0&limit=100"
        );
        initial.playlist = "SQ:7".into();
        initial.offset = 100;
        initial.expected_revision = Some("sp1:a&b=2".into());
        assert!(
            initial
                .path()
                .unwrap()
                .ends_with("&expected_revision=sp1%3Aa%26b%3D2")
        );
    }

    #[test]
    fn item_metadata_preserves_ordering_fields_and_unknown_duration() {
        let objects = parse_didl(r#"<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
          <item id="SQ:7/1" parentID="SQ:7"><dc:title>Same title</dc:title><dc:creator>Artist</dc:creator><upnp:album>Album</upnp:album><res duration="0:02:03">test:repeat</res></item>
          <item id="SQ:7/2" parentID="SQ:7"><dc:title>Same title</dc:title><res duration="unknown">test:repeat</res></item>
          <item id="SQ:7/3" parentID="SQ:7"><dc:title>Unavailable</dc:title></item>
        </DIDL-Lite>"#).unwrap();
        let items: Vec<_> = (1..)
            .zip(objects)
            .map(|(position, object)| SonosPlaylistItemDto::from_object(position, object))
            .collect();
        assert_eq!(items[0].position, 1);
        assert_eq!(items[1].position, 2);
        assert_eq!(items[0].uri, items[1].uri);
        assert_eq!(items[0].duration_secs, Some(123));
        assert_eq!(items[1].duration_secs, None);
        assert_eq!(items[2].uri, None);
    }
}
