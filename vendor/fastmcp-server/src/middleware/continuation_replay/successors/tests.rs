use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use fastmcp_core::{McpError, McpRequestCancellation};
use fastmcp_core::ingress::{SecurityPartitionDescriptor, VerifiedAudienceBinding, VerifiedIdentityFacts, VerifiedIngressAuthentication};
use fastmcp_core::partition::{ContinuationPartitionKey, DurableOwnerKey, PartitionAuthorization};
use fastmcp_protocol::{FINAL_PROTOCOL_VERSION, RequestId};
use crate::middleware::{Middleware, MiddlewareDecision};
use serde_json::json;

fn authority(subject: &str, owner: &McpRequestCancellation) -> ContinuationReplayAuthority {
    let ingress = VerifiedIngressAuthentication::from_verified_provider_output(VerifiedIdentityFacts {
        provider: "fixture", configuration_generation: 1,
        issuer: "https://issuer.example", canonical_resource: "https://mcp.example/mcp",
        verified_audience_binding: VerifiedAudienceBinding::OAuth {
            canonical_resource: "https://mcp.example/mcp".to_owned(), validated_audience: "https://mcp.example/mcp".to_owned(),
            audience_policy_id: "fixture".to_owned(), audience_policy_revision: 1,
            provider: "fixture".to_owned(), configuration_generation: 1,
        },
        tenant: "tenant", subject_or_principal: subject, authorized_party_or_client: "client",
        verified_claims: &[], auth_policy_revision: 1, trust_generation: 1,
    }).unwrap();
    let descriptor = SecurityPartitionDescriptor::from_verified_ingress(&ingress).to_partition_descriptor().unwrap();
    let key = ContinuationPartitionKey::derive(&descriptor, &["tools:call"], "checkout-v1",
        "capabilities", "policy", "deployment").unwrap();
    let owner_key = DurableOwnerKey::derive(&descriptor, 1).unwrap();
    ContinuationReplayAuthority::new(key, PartitionAuthorization::current(&descriptor, &owner_key), owner.clone())
}
fn context() -> McpContext { McpContext::new(Cx::for_testing(), 1) }
fn request(state: &str) -> JsonRpcRequest {
    JsonRpcRequest::new("tools/call", Some(json!({
        "_meta": {"io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities":{"roots":{}}},
        "name":"checkout", "arguments":{"quantity":1}, "requestState":state,
    })), 1i64)
}
fn waiting(next: &str) -> Value {
    json!({"resultType":"input_required","requestState":next,
        "inputRequests":{"roots":{"method":"roots/list"}},"com.example/private":"secret-checkpoint"})
}
fn complete() -> Value { json!({"resultType":"complete","content":[{"type":"text","text":"done"}]}) }
fn journal(limits: ContinuationReplayLimits) -> ContinuationReplayMiddleware {
    let owner = McpRequestCancellation::new();
    ContinuationReplayMiddleware::new_with_successor_recovery(&Cx::for_testing(),
        ProcessGenerationGuard::install().unwrap(), SnapshotCloneStance::NoLiveMemoryCloning,
        limits, move |_, _| Ok(authority("alice", &owner))).unwrap()
}
fn capture(mw: &ContinuationReplayMiddleware, request: &JsonRpcRequest, response: Value) {
    assert!(matches!(mw.on_request(&context(), request).unwrap(), MiddlewareDecision::Continue));
    assert_eq!(mw.on_response(&context(), request, response.clone()).unwrap(), response);
}
fn replay(mw: &ContinuationReplayMiddleware, request: &JsonRpcRequest) -> Value {
    let MiddlewareDecision::Respond(response) = mw.on_request(&context(), request).unwrap() else { panic!("expected recovery"); };
    mw.on_response(&context(), request, response).unwrap()
}
fn accounting(mw: &ContinuationReplayMiddleware) {
    let state = mw.journal.lock().unwrap();
    assert_eq!(state.retained_bytes, state.entries.values().map(|entry| entry.charge).sum::<usize>());
    assert!(state.entries.len() <= mw.limits.maximum_entries);
    assert!(state.retained_bytes <= mw.limits.maximum_bytes);
    assert!(state.successors.len() <= state.entries.len());
    for (next, parent) in &state.successors {
        let entry = state.entries.get(parent).unwrap();
        assert!(entry.result.is_some());
        let transition = entry.transition.as_ref().unwrap();
        assert!(!transition.superseded);
        assert_eq!(transition.successor, Some(*next));
        assert!(!state.entries.contains_key(next));
    }
}

