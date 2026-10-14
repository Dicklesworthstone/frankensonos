use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use fastmcp_core::ingress::{SecurityPartitionDescriptor, VerifiedAudienceBinding, VerifiedIdentityFacts, VerifiedIngressAuthentication};
use fastmcp_core::partition::DurableOwnerKey;
use fastmcp_protocol::{ClientCapabilities, FinalRequestMeta, RequestId};
use serde_json::json;

// Explicit verified-provider fixture, not a claim of network authentication.
fn authority(subject: &str, revision: &str, owner: &McpRequestCancellation) -> ContinuationReplayAuthority {
    let ingress = VerifiedIngressAuthentication::from_verified_provider_output(VerifiedIdentityFacts {
        provider: "fixture-provider", configuration_generation: 7,
        issuer: "https://issuer.example", canonical_resource: "https://mcp.example/mcp",
        verified_audience_binding: VerifiedAudienceBinding::OAuth {
            canonical_resource: "https://mcp.example/mcp".to_owned(), validated_audience: "https://mcp.example/mcp".to_owned(),
            audience_policy_id: "fixture-policy".to_owned(), audience_policy_revision: 3,
            provider: "fixture-provider".to_owned(), configuration_generation: 7,
        },
        tenant: "tenant", subject_or_principal: subject, authorized_party_or_client: "client",
        verified_claims: &[], auth_policy_revision: 4, trust_generation: 2,
    }).unwrap();
    let descriptor = SecurityPartitionDescriptor::from_verified_ingress(&ingress).to_partition_descriptor().unwrap();
    let key = ContinuationPartitionKey::derive(&descriptor, &["tools:call"], revision,
        "capabilities", "continuation-policy", "deployment").unwrap();
    let durable_owner = DurableOwnerKey::derive(&descriptor, 1).unwrap();
    ContinuationReplayAuthority::new(key, PartitionAuthorization::current(&descriptor, &durable_owner), owner.clone())
}
fn ctx() -> McpContext { McpContext::new(Cx::for_testing(), 1) }
fn request(state: &str) -> JsonRpcRequest {
    JsonRpcRequest::new("tools/call", Some(json!({
        "_meta": FinalRequestMeta::new(ClientCapabilities::default()),
        "name":"checkout", "arguments":{"quantity":1}, "requestState":state, "inputResponses":{}
    })), 1i64)
}
fn complete() -> Value {
    serde_json::from_str(r#"{"resultType":"complete","content":[{"type":"text","text":"private-result"}],"x-exact":900719925474099312345}"#).unwrap()
}
fn middleware(limits: ContinuationReplayLimits, owner: &McpRequestCancellation) -> ContinuationReplayMiddleware {
    let owner = owner.clone();
    ContinuationReplayMiddleware::new(&Cx::for_testing(), ProcessGenerationGuard::install().unwrap(),
        SnapshotCloneStance::NoLiveMemoryCloning, limits, move |_, _| Ok(authority("alice", "handler-v1", &owner))).unwrap()
}
fn finish(mw: &ContinuationReplayMiddleware, request: &JsonRpcRequest, value: Value) {
    assert!(matches!(mw.on_request(&ctx(), request).unwrap(), MiddlewareDecision::Continue));
    assert_eq!(mw.on_response(&ctx(), request, value.clone()).unwrap(), value);
}
fn replay(mw: &ContinuationReplayMiddleware, request: &JsonRpcRequest) -> Value {
    let MiddlewareDecision::Respond(value) = mw.on_request(&ctx(), request).unwrap() else { panic!("expected terminal replay"); };
    mw.on_response(&ctx(), request, value).unwrap()
}

#[test]
fn exact_retry_with_new_rpc_id_replays_without_resetting_expiry_or_resealing() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    let original = request("opaque-one");
    finish(&mw, &original, complete());
    let (before, deadline, ciphertext) = {
        let state = mw.journal.lock().unwrap();
        let entry = state.entries.values().next().unwrap();
        (state.retained_bytes, entry.expires_at, entry.result.clone())
    };
    let mut retry = original.clone();
    retry.id = Some(RequestId::String("retry-2".to_owned()));
    assert_eq!(replay(&mw, &retry), complete());
    assert!(serde_json::to_string(&replay(&mw, &retry)).unwrap().contains("900719925474099312345"));
    let state = mw.journal.lock().unwrap();
    let entry = state.entries.values().next().unwrap();
    assert_eq!((state.retained_bytes, entry.expires_at, &entry.result), (before, deadline, &ciphertext));
    assert!(!entry.result.as_ref().unwrap().windows(b"private-result".len()).any(|w| w == b"private-result"));
}

