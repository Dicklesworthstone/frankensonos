//! Optional recovery of intermediate replies, without minting or reopening state.
//!
//! A predecessor reply stops being recoverable when its matching successor is
//! admitted, not when that successor finishes. This is deliberately conservative:
//! after an uncertain successor attempt, never send an earlier reply as though
//! the caller should start that successor again. The router still decides whether
//! any supplied continuation or input response is valid.

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use super::{
    ContinuationReplayAuthority, ContinuationReplayLimits, ContinuationReplayMiddleware, Cx,
    IDENTITY_BYTES, Identity, Instant, JsonRpcRequest, Journal, LimitedWriter, McpContext,
    McpResult, ProcessGenerationGuard, SnapshotCloneStance, Value, Write, check_context,
    sha256_bounded, unavailable,
};

// Per entry: existing slot/fingerprint (64), stable-operation digest (32),
// successor selector (32) and one reverse-index key/value (64). Containers and
// cancellation handles have separate cardinality bounds (maximum_entries).
const CHAIN_IDENTITY_BYTES: usize = IDENTITY_BYTES + 128;
const RETIRED_IDENTITY_BYTES: usize = IDENTITY_BYTES + 32;

pub(super) struct SuccessorEntry {
    operation: [u8; 32],
    pub(super) successor: Option<[u8; 32]>,
    pub(super) superseded: bool,
}
impl SuccessorEntry {
    pub(super) fn new(operation: [u8; 32]) -> Self {
        Self { operation, successor: None, superseded: false }
    }
}

impl ContinuationReplayMiddleware {
    /// Recovers both terminal results and `input_required` replies carrying a
    /// nonempty successor requestState. The ordinary `new` constructor keeps
    /// its terminal-only behavior. Initial calls remain uncached in both modes.
    ///
    /// Route ALL retries for this endpoint through this same journal and use a
    /// stable authorization partition across rounds. The partition must bind
    /// current permissions and handler/configuration revisions, but not the
    /// changing requestState/inputResponses. The journal additionally compares
    /// every other parameter (including arguments and metadata) across a link.
    ///
    /// Register admission-only authorization/rate limits before this middleware
    /// and result transformations after it. Hits still authenticate and invoke
    /// the fresh authorization callback; they do not rerun later transforms.
    ///
    /// Admission of a successor atomically retires its predecessor's reply.
    /// Wrong arguments/metadata, failed authorization and failed reservation do
    /// not retire it. Once admitted, even an invalid answer or a lost successor
    /// outcome leaves the predecessor retired: errors never authorize replay of
    /// uncertain work. Retired identity records remain until expiry/revocation.
    ///
    /// This recovers only a captured response, not the registry's state. The
    /// registry's expiry, cancellation, one-use checks and transport restrictions
    /// still apply on resumption; recovering a reply cannot extend them. Requests
    /// bypassing this journal, process restart, and stream-event recovery are
    /// outside this mode. It is not durable or exactly-once execution.
    pub fn new_with_successor_recovery<F>(
        cx: &Cx, guard: &ProcessGenerationGuard, stance: SnapshotCloneStance,
        limits: ContinuationReplayLimits, authorize: F,
    ) -> McpResult<Self>
    where
        F: Fn(&McpContext, &JsonRpcRequest) -> McpResult<ContinuationReplayAuthority>
            + Send + Sync + 'static,
    {
        let mut middleware = Self::new(cx, guard, stance, limits, authorize)?;
        middleware.recover_successors = true;
        Ok(middleware)
    }

    pub(super) fn identity_bytes(&self) -> usize {
        if self.recover_successors { CHAIN_IDENTITY_BYTES } else { IDENTITY_BYTES }
    }

    pub(super) fn slot(
        &self, authority: &ContinuationReplayAuthority, method: &str, state: &str,
    ) -> McpResult<[u8; 32]> {
        // Retain the terminal-only selector's exact domain and framing.
        let maximum = self.limits.request_bytes + 256;
        let mut selector = LimitedWriter::new(maximum);
        selector.write_all(b"fastmcp/mrtr-terminal-slot/v1\0").map_err(|_| unavailable())?;
        selector.write_all(authority.key.as_bytes()).map_err(|_| unavailable())?;
        selector.write_all(authority.authorization.as_bytes()).map_err(|_| unavailable())?;
        serde_json::to_writer(&mut selector, &(method, state)).map_err(|_| unavailable())?;
        Ok(sha256_bounded(&selector.bytes, maximum).map_err(|_| unavailable())?.into_bytes())
    }
}