#[test]
fn intermediate_reply_recovers_exactly_without_extending_or_resealing() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("original");
    capture(&mw, &old, waiting("next"));
    let (expiry, ciphertext, bytes) = {
        let state = mw.journal.lock().unwrap();
        let entry = state.entries.values().next().unwrap();
        (entry.expires_at, entry.result.clone(), state.retained_bytes)
    };
    let mut retry = old.clone();
    retry.id = Some(RequestId::String("new-rpc-id".to_owned()));
    for _ in 0..3 { assert_eq!(replay(&mw, &retry), waiting("next")); }
    let state = mw.journal.lock().unwrap();
    let entry = state.entries.values().next().unwrap();
    assert_eq!((entry.expires_at, &entry.result, state.retained_bytes), (expiry, &ciphertext, bytes));
    assert!(!ciphertext.unwrap().windows(17).any(|w| w == b"secret-checkpoint"));
    drop(state);
    accounting(&mw);
}

#[test]
fn successor_admission_retires_ancestor_and_terminal_remains_recoverable() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("original");
    capture(&mw, &old, waiting("next"));
    let mut next = request("next");
    next.params.as_mut().unwrap()["inputResponses"] = json!({"roots":{"roots":[]}});
    assert!(matches!(mw.on_request(&context(), &next).unwrap(), MiddlewareDecision::Continue));
    assert!(mw.on_request(&context(), &old).is_err());
    assert!(mw.on_request(&context(), &next).is_err());
    assert_eq!(mw.on_response(&context(), &next, complete()).unwrap(), complete());
    assert_eq!(replay(&mw, &next), complete());
    assert!(mw.on_request(&context(), &old).is_err());
    accounting(&mw);
}

#[test]
fn changed_successor_operation_or_metadata_leaves_predecessor_usable() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("original");
    capture(&mw, &old, waiting("next"));
    let before = mw.journal.lock().unwrap().retained_bytes;
    for case in 0..3 {
        let mut next = request("next");
        let params = next.params.as_mut().unwrap();
        match case {
            0 => params["arguments"]["quantity"] = json!(2),
            1 => params["name"] = json!("other-tool"),
            _ => params["_meta"]["com.example/context"] = json!("changed"),
        }
        assert!(mw.on_request(&context(), &next).is_err());
        assert_eq!(mw.journal.lock().unwrap().retained_bytes, before);
        assert_eq!(replay(&mw, &old), waiting("next"));
    }
    accounting(&mw);
}

#[test]
fn entry_capacity_failure_cannot_destroy_the_recoverable_predecessor() {
    let mw = journal(ContinuationReplayLimits::new(1, 8192, 1024, 1024, Duration::from_secs(60)).unwrap());
    let old = request("original");
    capture(&mw, &old, waiting("next"));
    assert!(mw.on_request(&context(), &request("next")).is_err());
    assert_eq!(replay(&mw, &old), waiting("next"));
    accounting(&mw);
}

#[test]
fn byte_capacity_failure_is_atomic_before_successor_dispatch() {
    let probe = journal(ContinuationReplayLimits::new(4, 8192, 1024, 1024, Duration::from_secs(60)).unwrap());
    let one_reservation = probe.journal.lock().unwrap().protector.maximum_envelope_bytes() + CHAIN_IDENTITY_BYTES;
    let mw = journal(ContinuationReplayLimits::new(4, one_reservation, 1024, 1024, Duration::from_secs(60)).unwrap());
    let old = request("original");
    capture(&mw, &old, waiting("next"));
    assert!(mw.on_request(&context(), &request("next")).is_err());
    assert_eq!(replay(&mw, &old), waiting("next"));
    accounting(&mw);
}

#[test]
fn uncertain_successor_does_not_restore_an_ancestor_or_release_its_own_fence() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("original");
    let next = request("next");
    capture(&mw, &old, waiting("next"));
    mw.on_request(&context(), &next).unwrap();
    mw.on_error(&context(), &next, McpError::internal_error("uncertain effect"));
    mw.on_error(&context(), &old, McpError::invalid_params("duplicate"));
    assert!(mw.on_request(&context(), &old).is_err());
    assert!(mw.on_response(&context(), &old, waiting("next")).is_err());
    for entry in mw.journal.lock().unwrap().entries.values_mut() { entry.expires_at = Instant::now(); }
    assert_eq!(mw.prune(&Cx::for_testing()).unwrap(), 1, "retired ancestor only, not uncertain successor");
    assert!(mw.on_request(&context(), &next).is_err());
    accounting(&mw);
}

