//! Native saved-playlist edits do not operate on the playing queue.
//!
//! Wire fields: <https://sonos.svrooij.io/services/av-transport.html#reordertracksinsavedqueue>.
//! Zero-based CSV positions and an empty destination list for removal follow
//! SoCo's `reorder_sonos_playlist` and `clear_sonos_playlist`:
//! <https://soco.readthedocs.io/en/latest/_modules/soco/core.html>.

use crate::{ProtoError, Transport, soap};
use std::collections::HashSet;
use std::net::IpAddr;

/// Bound both native CSV arguments and the controller's complete observation.
pub const MAX_SAVED_QUEUE_EDIT_TRACKS: u32 = 1000;

/// The speaker's acknowledgement. A caller must still verify the complete
/// saved order and the new revision before reporting a confirmed change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedQueueChange {
    pub length_change: i32,
    pub total: u32,
    pub update_id: u32,
}

/// Send one `ReorderTracksInSavedQueue` with its observed, nonzero `UpdateID`.
/// `tracks` are distinct zero-based original positions, in descending order.
/// `positions` are their corresponding zero-based final positions, or `None`
/// to remove every selected occurrence. This is one CSV operation, never a
/// sequence of mutable-index moves. The public core uses one-based ranges.
///
/// No failed response is retried: the saved playlist may already have changed.
pub fn reorder_saved_tracks<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    playlist_id: &str,
    update_id: u32,
    tracks: &[u32],
    positions: Option<&[u32]>,
) -> Result<SavedQueueChange, ProtoError> {
    let native = playlist_id
        .strip_prefix("SQ:")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()));
    if !native
        || playlist_id.len() > 1024
        || update_id == 0
        || tracks.is_empty()
        || tracks.len() > MAX_SAVED_QUEUE_EDIT_TRACKS as usize
        || tracks.windows(2).any(|pair| pair[0] <= pair[1])
        || positions.is_some_and(|positions| {
            positions.len() != tracks.len()
                || positions.iter().collect::<HashSet<_>>().len() != positions.len()
        })
    {
        return Err(ProtoError::Malformed(
            "saved playlist edits require an exact SQ: id, a nonzero revision and bounded distinct positions".into(),
        ));
    }
    let csv = |values: &[u32]| {
        values
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let tracks = csv(tracks);
    let positions = positions.map(csv).unwrap_or_default();
    let response = soap::call(
        transport,
        host,
        &soap::AV_TRANSPORT,
        "ReorderTracksInSavedQueue",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("ObjectID", playlist_id),
            ("UpdateID", &update_id.to_string()),
            ("TrackList", &tracks),
            ("NewPositionList", &positions),
        ]),
    )?;
    let length_change = response
        .require("QueueLengthChange")?
        .trim()
        .parse()
        .map_err(|_| ProtoError::Malformed("invalid QueueLengthChange".into()))?;
    Ok(SavedQueueChange {
        length_change,
        total: response.require_u32("NewQueueLength")?,
        update_id: response.require_u32("NewUpdateID")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Wire {
        sent: RefCell<Vec<String>>,
        reply: Option<&'static str>,
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
                "\"urn:schemas-upnp-org:service:AVTransport:1#ReorderTracksInSavedQueue\""
            );
            self.sent.borrow_mut().push(body.into());
            Ok(soap::envelope(
                &soap::AV_TRANSPORT,
                "ReorderTracksInSavedQueueResponse",
                self.reply.unwrap_or(
                    "<QueueLengthChange> -2 </QueueLengthChange><NewQueueLength>3</NewQueueLength><NewUpdateID>18</NewUpdateID>",
                ),
            ))
        }
    }

    #[test]
    fn removal_encodes_zero_based_original_positions_and_one_empty_destination() {
        let wire = Wire::default();
        let result = reorder_saved_tracks(
            &wire,
            "192.0.2.10".parse().unwrap(),
            "SQ:7",
            17,
            &[3, 2],
            None,
        )
        .unwrap();
        assert_eq!(
            result,
            SavedQueueChange {
                length_change: -2,
                total: 3,
                update_id: 18,
            }
        );
        let calls = wire.sent.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(
            "<InstanceID>0</InstanceID><ObjectID>SQ:7</ObjectID><UpdateID>17</UpdateID><TrackList>3,2</TrackList><NewPositionList></NewPositionList>"
        ));
    }

    #[test]
    fn a_move_is_one_csv_request_and_an_invalid_acknowledgement_is_not_replayed() {
        let wire = Wire {
            reply: Some("<NewUpdateID>18</NewUpdateID>"),
            ..Wire::default()
        };
        assert!(
            reorder_saved_tracks(
                &wire,
                "192.0.2.10".parse().unwrap(),
                "SQ:7",
                17,
                &[3, 2, 1, 0],
                Some(&[0, 1, 2, 3]),
            )
            .is_err()
        );
        let calls = wire.sent.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("<TrackList>3,2,1,0</TrackList>"));
        assert!(calls[0].contains("<NewPositionList>0,1,2,3</NewPositionList>"));
    }

    #[test]
    fn malformed_selection_and_wildcard_revision_never_reach_the_player() {
        let wire = Wire::default();
        for (id, revision, tracks, positions) in [
            ("Q:0", 17, vec![1], None),
            ("SQ:7", 0, vec![1], None),
            ("SQ:7", 17, vec![], None),
            ("SQ:7", 17, vec![1, 1], None),
            ("SQ:7", 17, vec![0, 1], None),
            ("SQ:7", 17, vec![2, 1], Some(vec![0])),
            ("SQ:7", 17, vec![2, 1], Some(vec![0, 0])),
        ] {
            assert!(
                reorder_saved_tracks(
                    &wire,
                    "192.0.2.10".parse().unwrap(),
                    id,
                    revision,
                    &tracks,
                    positions.as_deref(),
                )
                .is_err()
            );
        }
        assert!(wire.sent.borrow().is_empty());
    }
}
