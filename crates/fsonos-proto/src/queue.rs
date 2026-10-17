//! Native queue reordering. Positions refer to the queue BEFORE the move;
//! `insert_before = length + 1` appends. No clear, enqueue, seek or Play.
//!
//! Wire contract: svrooij/sonos-api-docs, AVTransport/ReorderTracksInQueue.
//! A caller must resolve the coordinator, check the selected identities and
//! bounds against a coherent browse, and reconcile uncertain write replies.

use crate::{ProtoError, Transport, soap};
use std::net::IpAddr;

/// The UPnP fault a Sonos player answers `ReorderTracksInQueue` or
/// `RemoveTrackRangeFromQueue` with when the `UpdateID` it carries is not the
/// queue's current Browse `UpdateID`: the queue changed, and nothing was done.
/// Observed on S1 and S2 players (2026-10-17); `UpdateID` 0 skips the check.
pub const STALE_QUEUE_FAULT: u16 = 1028;

/// Move `count` consecutive tracks beginning at one-based `first` before
/// one-based `insert_before` in the original queue. Pass its observed,
/// nonzero `update_id`: zero is a wildcard, which this wrapper refuses.
/// The call is sent ONCE. An error may mean only its reply was lost.
pub fn reorder_tracks<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    first: u32,
    count: u32,
    insert_before: u32,
    update_id: u32,
) -> Result<(), ProtoError> {
    if first == 0
        || count == 0
        || insert_before == 0
        || update_id == 0
        || first.checked_add(count - 1).is_none()
    {
        return Err(ProtoError::Malformed(
            "queue moves require one-based positions, a nonempty range and a nonzero revision"
                .into(),
        ));
    }
    soap::call(
        transport,
        host,
        &soap::AV_TRANSPORT,
        "ReorderTracksInQueue",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("StartingIndex", &first.to_string()),
            ("NumberOfTracks", &count.to_string()),
            ("InsertBefore", &insert_before.to_string()),
            ("UpdateID", &update_id.to_string()),
        ]),
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Wire {
        calls: Cell<usize>,
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
            self.calls.set(self.calls.get() + 1);
            assert_eq!(path, "/MediaRenderer/AVTransport/Control");
            assert_eq!(
                action,
                "\"urn:schemas-upnp-org:service:AVTransport:1#ReorderTracksInQueue\""
            );
            assert!(body.contains(
                "<InstanceID>0</InstanceID><StartingIndex>2</StartingIndex>\
                 <NumberOfTracks>3</NumberOfTracks><InsertBefore>8</InsertBefore>\
                 <UpdateID>17</UpdateID>"
            ));
            if self.lost {
                return Err(ProtoError::Network {
                    target: "test-player".into(),
                    detail: "reply lost".into(),
                });
            }
            Ok(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                 <s:Body><u:ReorderTracksInQueueResponse \
                 xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>\
                 </s:Body></s:Envelope>"
                    .into(),
            )
        }
    }

    #[test]
    fn encodes_the_native_move_in_wire_order() {
        let wire = Wire {
            calls: Cell::new(0),
            lost: false,
        };
        reorder_tracks(&wire, "127.0.0.1".parse().unwrap(), 2, 3, 8, 17).unwrap();
        assert_eq!(wire.calls.get(), 1);
    }

    #[test]
    fn a_lost_reply_never_replays_the_non_idempotent_move() {
        let wire = Wire {
            calls: Cell::new(0),
            lost: true,
        };
        assert!(reorder_tracks(&wire, "127.0.0.1".parse().unwrap(), 2, 3, 8, 17).is_err());
        assert_eq!(wire.calls.get(), 1);
    }

    #[test]
    fn invalid_indices_and_wildcard_revision_are_rejected_before_io() {
        let wire = Wire {
            calls: Cell::new(0),
            lost: false,
        };
        for (first, count, before, revision) in [
            (0, 1, 2, 17),
            (1, 0, 2, 17),
            (1, 1, 0, 17),
            (1, 1, 2, 0),
            (u32::MAX, 2, 1, 17),
        ] {
            assert!(
                reorder_tracks(
                    &wire,
                    "127.0.0.1".parse().unwrap(),
                    first,
                    count,
                    before,
                    revision,
                )
                .is_err()
            );
        }
        assert_eq!(wire.calls.get(), 0);
    }
}