#[test]
fn completed_three_round_chain_recovers_each_reply_but_not_superseded_steps() {
    let mw = journal(ContinuationReplayLimits::default());
    let requests = [request("one"), request("two"), request("three")];
    for index in 0..3 {
        let response = if index == 2 { complete() } else { waiting(if index == 0 {"two"} else {"three"}) };
        capture(&mw, &requests[index], response.clone());
        assert_eq!(replay(&mw, &requests[index]), response);
        for previous in &requests[..index] { assert!(mw.on_request(&context(), previous).is_err()); }
        accounting(&mw);
    }
}

#[test]
fn repeated_successor_and_cycles_are_rejected_without_overwriting_another_link() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("one");
    capture(&mw, &old, waiting("two"));
    let other = request("other");
    mw.on_request(&context(), &other).unwrap();
    assert!(mw.on_response(&context(), &other, waiting("two")).is_err());
    assert_eq!(replay(&mw, &old), waiting("two"));
    let two = request("two");
    mw.on_request(&context(), &two).unwrap();
    assert!(mw.on_response(&context(), &two, waiting("one")).is_err());
    assert!(mw.on_response(&context(), &two, waiting("two")).is_err());
    accounting(&mw);
}

#[test]
fn expiry_prunes_the_reverse_index_and_never_exposes_expired_reply() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("one");
    capture(&mw, &old, waiting("two"));
    mw.journal.lock().unwrap().entries.values_mut().next().unwrap().expires_at = Instant::now();
    assert!(mw.on_request(&context(), &old).is_err());
    assert!(mw.on_request(&context(), &request("two")).is_err());
    assert_eq!(mw.prune(&Cx::for_testing()).unwrap(), 1);
    let state = mw.journal.lock().unwrap();
    assert!(state.entries.is_empty() && state.successors.is_empty());
    assert_eq!(state.retained_bytes, 0);
}

#[test]
fn current_authorization_failure_does_not_retire_the_previous_reply() {
    let deny = Arc::new(AtomicBool::new(false));
    let captured = deny.clone();
    let owner = McpRequestCancellation::new();
    let mw = ContinuationReplayMiddleware::new_with_successor_recovery(&Cx::for_testing(),
        ProcessGenerationGuard::install().unwrap(), SnapshotCloneStance::NoLiveMemoryCloning,
        ContinuationReplayLimits::default(), move |_, _| {
            if captured.load(Ordering::SeqCst) { return Err(unavailable()); }
            Ok(authority("alice", &owner))
        }).unwrap();
    let old = request("one");
    capture(&mw, &old, waiting("two"));
    deny.store(true, Ordering::SeqCst);
    assert!(mw.on_request(&context(), &request("two")).is_err());
    deny.store(false, Ordering::SeqCst);
    assert_eq!(replay(&mw, &old), waiting("two"));
    accounting(&mw);
}

#[test]
fn recovered_reply_racing_successor_cannot_republish_the_retired_checkpoint() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("one");
    capture(&mw, &old, waiting("two"));
    let MiddlewareDecision::Respond(in_flight) = mw.on_request(&context(), &old).unwrap() else { panic!(); };
    mw.on_request(&context(), &request("two")).unwrap();
    assert!(mw.on_response(&context(), &old, in_flight).is_err());
    accounting(&mw);
}

#[test]
fn key_rotation_preserves_the_intermediate_reply_and_close_revokes_every_link() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("one");
    capture(&mw, &old, waiting("two"));
    mw.rotate(&Cx::for_testing()).unwrap();
    assert_eq!(replay(&mw, &old), waiting("two"));
    mw.close().unwrap();
    assert!(mw.on_request(&context(), &old).is_err());
    assert!(mw.journal.lock().unwrap().successors.is_empty());
}

#[test]
fn malformed_successor_does_not_gain_replay_or_index_authority() {
    for response in [json!({"resultType":"input_required","requestState":null}),
        json!({"resultType":"input_required","requestState":"next","inputRequests":null})] {
        let mw = journal(ContinuationReplayLimits::default());
        let old = request("one");
        mw.on_request(&context(), &old).unwrap();
        assert!(mw.on_response(&context(), &old, response).is_err());
        assert!(mw.on_request(&context(), &old).is_err());
        assert!(mw.journal.lock().unwrap().successors.is_empty());
        accounting(&mw);
    }
}

#[test]
fn input_required_without_a_successor_passes_through_but_is_not_replayed() {
    let mw = journal(ContinuationReplayLimits::default());
    let old = request("one");
    let response = json!({"resultType":"input_required","inputRequests":{"roots":{"method":"roots/list"}}});
    capture(&mw, &old, response);
    assert!(mw.on_request(&context(), &old).is_err());
    assert!(mw.journal.lock().unwrap().successors.is_empty());
}
