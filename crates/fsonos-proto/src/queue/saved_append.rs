//! Append one observed native track to a saved playlist, independently of the
//! playing queue. Wire fields follow the AVTransport service description:
//! <https://sonos.svrooij.io/services/av-transport.html#adduritosavedqueue>.
//! The append index follows SoCo's `add_item_to_sonos_playlist`:
//! <https://soco.readthedocs.io/en/latest/_modules/soco/core.html>.

use crate::didl::{DidlKind, parse_didl};
use crate::{MAX_BODY_BYTES, ProtoError, Transport, soap};
use std::net::IpAddr;

/// Maximum bytes in one retained native track resource.
pub const MAX_SAVED_TRACK_URI_BYTES: usize = 16 * 1024;
/// Maximum bytes in one retained native track's DIDL document.
pub const MAX_SAVED_TRACK_METADATA_BYTES: usize = MAX_BODY_BYTES;
const TRACK_CLASS: &str = "object.item.audioItem.musicTrack";

/// The native acknowledgement, not proof of the resulting saved contents.
/// Callers confirm the complete order and revision before reporting success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedQueueAppend {
    pub added: u32,
    pub total: u32,
    pub update_id: u32,
}

/// Append one native track with its retained DIDL to an exact saved playlist.
/// The nonzero `update_id` is the destination's observed Browse revision.
/// URI and metadata must describe the same single music-track item; neither
/// is resolved, rewritten or expanded here. The URI is bounded to 16 KiB and
/// metadata to the protocol's 4 MiB body limit.
///
/// This sends `AddURIToSavedQueue` exactly once with `AddAtIndex = u32::MAX`.
/// A lost or invalid response may follow a completed write and is never retried.
pub fn append_saved_track<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    playlist_id: &str,
    update_id: u32,
    uri: &str,
    metadata: &str,
) -> Result<SavedQueueAppend, ProtoError> {
    let native = playlist_id
        .strip_prefix("SQ:")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()));
    if !native
        || playlist_id.len() > 1024
        || update_id == 0
        || uri.is_empty()
        || uri.len() > MAX_SAVED_TRACK_URI_BYTES
        || uri.chars().any(|c| c.is_control() || c.is_whitespace())
        || metadata.trim().is_empty()
        || metadata.len() > MAX_SAVED_TRACK_METADATA_BYTES
    {
        return Err(ProtoError::Malformed(
            "saved playlist append requires an exact SQ: id, a nonzero revision and bounded native track URI and metadata".into(),
        ));
    }
    let objects = parse_didl(metadata)?;
    let [item] = objects.as_slice() else {
        return Err(ProtoError::Malformed(
            "saved playlist append requires exactly one native track metadata item".into(),
        ));
    };
    if item.kind != DidlKind::Item
        || !(item.class == TRACK_CLASS
            || item
                .class
                .strip_prefix(TRACK_CLASS)
                .is_some_and(|suffix| suffix.starts_with('.')))
        || item.res.as_ref().is_none_or(|resource| resource.uri != uri)
    {
        return Err(ProtoError::Malformed(
            "saved playlist append must retain the selected music track and its resource".into(),
        ));
    }
    let response = soap::call(
        transport,
        host,
        &soap::AV_TRANSPORT,
        "AddURIToSavedQueue",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("ObjectID", playlist_id),
            ("UpdateID", &update_id.to_string()),
            ("EnqueuedURI", uri),
            ("EnqueuedURIMetaData", metadata),
            ("AddAtIndex", &u32::MAX.to_string()),
        ]),
    )?;
    Ok(SavedQueueAppend {
        added: response.require_u32("NumTracksAdded")?,
        total: response.require_u32("NewQueueLength")?,
        update_id: response.require_u32("NewUpdateID")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml;
    use std::cell::RefCell;

    const URI: &str = "https://media.example.test/song.mp3?first=1&second=2";
    const METADATA: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
      <item id="SQ:1/2" parentID="SQ:1" restricted="true">
        <dc:title>Song &amp; friends</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>
        <res protocolInfo="http-get:*:audio/mpeg:*" duration="0:02:37">https://media.example.test/song.mp3?first=1&amp;second=2</res>
        <desc id="cdudn" nameSpace="urn:example">SYNTHETIC &amp; retained</desc>
      </item></DIDL-Lite>"#;

    #[derive(Default)]
    struct Wire {
        sent: RefCell<Vec<String>>,
        reply: Option<&'static str>,
        lost: bool,
    }

    impl Transport for Wire {
        fn soap_post(
            &self,
            _: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            assert_eq!(path, "/MediaRenderer/AVTransport/Control");
            assert_eq!(
                action,
                "\"urn:schemas-upnp-org:service:AVTransport:1#AddURIToSavedQueue\""
            );
            self.sent.borrow_mut().push(body.into());
            if self.lost {
                return Err(ProtoError::Network {
                    target: "synthetic player".into(),
                    detail: "reply lost".into(),
                });
            }
            Ok(soap::envelope(
                &soap::AV_TRANSPORT,
                "AddURIToSavedQueueResponse",
                self.reply.unwrap_or(
                    "<NumTracksAdded>1</NumTracksAdded><NewQueueLength>4</NewQueueLength><NewUpdateID>18</NewUpdateID>",
                ),
            ))
        }
    }

    fn append(wire: &Wire) -> Result<SavedQueueAppend, ProtoError> {
        append_saved_track(
            wire,
            "192.0.2.10".parse().unwrap(),
            "SQ:7",
            17,
            URI,
            METADATA,
        )
    }

    #[test]
    fn native_append_retains_metadata_and_uses_the_saved_queue_response_fields() {
        let wire = Wire::default();
        assert_eq!(
            append(&wire).unwrap(),
            SavedQueueAppend {
                added: 1,
                total: 4,
                update_id: 18,
            }
        );
        let calls = wire.sent.borrow();
        assert_eq!(calls.len(), 1);
        let document = xml::parse(&calls[0]).unwrap();
        let action = document
            .descendants()
            .find(|node| node.has_tag_name("AddURIToSavedQueue"))
            .unwrap();
        let args: Vec<_> = action
            .children()
            .filter(roxmltree::Node::is_element)
            .map(|node| (node.tag_name().name(), node.text().unwrap_or_default()))
            .collect();
        assert_eq!(
            args,
            [
                ("InstanceID", "0"),
                ("ObjectID", "SQ:7"),
                ("UpdateID", "17"),
                ("EnqueuedURI", URI),
                ("EnqueuedURIMetaData", METADATA),
                ("AddAtIndex", "4294967295"),
            ]
        );
    }

    #[test]
    fn lost_or_malformed_native_acknowledgements_never_replay_the_append() {
        let wire = Wire {
            lost: true,
            ..Wire::default()
        };
        assert!(matches!(append(&wire), Err(ProtoError::Network { .. })));
        assert_eq!(wire.sent.borrow().len(), 1);
        for reply in [
            "<NewQueueLength>4</NewQueueLength><NewUpdateID>18</NewUpdateID>",
            "<NumTracksAdded>-1</NumTracksAdded><NewQueueLength>4</NewQueueLength><NewUpdateID>18</NewUpdateID>",
            "<NumTracksAdded>1</NumTracksAdded><NewQueueLength>4294967296</NewQueueLength><NewUpdateID>18</NewUpdateID>",
            "<NumTracksAdded>1</NumTracksAdded><NewQueueLength>4</NewQueueLength><NewUpdateID>unknown</NewUpdateID>",
        ] {
            let wire = Wire {
                reply: Some(reply),
                ..Wire::default()
            };
            assert!(matches!(append(&wire), Err(ProtoError::Malformed(_))));
            assert_eq!(wire.sent.borrow().len(), 1);
        }
    }

    #[test]
    fn malformed_native_tracks_and_wildcard_revisions_are_rejected_before_io() {
        let wire = Wire::default();
        for (id, revision, uri, metadata) in [
            ("Q:0", 17, URI, METADATA.to_owned()),
            ("SQ:", 17, URI, METADATA.to_owned()),
            ("SQ:7 ", 17, URI, METADATA.to_owned()),
            ("SQ:7", 0, URI, METADATA.to_owned()),
            ("SQ:7", 17, "", METADATA.to_owned()),
            ("SQ:7", 17, "http://example.test/a\nb", METADATA.to_owned()),
            ("SQ:7", 17, URI, String::new()),
            ("SQ:7", 17, URI, "<DIDL-Lite/>".to_owned()),
            ("SQ:7", 17, URI, "<DIDL-Lite>".to_owned()),
            (
                "SQ:7",
                17,
                URI,
                METADATA.replace("musicTrack", "audioBroadcast"),
            ),
            ("SQ:7", 17, URI, METADATA.replace("song.mp3", "another.mp3")),
            (
                "SQ:7",
                17,
                URI,
                METADATA
                    .replace("<item ", "<container ")
                    .replace("</item>", "</container>"),
            ),
            ("SQ:7", 17, URI, "x".repeat(MAX_BODY_BYTES + 1)),
        ] {
            assert!(
                append_saved_track(
                    &wire,
                    "192.0.2.10".parse().unwrap(),
                    id,
                    revision,
                    uri,
                    &metadata,
                )
                .is_err()
            );
        }
        assert!(
            append_saved_track(
                &wire,
                "192.0.2.10".parse().unwrap(),
                "SQ:7",
                17,
                &"x".repeat(MAX_SAVED_TRACK_URI_BYTES + 1),
                METADATA,
            )
            .is_err()
        );
        assert!(wire.sent.borrow().is_empty());
    }
}