#[test]
fn initial_empty_and_legacy_requests_never_enter_the_reply_journal() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    for case in 0..4 {
        let mut req = request("opaque");
        match case {
            0 => { req.params.as_mut().unwrap().as_object_mut().unwrap().remove("requestState"); },
            1 => req.params.as_mut().unwrap()["requestState"] = json!(""),
            2 => { req.params.as_mut().unwrap().as_object_mut().unwrap().remove("_meta"); },
            _ => req.method = "tasks/update".to_owned(),
        }
        for _ in 0..2 { assert!(matches!(mw.on_request(&ctx(), &req).unwrap(), MiddlewareDecision::Continue)); }
        assert_eq!(mw.on_response(&ctx(), &req, complete()).unwrap(), complete());
    }
    assert!(mw.journal.lock().unwrap().entries.is_empty());
}

#[test]
fn changed_answers_arguments_or_metadata_cannot_replay_or_consume_the_original() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    let req = request("opaque");
    finish(&mw, &req, complete());
    for case in 0..3 {
        let mut changed = req.clone();
        let params = changed.params.as_mut().unwrap();
        match case {
            0 => params["arguments"]["quantity"] = json!(2),
            1 => params["inputResponses"] = json!({"roots":{"roots":[]}}),
            _ => params["_meta"]["com.example/tenant"] = json!("different"),
        }
        assert!(mw.on_request(&ctx(), &changed).is_err());
        assert_eq!(replay(&mw, &req), complete());
    }
}

#[test]
fn duplicate_error_and_auth_failure_cannot_release_an_inflight_reservation() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    let req = request("opaque");
    assert!(matches!(mw.on_request(&ctx(), &req).unwrap(), MiddlewareDecision::Continue));
    let err = mw.on_request(&ctx(), &req).unwrap_err();
    mw.on_error(&ctx(), &req, err);
    mw.on_error(&ctx(), &req, McpError::invalid_params("authentication rejected"));
    assert!(mw.on_request(&ctx(), &req).is_err());
    assert_eq!(mw.on_response(&ctx(), &req, complete()).unwrap(), complete());
    assert_eq!(replay(&mw, &req), complete());
}

#[test]
fn current_host_authorization_is_checked_again_on_cache_hits() {
    let denied = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = denied.clone();
    let observed = calls.clone();
    let owner = McpRequestCancellation::new();
    let mw = ContinuationReplayMiddleware::new(&Cx::for_testing(), ProcessGenerationGuard::install().unwrap(),
        SnapshotCloneStance::NoLiveMemoryCloning, ContinuationReplayLimits::default(), move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            if captured.load(Ordering::SeqCst) { return Err(unavailable()); }
            Ok(authority("alice", "handler-v1", &owner))
        }).unwrap();
    let req = request("opaque");
    finish(&mw, &req, complete());
    denied.store(true, Ordering::SeqCst);
    assert!(mw.on_request(&ctx(), &req).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    denied.store(false, Ordering::SeqCst);
    assert_eq!(replay(&mw, &req), complete());
}

