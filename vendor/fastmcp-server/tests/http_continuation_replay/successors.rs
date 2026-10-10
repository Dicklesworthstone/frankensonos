//! Successive partial answers through the public authenticated HTTP embedding.
//! The real router owns the input ledger and one-use handles; this test never
//! injects registry state or mocks continuation admission. Reply loss is modeled
//! by discarding an embedding response, not by simulating a TCP/TLS failure.
use super::*;

fn fixture(cx: &Cx, recover: bool, limits: ContinuationReplayLimits) -> Fixture {
    let mut probe = Probe::new();
    probe.require_roots = true;
    Fixture::with_probe(cx, true, limits, probe, recover)
}
fn answer(mut request: Value, key: &str) -> Value {
    request["inputResponses"] = json!({key:{"roots":[{"uri":format!("file:///{key}")} ]}});
    request
}
fn successor(original: &Value, reply: &Value) -> Value {
    let mut next = original.clone();
    next["requestState"] = reply["requestState"].clone();
    next.as_object_mut().unwrap().remove("inputResponses");
    answer(next, "right")
}
async fn partial(f: &Fixture, cx: &Cx, request: &Value, id: i64) -> Value {
    let (status, body) = f.post(cx, id, &f.probe.alice, request.clone()).await;
    assert_eq!(status, 200);
    assert!(body.get("error").is_none(), "{body}");
    assert_eq!(body["id"], id);
    let result = &body["result"];
    assert_eq!(result["resultType"], "input_required");
    assert_eq!(result["inputRequests"], json!({"right":{"method":"roots/list"}}));
    assert!(result["requestState"].is_string());
    assert_ne!(result["requestState"], request["requestState"]);
    assert_eq!(f.probe.effects(), 0, "partial admission must not invoke the resumed handler");
    result.clone()
}
fn assert_terminal(f: &Fixture, result: &Value) {
    assert_eq!(result["structuredContent"]["accepted"]["left"]["roots"][0]["uri"], "file:///left");
    assert_eq!(result["structuredContent"]["accepted"]["right"]["roots"][0]["uri"], "file:///right");
    assert_eq!(result["structuredContent"]["accepted"]["order"], json!(["left","right"]));
    assert_eq!(f.probe.effects(), 1);
    assert_eq!(f.probe.starts.load(Ordering::SeqCst), 1);
    assert_eq!(f.probe.transforms.load(Ordering::SeqCst), 1);
}

#[test]
fn public_partial_reply_loss_recovers_and_completes_without_repeating_answers() {
    run(|cx| async move {
        let f = fixture(&cx, true, ContinuationReplayLimits::default());
        let first = answer(f.begin(&cx, 1).await, "left");
        let delivered = partial(&f, &cx, &first, 2).await;
        let expected = delivered.clone();
        drop(delivered);
        let recovered = partial(&f, &cx, &first, 3).await;
        assert_eq!(recovered, expected, "the exact successor, not a newly issued handle");
        let next = successor(&first, &recovered);
        let terminal = f.finish(&cx, &next, 4).await;
        assert_terminal(&f, &terminal);
        assert!(rejected(&f.post(&cx, 5, &f.probe.alice, first).await));
        assert_eq!(f.finish(&cx, &next, 6).await, terminal);
        assert_terminal(&f, &terminal);
    });
}

#[test]
fn public_recovered_successor_state_is_still_validated_by_the_router() {
    run(|cx| async move {
        let f = fixture(&cx, true, ContinuationReplayLimits::default());
        let first = answer(f.begin(&cx, 1).await, "left");
        drop(partial(&f, &cx, &first, 2).await);
        let recovered = partial(&f, &cx, &first, 3).await;
        // Replaying the reply grants no authority over its requestState: one
        // changed byte is refused by the router on the next retry.
        let mut state = recovered["requestState"].as_str().unwrap().to_owned();
        let last = state.pop().unwrap();
        state.push(if last == 'A' { 'B' } else { 'A' });
        let mut tampered = recovered.clone();
        tampered["requestState"] = json!(state);
        assert!(rejected(&f.post(&cx, 4, &f.probe.alice, successor(&first, &tampered)).await));
        assert_eq!(f.probe.effects(), 0);
        let terminal = f.finish(&cx, &successor(&first, &recovered), 5).await;
        assert_terminal(&f, &terminal);
    });
}

#[test]
fn public_terminal_only_control_does_not_claim_partial_reply_recovery() {
    run(|cx| async move {
        let f = fixture(&cx, false, ContinuationReplayLimits::default());
        let first = answer(f.begin(&cx, 1).await, "left");
        let delivered = partial(&f, &cx, &first, 2).await;
        assert!(rejected(&f.post(&cx, 3, &f.probe.alice, first.clone()).await));
        // The control kept the original reply only to check the unchanged
        // native continuation path. It cannot recover that reply from a retry.
        let next = successor(&first, &delivered);
        let terminal = f.finish(&cx, &next, 4).await;
        assert_terminal(&f, &terminal);
    });
}

