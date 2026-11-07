use super::*;

mod model;

use model::{Effect, Model, coordinator};

#[test]
fn five_thousand_items_and_twelve_movements_need_only_one_full_snapshot() {
    let transport = Model::new(5_000);
    let mut batch = Batch::new(&coordinator());
    for index in 0..12 {
        let position = 5_001 + index;
        assert_eq!(
            transport
                .add(&mut batch, TRACK, 0, Effect::Healthy)
                .unwrap(),
            (position, Some(position))
        );
    }
    assert_eq!(transport.writes.get(), 12);
    assert_eq!(transport.pages.get(), 50);
    assert_eq!(
        transport.browses.get(),
        86,
        "50 full pages plus three slot reads per movement"
    );
    assert_eq!(
        transport.reads.get(),
        136,
        "including all topology and source fences"
    );
    assert_eq!(transport.state.borrow().uris.len(), 5_012);
    // The previous path read the entire growing queue before each write:
    // 50 pages for the first movement, then 51 pages for each of eleven.
    assert_eq!(
        (0usize..12)
            .map(|index| (5_000 + index).div_ceil(100))
            .sum::<usize>(),
        611
    );
}

#[test]
fn proof_scope_ends_with_each_batch() {
    let transport = Model::new(201);
    for _ in 0..2 {
        transport
            .add(&mut Batch::new(&coordinator()), TRACK, 0, Effect::Healthy)
            .unwrap();
    }
    assert_eq!(
        transport.pages.get(),
        6,
        "each batch establishes its own three-page baseline"
    );
    assert_eq!(transport.writes.get(), 2);
}

#[test]
fn missing_boot_or_generation_retains_full_readback_for_every_add() {
    for unknown_boot in [true, false] {
        let transport = Model::new(201);
        if unknown_boot {
            transport.state.borrow_mut().boot = None;
        } else {
            transport.state.borrow_mut().generation = None;
        }
        let mut batch = Batch::new(&coordinator());
        for _ in 0..3 {
            transport
                .add(&mut batch, TRACK, 0, Effect::Healthy)
                .unwrap();
            assert!(batch.queue.is_none());
        }
        assert_eq!(
            transport.pages.get(),
            18,
            "a full before/after read for every add"
        );
        assert_eq!(transport.browses.get(), 24);
    }
}

#[test]
fn lost_and_inconsistent_replies_reconcile_the_warm_batch_without_replay() {
    for effect in [Effect::Lost, Effect::WrongAcknowledgement] {
        let transport = Model::new(201);
        let mut batch = Batch::new(&coordinator());
        transport
            .add(&mut batch, TRACK, 0, Effect::Healthy)
            .unwrap();
        assert_eq!(
            transport.add(&mut batch, TRACK, 0, effect).unwrap(),
            (203, Some(203))
        );
        transport
            .add(&mut batch, TRACK, 0, Effect::Healthy)
            .unwrap();
        assert_eq!(transport.writes.get(), 3);
        assert_eq!(
            transport.pages.get(),
            6,
            "only the uncertain reply forces another full read"
        );
        assert_eq!(transport.state.borrow().uris[201..], [TRACK, TRACK, TRACK]);
    }
}

#[test]
fn revision_jumps_and_wraps_fall_back_to_the_complete_exact_sequence() {
    for wrapping in [true, false] {
        let transport = Model::new(201);
        let mut batch = Batch::new(&coordinator());
        if wrapping {
            transport.state.borrow_mut().revision = u32::MAX;
        }
        let effect = if wrapping {
            Effect::Healthy
        } else {
            Effect::SkipRevision
        };
        assert_eq!(
            transport.add(&mut batch, TRACK, 0, effect).unwrap(),
            (202, Some(202))
        );
        assert_eq!(
            transport.pages.get(),
            6,
            "a jump or wrap never qualifies for slot proof"
        );
        assert_eq!(transport.writes.get(), 1);
    }
}

#[test]
fn an_external_edit_between_movements_is_detected_before_another_write() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    {
        let mut state = transport.state.borrow_mut();
        state.uris.swap(1, 2);
        state.revision += 1;
    }
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::Healthy),
        Err(FeedError::Proto(_))
    ));
    assert_eq!(transport.writes.get(), 1);
    assert!(
        batch.queue.is_none(),
        "a failed preflight invalidates retained evidence"
    );
}