#[test]
fn different_principal_or_handler_revision_cannot_read_an_existing_reply() {
    for dimension in 0..2 {
        let changed = Arc::new(AtomicBool::new(false));
        let captured = changed.clone();
        let owner = McpRequestCancellation::new();
        let mw = ContinuationReplayMiddleware::new(&Cx::for_testing(), ProcessGenerationGuard::install().unwrap(),
            SnapshotCloneStance::NoLiveMemoryCloning, ContinuationReplayLimits::default(), move |_, _| {
                let changed = captured.load(Ordering::SeqCst);
                Ok(authority(if changed && dimension == 0 {"bob"} else {"alice"},
                    if changed && dimension == 1 {"handler-v2"} else {"handler-v1"}, &owner))
            }).unwrap();
        let req = request("opaque");
        finish(&mw, &req, complete());
        changed.store(true, Ordering::SeqCst);
        // No disclosure: the normal MRTR registry must adjudicate this miss.
        assert!(matches!(mw.on_request(&ctx(), &req).unwrap(), MiddlewareDecision::Continue));
        changed.store(false, Ordering::SeqCst);
        assert_eq!(replay(&mw, &req), complete());
    }
}

#[test]
fn revocation_expiry_and_key_rotation_preserve_the_right_lifetimes() {
    let owner = McpRequestCancellation::new();
    let mw = middleware(ContinuationReplayLimits::default(), &owner);
    let req = request("opaque");
    finish(&mw, &req, complete());
    assert_eq!(mw.rotate(&Cx::for_testing()).unwrap(), 2);
    assert_eq!(replay(&mw, &req), complete());
    owner.cancel();
    assert!(mw.on_request(&ctx(), &req).is_err());
    assert_eq!(mw.prune(&Cx::for_testing()).unwrap(), 1);
    assert_eq!(mw.journal.lock().unwrap().retained_bytes, 0);
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    finish(&mw, &req, complete());
    mw.journal.lock().unwrap().entries.values_mut().next().unwrap().expires_at = Instant::now();
    assert!(mw.on_request(&ctx(), &req).is_err());
    assert_eq!(mw.prune(&Cx::for_testing()).unwrap(), 1);
}

#[test]
fn pending_budget_reservation_precedes_dispatch_and_never_evicts_live_replies() {
    let limits = ContinuationReplayLimits::new(1, 4096, 1024, 1024, Duration::from_secs(60)).unwrap();
    let mw = middleware(limits, &McpRequestCancellation::new());
    let req = request("one");
    assert!(matches!(mw.on_request(&ctx(), &req).unwrap(), MiddlewareDecision::Continue));
    let before = mw.journal.lock().unwrap().retained_bytes;
    assert!(before > limits.result_bytes);
    assert!(mw.on_request(&ctx(), &request("two")).is_err());
    assert_eq!(mw.journal.lock().unwrap().retained_bytes, before);
    mw.on_response(&ctx(), &req, complete()).unwrap();
    assert!(mw.journal.lock().unwrap().retained_bytes < before);
    assert!(mw.on_request(&ctx(), &request("two")).is_err());
    assert_eq!(replay(&mw, &req), complete());
    let tiny = middleware(ContinuationReplayLimits::new(1, 1, 1024, 1024, Duration::from_secs(1)).unwrap(), &McpRequestCancellation::new());
    assert!(tiny.on_request(&ctx(), &req).is_err());
    assert!(tiny.journal.lock().unwrap().entries.is_empty());
}

#[test]
fn uncertain_failed_and_successor_attempts_remain_fenced_without_replay() {
    for successor in [false, true] {
        let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
        let req = request("opaque");
        mw.on_request(&ctx(), &req).unwrap();
        if successor {
            let response = json!({"resultType":"input_required","requestState":"next"});
            assert_eq!(mw.on_response(&ctx(), &req, response.clone()).unwrap(), response);
        } else { mw.on_error(&ctx(), &req, McpError::internal_error("uncertain effect")); }
        mw.journal.lock().unwrap().entries.values_mut().next().unwrap().expires_at = Instant::now();
        assert_eq!(mw.prune(&Cx::for_testing()).unwrap(), 0);
        assert!(mw.on_request(&ctx(), &req).is_err());
    }
}