#[test]
fn public_changed_successor_operation_cannot_destroy_the_original_recovery() {
    run(|cx| async move {
        let f = fixture(&cx, true, ContinuationReplayLimits::default());
        let first = answer(f.begin(&cx, 1).await, "left");
        let delivered = partial(&f, &cx, &first, 2).await;
        let next = successor(&first, &delivered);
        for change in 0..2 {
            let mut wrong = next.clone();
            if change == 0 { wrong["arguments"]["quantity"] = json!(2); }
            else { wrong["_meta"]["com.example/context"] = json!("changed"); }
            assert!(rejected(&f.post(&cx, 3 + change, &f.probe.alice, wrong).await));
            assert_eq!(partial(&f, &cx, &first, 5 + change).await, delivered);
        }
        assert_terminal(&f, &f.finish(&cx, &next, 8).await);
    });
}

#[test]
fn public_successor_authentication_and_foreign_principal_attempts_do_not_retire_owner_reply() {
    run(|cx| async move {
        let f = fixture(&cx, true, ContinuationReplayLimits::default());
        let first = answer(f.begin(&cx, 1).await, "left");
        let delivered = partial(&f, &cx, &first, 2).await;
        let next = successor(&first, &delivered);
        let before = f.probe.authentications.load(Ordering::SeqCst);
        assert_eq!(f.post(&cx, 3, "not-a-valid-bearer", next.clone()).await.0, 401);
        assert!(rejected(&f.post(&cx, 4, &f.probe.bob, next.clone()).await));
        assert_eq!(partial(&f, &cx, &first, 5).await, delivered);
        assert!(f.probe.authentications.load(Ordering::SeqCst) >= before + 3);
        assert_terminal(&f, &f.finish(&cx, &next, 6).await);
    });
}

#[test]
fn public_successor_capacity_failure_preserves_reply_and_prevents_handler_effect() {
    run(|cx| async move {
        let limits = ContinuationReplayLimits::new(1, 8192, 2048, 2048, Duration::from_secs(60)).unwrap();
        let f = fixture(&cx, true, limits);
        let first = answer(f.begin(&cx, 1).await, "left");
        let delivered = partial(&f, &cx, &first, 2).await;
        let next = successor(&first, &delivered);
        assert!(rejected(&f.post(&cx, 3, &f.probe.alice, next).await));
        assert_eq!(partial(&f, &cx, &first, 4).await, delivered);
        assert_eq!(f.probe.effects(), 0);
        assert_eq!(f.probe.starts.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn public_invalid_admitted_successor_stays_fenced_instead_of_restarting_earlier_step() {
    run(|cx| async move {
        let f = fixture(&cx, true, ContinuationReplayLimits::default());
        let first = answer(f.begin(&cx, 1).await, "left");
        let delivered = partial(&f, &cx, &first, 2).await;
        let next = successor(&first, &delivered);
        let mut wrong = next.clone();
        wrong["inputResponses"] = json!({"unknown-key":{"roots":[]}});
        // The method codec admits this typed map, then the real registry rejects
        // it because no outstanding input key made progress. The journal does
        // not mistake the error for authority to replay the earlier transition.
        assert!(rejected(&f.post(&cx, 3, &f.probe.alice, wrong).await));
        assert!(rejected(&f.post(&cx, 4, &f.probe.alice, first).await));
        assert!(rejected(&f.post(&cx, 5, &f.probe.alice, next).await));
        assert_eq!(f.probe.effects(), 0);
        assert_eq!(f.journal.prune(&cx).unwrap(), 0);
    });
}

async fn both<L: Future, R: Future>(left: L, right: R) -> (L::Output, R::Output) {
    let mut left = std::pin::pin!(left);
    let mut right = std::pin::pin!(right);
    let (mut a, mut b) = (None, None);
    std::future::poll_fn(|cx| {
        if a.is_none() { if let std::task::Poll::Ready(value) = left.as_mut().poll(cx) { a = Some(value); } }
        if b.is_none() { if let std::task::Poll::Ready(value) = right.as_mut().poll(cx) { b = Some(value); } }
        if a.is_some() && b.is_some() { std::task::Poll::Ready((a.take().unwrap(), b.take().unwrap())) }
        else { std::task::Poll::Pending }
    }).await
}

#[test]
fn public_concurrent_partial_retries_admit_one_transition_and_keep_the_same_successor() {
    run(|cx| async move {
        let f = fixture(&cx, true, ContinuationReplayLimits::default());
        let first = answer(f.begin(&cx, 1).await, "left");
        let (a, b) = both(f.post(&cx, 2, &f.probe.alice, first.clone()),
            f.post(&cx, 3, &f.probe.alice, first.clone())).await;
        let successes: Vec<_> = [&a, &b].into_iter().filter(|reply| !rejected(reply)).collect();
        assert!(!successes.is_empty());
        let recovered = partial(&f, &cx, &first, 4).await;
        for reply in successes { assert_eq!(reply.1["result"], recovered); }
        let next = successor(&first, &recovered);
        assert_terminal(&f, &f.finish(&cx, &next, 5).await);
    });
}

#[test]
fn public_state_only_retry_distinguishes_absent_from_empty_input_responses() {
    run(|cx| async move {
        let f = Fixture::new(&cx, false, ContinuationReplayLimits::default());
        let valid = f.begin(&cx, 1).await;
        assert!(valid.get("inputResponses").is_none());
        let mut empty = valid.clone();
        empty["inputResponses"] = json!({});
        assert!(rejected(&f.post(&cx, 2, &f.probe.alice, empty).await));
        assert_eq!(f.probe.effects(), 0);
        f.finish(&cx, &valid, 3).await;
        assert_eq!(f.probe.effects(), 1);
    });
}