impl Journal {
    /// Pure admission: caller checks quota and liveness again before committing.
    pub(super) fn predecessor(&self, ctx: &McpContext, identity: &Identity)
        -> McpResult<Option<([u8; 32], usize)>>
    {
        let Some(parent) = self.successors.get(&identity.slot) else { return Ok(None); };
        let entry = self.entries.get(parent).ok_or_else(unavailable)?;
        let transition = entry.transition.as_ref().ok_or_else(unavailable)?;
        check_context(ctx)?;
        if transition.superseded || transition.successor != Some(identity.slot)
            || identity.operation != Some(transition.operation)
            || entry.lifetime.is_cancel_requested() || identity.lifetime.is_cancel_requested()
            || Instant::now() >= entry.expires_at || entry.result.is_none()
        { return Err(unavailable()); }
        let released = entry.charge.checked_sub(RETIRED_IDENTITY_BYTES).ok_or_else(unavailable)?;
        Ok(Some((*parent, released)))
    }

    /// Called only after predecessor/quota admission under the same journal lock.
    /// No allocation, user callback or fallible operation follows this transition.
    pub(super) fn retire_predecessor(&mut self, parent: [u8; 32], successor: [u8; 32]) {
        let entry = self.entries.get_mut(&parent).expect("admitted predecessor remains under lock");
        let transition = entry.transition.as_mut().expect("admitted successor mode");
        transition.superseded = true;
        transition.successor = None;
        entry.result = None;
        entry.charge = RETIRED_IDENTITY_BYTES;
        self.successors.remove(&successor);
    }

    pub(super) fn admit_successor(&self, parent: [u8; 32], successor: [u8; 32]) -> McpResult<()> {
        // Never overwrite another exchange's index, create a self-loop, or
        // advertise a previously admitted/retired state as a fresh successor.
        if parent == successor || self.entries.contains_key(&successor)
            || self.successors.contains_key(&successor)
        { return Err(unavailable()); }
        Ok(())
    }

    pub(super) fn prune_finished(&mut self, now: Instant) -> usize {
        let stale: Vec<_> = self.entries.iter().filter_map(|(slot, entry)| {
            let finished = entry.result.is_some()
                || entry.transition.as_ref().is_some_and(|transition| transition.superseded);
            (finished && (now >= entry.expires_at || entry.lifetime.is_cancel_requested())).then_some(*slot)
        }).collect();
        for slot in &stale {
            let entry = self.entries.remove(slot).expect("selected entry remains under lock");
            if let Some(next) = entry.transition.and_then(|transition| transition.successor) {
                self.successors.remove(&next);
            }
            self.retained_bytes -= entry.charge;
        }
        stale.len()
    }
}

/// Serialize by borrowing the admitted parameter tree; don't clone secrets or
/// normalize number lexemes. Only the two protocol continuation fields vary.
struct StableParams<'a>(&'a serde_json::Map<String, Value>);
impl Serialize for StableParams<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let count = self.0.keys().filter(|key| !matches!(key.as_str(), "requestState" | "inputResponses")).count();
        let mut map = serializer.serialize_map(Some(count))?;
        for (key, value) in self.0 {
            if !matches!(key.as_str(), "requestState" | "inputResponses") { map.serialize_entry(key, value)?; }
        }
        map.end()
    }
}
pub(super) fn operation_fingerprint(method: &str, params: &Value, maximum: usize) -> McpResult<[u8; 32]> {
    let object = params.as_object().ok_or_else(unavailable)?;
    let mut encoded = LimitedWriter::new(maximum);
    serde_json::to_writer(&mut encoded, &(method, StableParams(object))).map_err(|_| unavailable())?;
    Ok(sha256_bounded(&encoded.bytes, maximum).map_err(|_| unavailable())?.into_bytes())
}

#[cfg(test)]
mod tests;
