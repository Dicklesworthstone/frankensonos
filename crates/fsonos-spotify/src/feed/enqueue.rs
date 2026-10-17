//! Reconcile a lost AddURIToQueue reply without sending the add a second time.
//!
//! Capture a coherent queue before the mutation. On a failed or malformed
//! reply, another coherent read must show exactly the intended insertion or
//! an explicit rejection with an unchanged original revision. Anything else
//! is indeterminate: callers relinquish automatic feeding, leaving the
//! speaker's music alone.

use super::FeedError;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

#[derive(Debug, PartialEq, Eq)]
struct Queue {
    revision: u32,
    uris: Vec<String>,
}

fn read<T: Transport + ?Sized>(transport: &T, host: IpAddr) -> Result<Queue, FeedError> {
    let snapshot = browse_snapshot(transport, host, QUEUE)?;
    let uris = snapshot
        .objects
        .into_iter()
        .map(|object| {
            let uri = object
                .res
                .map(|res| res.uri)
                .filter(|uri| !uri.is_empty())
                .ok_or_else(|| {
                    ProtoError::Malformed("queue item has no identity for enqueue recovery".into())
                })?;
            Ok(spotify_uri_from_renderer_uri(&uri).unwrap_or(uri))
        })
        .collect::<Result<Vec<_>, ProtoError>>()?;
    Ok(Queue {
        revision: snapshot.update_id,
        uris,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    Inserted,
    Unchanged,
    Indeterminate,
}

fn observe(before: &Queue, after: &Queue, index: usize, uri: &str) -> Observed {
    if before == after {
        return Observed::Unchanged;
    }
    if before.revision != after.revision
        && index <= before.uris.len()
        && before.uris.len().checked_add(1) == Some(after.uris.len())
        && after.uris.get(index).is_some_and(|found| found == uri)
        && before.uris[..index] == after.uris[..index]
        && before.uris[index..] == after.uris[index + 1..]
    {
        return Observed::Inserted;
    }
    Observed::Indeterminate
}

/// `send` is invoked at most once. A preflight read failure sends nothing;
/// recovery retries only reads, never the non-idempotent queue mutation.
/// One full queue read per movement is deliberate: a count or matching URI
/// alone cannot distinguish an accepted insertion from an existing copy.
pub(super) fn confirmed<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    uri: &str,
    desired: u32,
    send: impl FnOnce() -> Result<(u32, Option<u32>), FeedError>,
) -> Result<(u32, Option<u32>), FeedError> {
    let before = read(transport, host)?;
    let index = if desired == 0 {
        before.uris.len()
    } else {
        usize::try_from(desired - 1)
            .map_err(|_| ProtoError::Malformed("enqueue position does not fit usize".into()))?
    };
    if index > before.uris.len() || uri.is_empty() {
        return Err(
            ProtoError::Malformed("invalid position or empty enqueue identity".into()).into(),
        );
    }
    // Validate these before sending, not after an acknowledged mutation.
    let position = u32::try_from(index + 1)
        .map_err(|_| ProtoError::Malformed("enqueue position exceeds u32".into()))?;
    let length = u32::try_from(before.uris.len() + 1)
        .map_err(|_| ProtoError::Malformed("queue length exceeds u32".into()))?;
    let failure = match send() {
        Ok(accepted) => return Ok(accepted),
        Err(error) => error,
    };
    let after = read(transport, host).map_err(|error| {
        FeedError::UncertainEnqueue(format!(
            "{failure}; queue read-back failed: {error}; the add was not replayed"
        ))
    })?;
    match observe(&before, &after, index, uri) {
        Observed::Inserted => {
            tracing::warn!(cause = %failure, "enqueue confirmed by read-back; add not replayed");
            Ok((position, Some(length)))
        }
        Observed::Unchanged
            if matches!(&failure, FeedError::Proto(ProtoError::SoapFault { .. })) =>
        {
            Err(failure)
        }
        // A timeout does not prove the original request has stopped executing.
        // Even an unchanged read cannot authorize sending that request again.
        Observed::Unchanged | Observed::Indeterminate => Err(FeedError::UncertainEnqueue(format!(
            "{failure}; queue reads did not confirm exactly the intended insertion; \
             the add was not replayed"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dj::{DjConfig, WorkPool};
    use crate::feed::{Movement, Planning, QueueFeed, Speakers, add_movement};
    use crate::test_shelf::{MIDNIGHT, shelf_items, works_of};
    use fsonos_core::HouseholdState;
    use fsonos_core::playback::PlayerPlayback;
    use fsonos_core::store::MemStore;
    use fsonos_proto::control;
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
    use fsonos_types::{PlayerId, TransportState};
    use std::cell::Cell;

    const TRACK: &str = "spotify:track:0000000000000000000001";
    const OWNER: &str = "x-file-cifs://example.invalid/owner.flac";

    fn queue(revision: u32, uris: &[&str]) -> Queue {
        Queue {
            revision,
            uris: uris.iter().map(|uri| (*uri).to_string()).collect(),
        }
    }

    #[test]
    fn exact_append_or_middle_insertion_confirms_one_effect() {
        let before = queue(7, &["a", "b"]);
        assert_eq!(
            observe(&before, &queue(8, &["a", "b", "x"]), 2, "x"),
            Observed::Inserted
        );
        assert_eq!(
            observe(&before, &queue(8, &["a", "x", "b"]), 1, "x"),
            Observed::Inserted
        );
        assert_eq!(
            observe(&queue(7, &[]), &queue(8, &["x"]), 0, "x"),
            Observed::Inserted
        );
    }

    #[test]
    fn existing_duplicate_is_not_mistaken_for_a_new_insertion() {
        let before = queue(7, &["x"]);
        assert_eq!(
            observe(&before, &queue(7, &["x"]), 1, "x"),
            Observed::Unchanged
        );
        assert_eq!(
            observe(&before, &queue(8, &["x", "x"]), 1, "x"),
            Observed::Inserted
        );
    }

    #[test]
    fn an_edit_then_removal_is_not_evidence_that_no_write_happened() {
        assert_eq!(
            observe(&queue(7, &["a"]), &queue(9, &["a"]), 1, "x"),
            Observed::Indeterminate
        );
    }

    #[test]
    fn wrong_slot_extra_items_or_changed_prefix_are_indeterminate() {
        let before = queue(7, &["a", "b"]);
        for after in [
            queue(8, &["x", "a", "b"]),
            queue(8, &["a", "b", "x", "x"]),
            queue(8, &["z", "b", "x"]),
            queue(7, &["a", "b", "x"]),
        ] {
            assert_eq!(observe(&before, &after, 2, "x"), Observed::Indeterminate);
        }
    }

    #[test]
    fn revision_wrap_still_requires_the_exact_inserted_sequence() {
        assert_eq!(
            observe(&queue(u32::MAX, &["a"]), &queue(0, &["a", "x"]), 1, "x"),
            Observed::Inserted
        );
    }

    #[derive(Clone, Copy)]
    enum Fault {
        Rejected,
        BeforeAdd,
        LostReply,
        MalformedReply,
        InvalidCounters,
        LostReplyAndRead,
        ConcurrentEdit,
    }

    struct Replies {
        inner: SimLan,
        fault: Cell<Option<Fault>>,
        adds: Cell<usize>,
        unreadable: Cell<bool>,
    }

    impl Replies {
        fn new(sim: &SimHandle, fault: Fault) -> Self {
            Self {
                inner: sim.lan(),
                fault: Cell::new(Some(fault)),
                adds: Cell::new(0),
                unreadable: Cell::new(false),
            }
        }

        fn lost(host: IpAddr) -> ProtoError {
            ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost response".into(),
            }
        }
    }

    impl Transport for Replies {
        fn soap_post(
            &self,
            host: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            if action.contains("#Browse") && body.contains("Q:0") && self.unreadable.get() {
                return Err(Self::lost(host));
            }
            if !action.contains("#AddURIToQueue") {
                return self.inner.soap_post(host, path, action, body);
            }
            self.adds.set(self.adds.get() + 1);
            let fault = self.fault.take();
            if matches!(fault, Some(Fault::BeforeAdd)) {
                return Err(Self::lost(host));
            }
            if matches!(fault, Some(Fault::Rejected)) {
                return Err(ProtoError::SoapFault {
                    code: 501,
                    reason: "injected explicit rejection".into(),
                });
            }
            let response = self.inner.soap_post(host, path, action, body)?;
            match fault {
                Some(Fault::LostReply) => Err(Self::lost(host)),
                Some(Fault::MalformedReply) => Ok("<broken-reply>".into()),
                Some(Fault::InvalidCounters) => {
                    assert!(response.contains("<NumTracksAdded>1</NumTracksAdded>"));
                    Ok(response.replace(
                        "<NumTracksAdded>1</NumTracksAdded>",
                        "<NumTracksAdded>2</NumTracksAdded>",
                    ))
                }
                Some(Fault::LostReplyAndRead) => {
                    self.unreadable.set(true);
                    Err(Self::lost(host))
                }
                Some(Fault::ConcurrentEdit) => {
                    control::add_uri_to_queue(&self.inner, host, OWNER, "", false)?;
                    Err(Self::lost(host))
                }
                None => Ok(response),
                Some(Fault::BeforeAdd | Fault::Rejected) => {
                    unreachable!("returned before the request")
                }
            }
        }
    }

    fn fixture(generation: u8) -> (SimHandle, Vec<HouseholdState>, PlayerId) {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
        };
        let sim = builder.spawn().unwrap();
        let player = sim.player("Den").unwrap();
        let coordinator = PlayerId(player.uuid.clone());
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
        (sim, vec![household], coordinator)
    }

    fn speakers<'a, T: Transport + ?Sized>(
        transport: &'a T,
        households: &'a [HouseholdState],
        coordinator: &'a PlayerId,
    ) -> Speakers<'a, T> {
        Speakers {
            transport,
            households,
            coordinator,
        }
    }

    fn planning(pool: &WorkPool) -> Planning<'_> {
        Planning {
            pool,
            steer: None,
            feedback: None,
            now: MIDNIGHT,
            local_hour: Some(20),
            queued_elsewhere: &[],
        }
    }

    fn movement() -> Movement {
        Movement {
            uri: TRACK.into(),
            title: "Synthetic movement".into(),
            position: None,
            added: false,
        }
    }

    #[test]
    fn lost_and_malformed_replies_are_recovered_over_s1_and_s2_sockets() {
        for generation in [1, 2] {
            for fault in [
                Fault::LostReply,
                Fault::MalformedReply,
                Fault::InvalidCounters,
            ] {
                let (sim, _, _) = fixture(generation);
                let host = sim.player("Den").unwrap().ip;
                let params = sim.render_params(generation).unwrap();
                let transport = Replies::new(&sim, fault);
                assert_eq!(
                    add_movement(&transport, host, &params, &movement(), 0).unwrap(),
                    (1, Some(1))
                );
                assert_eq!(transport.adds.get(), 1, "the mutation must not be repeated");
                assert_eq!(read(&sim.lan(), host).unwrap().uris, [TRACK]);
            }
        }
    }

    #[test]
    fn lost_reply_during_insertion_keeps_neighboring_owner_items() {
        let (sim, _, _) = fixture(2);
        let host = sim.player("Den").unwrap().ip;
        control::add_uri_to_queue(&sim.lan(), host, OWNER, "", false).unwrap();
        let transport = Replies::new(&sim, Fault::LostReply);
        let params = sim.render_params(2).unwrap();
        assert_eq!(
            add_movement(&transport, host, &params, &movement(), 1).unwrap(),
            (1, Some(2))
        );
        assert_eq!(read(&sim.lan(), host).unwrap().uris, [TRACK, OWNER]);
        assert_eq!(transport.adds.get(), 1);
    }

    #[test]
    fn an_explicit_rejection_remains_retryable_without_a_duplicate() {
        let (sim, _, _) = fixture(2);
        let host = sim.player("Den").unwrap().ip;
        let transport = Replies::new(&sim, Fault::Rejected);
        let params = sim.render_params(2).unwrap();
        assert!(matches!(
            add_movement(&transport, host, &params, &movement(), 0),
            Err(FeedError::Proto(ProtoError::SoapFault { code: 501, .. }))
        ));
        assert!(read(&sim.lan(), host).unwrap().uris.is_empty());
        assert_eq!(
            add_movement(&transport, host, &params, &movement(), 0).unwrap(),
            (1, Some(1))
        );
        assert_eq!(read(&sim.lan(), host).unwrap().uris, [TRACK]);
    }

    #[test]
    fn an_unchanged_queue_after_a_timeout_does_not_authorize_a_repeat() {
        let (sim, _, _) = fixture(2);
        let host = sim.player("Den").unwrap().ip;
        let transport = Replies::new(&sim, Fault::BeforeAdd);
        let result = add_movement(
            &transport,
            host,
            &sim.render_params(2).unwrap(),
            &movement(),
            0,
        );
        assert!(matches!(result, Err(FeedError::UncertainEnqueue(_))));
        assert_eq!(transport.adds.get(), 1);
        assert!(read(&sim.lan(), host).unwrap().uris.is_empty());
    }

    #[test]
    fn failed_preflight_never_sends_the_mutation() {
        let (sim, _, _) = fixture(2);
        let host = sim.player("Den").unwrap().ip;
        let transport = Replies::new(&sim, Fault::LostReply);
        transport.unreadable.set(true);
        let result = add_movement(
            &transport,
            host,
            &sim.render_params(2).unwrap(),
            &movement(),
            0,
        );
        assert!(matches!(result, Err(FeedError::Proto(_))));
        assert_eq!(transport.adds.get(), 0);
    }

    #[test]
    fn the_full_start_finishes_after_losing_its_first_enqueue_reply() {
        let (sim, households, coordinator) = fixture(2);
        let transport = Replies::new(&sim, Fault::LostReply);
        let speakers = speakers(&transport, &households, &coordinator);
        let pool = works_of(&shelf_items(1));
        let planning = planning(&pool);
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&coordinator, DjConfig::default(), 7);
        feed.start(&speakers, planning, &mut store).unwrap();
        let tracked: Vec<String> = feed
            .queued()
            .iter()
            .flat_map(|work| work.movements.iter().map(|m| m.uri.clone()))
            .collect();
        let host = sim.player("Den").unwrap().ip;
        assert_eq!(read(&sim.lan(), host).unwrap().uris, tracked);
        assert_eq!(transport.adds.get(), tracked.len());
        assert_eq!(
            control::get_transport_info(&sim.lan(), host).unwrap().state,
            TransportState::Playing
        );
    }

    #[test]
    fn an_indeterminate_top_up_stops_feeding_without_stopping_the_music() {
        for fault in [
            Fault::LostReplyAndRead,
            Fault::ConcurrentEdit,
            Fault::BeforeAdd,
        ] {
            let (sim, households, coordinator) = fixture(2);
            let lan = sim.lan();
            let at = speakers(&lan, &households, &coordinator);
            let pool = works_of(&shelf_items(1));
            let planning = planning(&pool);
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&coordinator, DjConfig::default(), 7);
            feed.start(&at, planning, &mut store).unwrap();
            let transport = Replies::new(&sim, fault);
            let speakers = speakers(&transport, &households, &coordinator);
            let result = feed.enqueue(&speakers, planning, &mut store, 1);
            assert!(matches!(result, Err(FeedError::UncertainEnqueue(_))));
            assert!(!feed.is_active());
            assert!(
                feed.snapshot().is_none(),
                "a relinquished feed is not included in subsequent saved snapshots"
            );
            let before = sim.soap_log().len();
            assert_eq!(
                feed.on_playback(&speakers, planning, &mut store, &PlayerPlayback::default())
                    .unwrap(),
                0
            );
            assert_eq!(
                sim.soap_log().len(),
                before,
                "no automatic retry or rollback"
            );
            assert_eq!(
                control::get_transport_info(&lan, sim.player("Den").unwrap().ip)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
        }
    }
}