#[test]
fn swapped_ciphertexts_and_damaged_results_do_not_disclose_plaintext() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    let one = request("one");
    let two = request("two");
    finish(&mw, &one, complete());
    finish(&mw, &two, complete());
    {
        let mut state = mw.journal.lock().unwrap();
        let keys: Vec<_> = state.entries.keys().copied().collect();
        let first = state.entries.get_mut(&keys[0]).unwrap().result.take();
        let second = state.entries.get_mut(&keys[1]).unwrap().result.take();
        state.entries.get_mut(&keys[0]).unwrap().result = second;
        state.entries.get_mut(&keys[1]).unwrap().result = first;
    }
    assert!(mw.on_request(&ctx(), &one).is_err());
    assert!(mw.on_request(&ctx(), &two).is_err());
}

#[test]
fn malformed_oversized_and_overdeep_requests_fail_before_authorization() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mw = ContinuationReplayMiddleware::new(&Cx::for_testing(), ProcessGenerationGuard::install().unwrap(),
        SnapshotCloneStance::NoLiveMemoryCloning, ContinuationReplayLimits::default(), move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst); Err(unavailable())
        }).unwrap();
    for case in 0..4 {
        let mut req = request("opaque");
        let params = req.params.as_mut().unwrap();
        match case {
            0 => params["requestState"] = Value::Null,
            1 => params["arguments"] = json!({"huge":"x".repeat(65536)}),
            2 => {
                let mut deep = Value::Null;
                for _ in 0..66 { deep = json!([deep]); }
                params["arguments"] = deep;
            }
            _ => req.id = None,
        }
        assert!(mw.on_request(&ctx(), &req).is_err());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn malformed_or_oversized_completed_results_leave_the_dispatch_fenced() {
    for invalid in [json!({"resultType":"complete","content":null}),
        json!({"resultType":"complete","content":[{"type":"text","text":"x".repeat(65536)}]})] {
        let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
        let req = request("opaque");
        mw.on_request(&ctx(), &req).unwrap();
        assert!(mw.on_response(&ctx(), &req, invalid).is_err());
        assert!(mw.on_request(&ctx(), &req).is_err());
        assert!(mw.journal.lock().unwrap().entries.values().all(|entry| entry.result.is_none()));
    }
}

#[test]
fn lock_contention_and_close_are_fail_closed_not_waiting_or_replay_permission() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    let req = request("opaque");
    finish(&mw, &req, complete());
    {
        let _held = mw.journal.lock().unwrap();
        assert!(mw.on_request(&ctx(), &req).is_err());
        assert!(mw.close().is_err());
    }
    assert_eq!(replay(&mw, &req), complete());
    mw.close().unwrap();
    assert!(mw.on_request(&ctx(), &req).is_err());
    assert!(mw.journal.lock().unwrap().entries.is_empty());
    assert!(!format!("{mw:?}").contains("opaque"));
}

#[test]
fn resources_and_prompts_replay_through_their_own_method_result_codecs() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    let cases = [
        ("resources/read", json!({"uri":"file:///one"}), json!({
            "resultType":"complete", "contents":[{"uri":"file:///one","text":"resource-value"}],
            "ttlMs":0,"cacheScope":"private"
        })),
        ("prompts/get", json!({"name":"greeting"}), json!({
            "resultType":"complete", "messages":[{"role":"user","content":{"type":"text","text":"prompt-value"}}],
            "ttlMs":0,"cacheScope":"private"
        })),
    ];
    for (method, mut params, expected) in cases {
        params["_meta"] = serde_json::to_value(FinalRequestMeta::new(ClientCapabilities::default())).unwrap();
        params["requestState"] = json!("shared-wire-state");
        params["inputResponses"] = json!({});
        let req = JsonRpcRequest::new(method, Some(params), 1i64);
        finish(&mw, &req, expected.clone());
        assert_eq!(replay(&mw, &req), expected);
    }
    assert_eq!(mw.journal.lock().unwrap().entries.len(), 2, "the same wire state cannot alias across methods");
}

#[test]
fn completed_tool_errors_replay_as_errors_not_successful_empty_results() {
    let mw = middleware(ContinuationReplayLimits::default(), &McpRequestCancellation::new());
    let req = request("error-state");
    let expected = json!({"resultType":"complete","isError":true,
        "content":[{"type":"text","text":"the operation was declined"}]});
    finish(&mw, &req, expected.clone());
    assert_eq!(replay(&mw, &req), expected);
}