#[test]
fn changed_prefix_after_send_is_uncertain_even_when_the_new_slot_matches() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::PrefixEdit),
        Err(FeedError::UncertainEnqueue(_))
    ));
    assert_eq!(transport.writes.get(), 2);
    assert_eq!(transport.pages.get(), 6);
    assert!(batch.queue.is_none());
}

#[test]
fn reboot_source_and_membership_changes_after_send_are_uncertain() {
    for effect in [Effect::Reboot, Effect::SourceChange, Effect::GroupChange] {
        let transport = Model::new(201);
        let mut batch = Batch::new(&coordinator());
        transport
            .add(&mut batch, TRACK, 0, Effect::Healthy)
            .unwrap();
        assert!(matches!(
            transport.add(&mut batch, TRACK, 0, effect),
            Err(FeedError::UncertainEnqueue(_))
        ));
        assert_eq!(transport.writes.get(), 2);
        assert!(batch.queue.is_none());
    }
}

#[test]
fn a_reboot_before_the_next_add_sends_nothing() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    transport.state.borrow_mut().boot = Some(4);
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::Healthy),
        Err(FeedError::Proto(_))
    ));
    assert_eq!(transport.writes.get(), 1);
    assert!(batch.queue.is_none());
}

#[test]
fn an_explicit_refusal_does_not_claim_an_identical_external_insertion() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::RejectWithExternalInsertion),
        Err(FeedError::UncertainEnqueue(_))
    ));
    assert_eq!(transport.writes.get(), 2);
    assert!(batch.queue.is_none());
}

#[test]
fn a_proven_unchanged_refusal_remains_retryable_and_discards_the_old_cache() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::Reject),
        Err(FeedError::Proto(ProtoError::SoapFault { code: 501, .. }))
    ));
    assert!(batch.queue.is_none());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert_eq!(
        transport.writes.get(),
        3,
        "a rejected write is retried only by a new caller"
    );
    assert_eq!(transport.pages.get(), 9);
    assert_eq!(transport.state.borrow().uris.len(), 203);
}

#[test]
fn unreadable_post_send_state_never_replays_or_keeps_a_reusable_proof() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::Unreadable),
        Err(FeedError::UncertainEnqueue(_))
    ));
    assert_eq!(transport.writes.get(), 2);
    assert!(batch.queue.is_none());
}

#[test]
fn duplicate_middle_insertion_preserves_both_neighbor_sequences() {
    let transport = Model::new(3);
    transport.state.borrow_mut().uris = vec![TRACK.into(), OWNER.into(), TRACK.into()];
    let mut batch = Batch::new(&coordinator());
    assert_eq!(
        transport
            .add(&mut batch, TRACK, 2, Effect::Healthy)
            .unwrap(),
        (2, Some(4))
    );
    assert_eq!(
        transport.add(&mut batch, TRACK, 3, Effect::Lost).unwrap(),
        (3, Some(5))
    );
    assert_eq!(
        transport.state.borrow().uris,
        [TRACK, TRACK, TRACK, OWNER, TRACK]
    );
    assert_eq!(transport.writes.get(), 2);
}

#[test]
fn real_s1_and_s2_batches_recover_a_lost_second_reply_without_duplicates() {
    for generation in [1, 2] {
        let (sim, _, coordinator) = fixture(generation);
        let target = sim.player("Den").unwrap().ip;
        let params = sim.render_params(generation).unwrap();
        let transport = Replies::new(&sim, Fault::LostReply);
        transport.fault.set(None);
        let mut batch = Batch::new(&coordinator);
        add_movement(&transport, target, &params, &movement(), 0, &mut batch).unwrap();
        assert!(
            batch.queue.is_some(),
            "the simulator reports boot and generation"
        );
        transport.fault.set(Some(Fault::LostReply));
        assert_eq!(
            add_movement(&transport, target, &params, &movement(), 0, &mut batch).unwrap(),
            (2, Some(2))
        );
        add_movement(&transport, target, &params, &movement(), 0, &mut batch).unwrap();
        assert_eq!(transport.adds.get(), 3);
        assert_eq!(
            read(&sim.lan(), target).unwrap().uris,
            [TRACK, TRACK, TRACK]
        );
    }
}

#[test]
fn real_reboot_invalidates_a_warm_batch_before_any_new_add() {
    let (sim, _, coordinator) = fixture(2);
    let target = sim.player("Den").unwrap().ip;
    let params = sim.render_params(2).unwrap();
    let transport = Replies::new(&sim, Fault::LostReply);
    transport.fault.set(None);
    let mut batch = Batch::new(&coordinator);
    add_movement(&transport, target, &params, &movement(), 0, &mut batch).unwrap();
    sim.reboot("Den", std::time::Duration::ZERO).unwrap();
    assert!(matches!(
        add_movement(&transport, target, &params, &movement(), 0, &mut batch),
        Err(FeedError::Proto(_))
    ));
    assert_eq!(transport.adds.get(), 1);
    assert_eq!(read(&sim.lan(), target).unwrap().uris, [TRACK]);
}

#[test]
fn an_edit_after_slot_confirmation_is_caught_by_the_final_revision_fence() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::BeforeFinalFence),
        Err(FeedError::UncertainEnqueue(_))
    ));
    assert_eq!(transport.writes.get(), 2);
    assert!(batch.queue.is_none());
}

#[test]
fn a_transient_slot_read_failure_recovers_without_replaying_the_add() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert_eq!(
        transport
            .add(&mut batch, TRACK, 0, Effect::TransientReadFailure)
            .unwrap(),
        (203, Some(203))
    );
    assert_eq!(transport.pages.get(), 6);
    assert_eq!(transport.browses.get(), 12);
    assert_eq!(transport.writes.get(), 2);
    assert!(batch.queue.is_some());
}

#[test]
fn explicitly_empty_source_is_valid_but_an_absent_source_prevents_the_add() {
    let transport = Model::new(201);
    transport.state.borrow_mut().source.clear();
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert!(batch.queue.is_some());

    let transport = Model::new(201);
    transport.omit_source.set(true);
    let mut batch = Batch::new(&coordinator());
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::Healthy),
        Err(FeedError::Proto(ProtoError::Malformed(_)))
    ));
    assert_eq!(transport.writes.get(), 0);
    assert!(batch.queue.is_none());
}

#[test]
fn missing_source_evidence_after_send_is_uncertain_without_replay() {
    let transport = Model::new(201);
    let mut batch = Batch::new(&coordinator());
    transport
        .add(&mut batch, TRACK, 0, Effect::Healthy)
        .unwrap();
    assert!(matches!(
        transport.add(&mut batch, TRACK, 0, Effect::MissingSource),
        Err(FeedError::UncertainEnqueue(_))
    ));
    assert_eq!(transport.writes.get(), 2);
    assert!(batch.queue.is_none());
}

#[test]
fn readiness_checks_capacity_without_a_write_or_retained_batch_proof() {
    let transport = Model::new(5);
    let before = transport.state.borrow().uris.clone();
    let remaining = usize::try_from(MAX_BROWSE_OBJECTS).unwrap() - before.len();
    ready(&transport, model::host(), &coordinator(), remaining).unwrap();
    for additional in [remaining + 1, usize::MAX] {
        let error = ready(&transport, model::host(), &coordinator(), additional).unwrap_err();
        assert!(error.to_string().contains("verification limit"), "{error}");
    }
    assert_eq!(transport.writes.get(), 0);
    assert_eq!(transport.state.borrow().uris, before);
}

#[test]
fn readiness_rejects_missing_source_and_unidentifiable_queue_entries_without_writes() {
    let transport = Model::new(5);
    transport.omit_source.set(true);
    assert!(ready(&transport, model::host(), &coordinator(), 2).is_err());
    transport.omit_source.set(false);
    transport.state.borrow_mut().uris[2] = "NOT_IMPLEMENTED".into();
    let error = ready(&transport, model::host(), &coordinator(), 2).unwrap_err();
    assert!(error.to_string().contains("no identity"), "{error}");
    assert_eq!(transport.writes.get(), 0);
}
