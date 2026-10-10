//! Response caching middleware for MCP servers.
//!
//! This module provides a bounded, process-local response cache for MCP methods
//! whose results are safe to share between otherwise unrelated requests.
//!
//! Cache keys include the complete verified [`fastmcp_core::AuthContext`] plus
//! its hidden stable owner key. Session-backed requests additionally include a
//! cryptographically opaque session-state identity and monotonic mutation
//! revision. Stateless authenticated contexts are therefore isolated by both
//! handler-visible facts and provider-scoped ownership. Requests with
//! uncommitted authentication, or session state whose partition cannot be
//! obtained, bypass lookup and storage. Direct anonymous contexts remain in a
//! separate stateless domain for standalone middleware use and tests.
//!
//! # Cached Methods
//!
//! For a live context with a safe complete partition (or a standalone context
//! with neither session nor auth), the default method policy permits caching:
//! - `server/discover` - its final private `ttlMs` policy
//! - `tools/list` - 5 minute TTL
//! - `resources/list` - 5 minute TTL
//! - `resources/templates/list` - 5 minute TTL
//! - `prompts/list` - 5 minute TTL
//! - `resources/read` - 1 hour TTL
//! - `prompts/get` - 1 hour TTL
//! - `tools/call` - disabled unless individual tool names are explicitly
//!   allowlisted with [`ResponseCachingMiddleware::include_tools`]
//!
//! Exclusions always override the `tools/call` allowlist. Tool-call caching is
//! intended only for tools whose results are deterministic, side-effect free,
//! and independent of external mutable state. Even an allowlisted tool is
//! bypassed when a complete safe cache partition cannot be derived.
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_rust::{modern::ServerBuilder, prelude::*};
//! use fastmcp_rust::caching::ResponseCachingMiddleware;
//!
//! let caching = ResponseCachingMiddleware::new()
//!     .list_ttl_secs(600)  // 10 minute TTL for list operations
//!     .call_ttl_secs(3600) // 1 hour TTL for call/get/read operations
//!     .include_tools(vec!["deterministic_lookup".to_string()]);
//!
//! // Inside the application's async entry point, with its supplied `cx`.
//! ServerBuilder::new("my-server", "1.0.0")
//!     .middleware(caching)
//!     .build()
//!     .run_stdio_with_cx(cx)
//!     .await;
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fastmcp_core::{McpContext, McpError, McpResult, Sha256Digest, sha256_bounded};
use fastmcp_protocol::protocol_policy::ProtocolEra;
use fastmcp_protocol::{
    CacheTtl, FINAL_PROTOCOL_VERSION, FINAL_PROTOCOL_VERSION_META_KEY, JsonRpcRequest,
    SERVER_DISCOVER_METHOD,
};

use crate::{Middleware, MiddlewareDecision};

/// Default TTL for list operations (5 minutes).
pub const DEFAULT_LIST_TTL_SECS: u64 = 300;

/// Default TTL for allowlisted call/get/read operations (1 hour).
pub const DEFAULT_CALL_TTL_SECS: u64 = 3600;

/// Maximum cache item size in bytes (1 MB).
pub const DEFAULT_MAX_ITEM_SIZE: usize = 1024 * 1024;

/// Maximum canonical input admitted while deriving one fixed-width cache key.
const MAX_CACHE_KEY_INPUT_BYTES: usize = 10 * 1024 * 1024;

/// Maximum JSON nesting and aggregate nodes admitted to cache serialization.
const MAX_CACHE_JSON_DEPTH: usize = 128;
const MAX_CACHE_JSON_NODES: usize = 100_000;

/// Small writes share an initial allocation and subsequent growth doubles the
/// current capacity. Every target is still capped by the caller's logical byte
/// limit, so fragmented serializer output cannot trigger one allocation per
/// fragment or reserve beyond the configured bound.
const CACHE_BYTES_GROWTH_CHUNK: usize = 4 * 1024;

/// Conservative accounting for the entry, duplicate map/order keys, hash-table
/// bucket/control storage, the `Arc` allocation header, and allocator metadata.
/// The encoded payload length is added separately.
const CACHE_ENTRY_METADATA_BYTES: usize = 512;

/// Domain separators for cache request and authorization/session partitions.
const CACHE_REQUEST_KEY_DOMAIN: &[u8] = b"fastmcp-response-cache-request-v2\0";
const CACHE_INVALIDATION_KEY_DOMAIN: &[u8] = b"fastmcp-response-cache-invalidation-v1\0";
const CACHE_PARTITION_KEY_DOMAIN: &[u8] = b"fastmcp-response-cache-partition-v2\0";
const CACHE_STATELESS_PARTITION_DOMAIN: &[u8] = b"fastmcp-response-cache-stateless-partition-v1\0";

static NEXT_CACHE_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Allocates a nonzero response-cache hit identity; 0 means the space is exhausted.
pub(crate) fn next_cache_instance_id() -> u64 {
    NEXT_CACHE_INSTANCE_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .unwrap_or(0)
}

/// A cached response with expiration time.
#[derive(Clone)]
struct CacheEntry {
    encoded: Arc<[u8]>,
    expires_at: Instant,
    size_bytes: usize,
}

impl std::fmt::Debug for CacheEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheEntry")
            .field("payload_bytes", &self.encoded.len())
            .field("expires_at", &self.expires_at)
            .field("accounted_bytes", &self.size_bytes)
            .finish()
    }
}

impl CacheEntry {
    fn new(value: serde_json::Value, ttl: Duration, max_size_bytes: usize) -> Option<Self> {
        let encoded = encode_json_bounded(&value, max_size_bytes)?;
        Self::new_encoded(encoded, ttl)
    }

    fn new_encoded(encoded: Arc<[u8]>, ttl: Duration) -> Option<Self> {
        if ttl.is_zero() {
            return None;
        }
        let expires_at = Instant::now().checked_add(ttl)?;
        let size_bytes = encoded.len().checked_add(CACHE_ENTRY_METADATA_BYTES)?;
        Some(Self {
            encoded,
            expires_at,
            size_bytes,
        })
    }

    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// Cache key derived from method and parameters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    request_digest: Sha256Digest,
    invalidation_digest: Sha256Digest,
    partition_digest: Sha256Digest,
    binding: CacheEntryBinding,
}

/// Extra identity attached only to final discovery cache entries.
///
/// Discovery is a final-only surface, but the cache can sit behind a dual-era
/// transport. The selected era and the invalidation generation therefore
/// participate in the key rather than being inferred from a previously cached
/// payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DiscoveryCacheBinding {
    era: ProtocolEra,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CacheEntryBinding {
    Ordinary,
    Discovery(DiscoveryCacheBinding),
}

impl CacheKey {
    fn try_request_digest(
        method: &str,
        params: Option<&serde_json::Value>,
    ) -> Option<Sha256Digest> {
        Self::try_digest(CACHE_REQUEST_KEY_DOMAIN, method, params)
    }

    /// Derives the identity used to invalidate an entire paginated result set.
    ///
    /// The request key still includes the opaque cursor exactly, so distinct
    /// pages cannot collide at lookup. Invalidation intentionally removes the
    /// cursor from the semantic result-set identity so a catalog or resource
    /// mutation cannot leave another page observable from the old generation.
    fn try_invalidation_digest(
        method: &str,
        params: Option<&serde_json::Value>,
    ) -> Option<Sha256Digest> {
        let mut projection = params.cloned();
        if let Some(serde_json::Value::Object(object)) = projection.as_mut() {
            object.remove("cursor");
            // A cursor-only request has an empty semantic parameter set; it
            // must share the invalidation identity of the parameterless
            // request, or `invalidate(method, None)` can never remove cached
            // continuation pages after a catalog mutation.
            if object.is_empty() {
                projection = None;
            }
        }
        Self::try_digest(CACHE_INVALIDATION_KEY_DOMAIN, method, projection.as_ref())
    }

    fn try_digest(
        domain: &[u8],
        method: &str,
        params: Option<&serde_json::Value>,
    ) -> Option<Sha256Digest> {
        if params.is_some_and(|params| !cache_json_shape_is_bounded(params)) {
            return None;
        }
        let mut canonical = BoundedCacheBytes::new(MAX_CACHE_KEY_INPUT_BYTES);
        canonical.write_all(domain).ok()?;
        let method_len = u64::try_from(method.len()).ok()?;
        canonical.write_all(&method_len.to_be_bytes()).ok()?;
        canonical.write_all(method.as_bytes()).ok()?;
        match params {
            None => canonical.write_all(&[0]).ok()?,
            Some(params) => {
                canonical.write_all(&[1]).ok()?;
                serde_json::to_writer(&mut canonical, params).ok()?;
            }
        }
        sha256_bounded(&canonical.bytes, MAX_CACHE_KEY_INPUT_BYTES).ok()
    }

    fn try_new_partitioned(
        method: &str,
        params: Option<&serde_json::Value>,
        partition_digest: Sha256Digest,
        binding: CacheEntryBinding,
    ) -> Option<Self> {
        Some(Self {
            request_digest: Self::try_request_digest(method, params)?,
            invalidation_digest: Self::try_invalidation_digest(method, params)?,
            partition_digest,
            binding,
        })
    }

    #[cfg(test)]
    fn try_new(method: &str, params: Option<&serde_json::Value>) -> Option<Self> {
        let partition_digest = sha256_bounded(
            CACHE_STATELESS_PARTITION_DOMAIN,
            CACHE_STATELESS_PARTITION_DOMAIN.len(),
        )
        .ok()?;
        Self::try_new_partitioned(
            method,
            params,
            partition_digest,
            CacheEntryBinding::Ordinary,
        )
    }

    #[cfg(test)]
    fn new(method: &str, params: Option<&serde_json::Value>) -> Self {
        Self::try_new(method, params).expect("test cache key must fit the fixed input bound")
    }
}

struct BoundedCacheBytes {
    bytes: Vec<u8>,
    max_bytes: usize,
    #[cfg(test)]
    growth_events: usize,
}

impl BoundedCacheBytes {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            #[cfg(test)]
            growth_events: 0,
        }
    }

    fn ensure_capacity_for(&mut self, next_size: usize) -> std::io::Result<()> {
        if next_size <= self.bytes.capacity() {
            return Ok(());
        }

        let current_capacity = self.bytes.capacity();
        let chunk_target = CACHE_BYTES_GROWTH_CHUNK.min(self.max_bytes);
        let geometric_target = if current_capacity == 0 {
            chunk_target
        } else {
            current_capacity
                .checked_mul(2)
                .unwrap_or(self.max_bytes)
                .min(self.max_bytes)
        };
        let target_capacity = next_size.max(geometric_target).min(self.max_bytes);

        // Allocate separately so even an allocator that reports more capacity
        // than requested cannot leave this bounded writer above its logical
        // limit. The existing buffer remains intact on every failure path.
        let mut grown = Vec::new();
        grown
            .try_reserve_exact(target_capacity)
            .map_err(|_| std::io::Error::other("cannot allocate bounded cache input"))?;
        if grown.capacity() > self.max_bytes {
            return Err(std::io::Error::other(
                "cache input allocation exceeds configured limit",
            ));
        }
        grown.extend_from_slice(&self.bytes);
        self.bytes = grown;
        #[cfg(test)]
        {
            self.growth_events = self.growth_events.saturating_add(1);
        }
        Ok(())
    }
}

impl Write for BoundedCacheBytes {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next_size = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .filter(|size| *size <= self.max_bytes)
            .ok_or_else(|| std::io::Error::other("cache input exceeds configured limit"))?;
        self.ensure_capacity_for(next_size)?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn cache_json_shape_is_bounded(value: &serde_json::Value) -> bool {
    let mut stack = Vec::new();
    if stack.try_reserve_exact(1).is_err() {
        return false;
    }
    stack.push((value, 0_usize));
    let mut admitted_nodes = 1_usize;

    while let Some((node, depth)) = stack.pop() {
        let child_depth = match depth.checked_add(1) {
            Some(depth) => depth,
            None => return false,
        };
        match node {
            serde_json::Value::Array(values) => {
                if !values.is_empty() && child_depth > MAX_CACHE_JSON_DEPTH {
                    return false;
                }
                admitted_nodes = match admitted_nodes
                    .checked_add(values.len())
                    .filter(|nodes| *nodes <= MAX_CACHE_JSON_NODES)
                {
                    Some(nodes) => nodes,
                    None => return false,
                };
                if stack.try_reserve(values.len()).is_err() {
                    return false;
                }
                stack.extend(values.iter().map(|value| (value, child_depth)));
            }
            serde_json::Value::Object(values) => {
                if !values.is_empty() && child_depth > MAX_CACHE_JSON_DEPTH {
                    return false;
                }
                admitted_nodes = match admitted_nodes
                    .checked_add(values.len())
                    .filter(|nodes| *nodes <= MAX_CACHE_JSON_NODES)
                {
                    Some(nodes) => nodes,
                    None => return false,
                };
                if stack.try_reserve(values.len()).is_err() {
                    return false;
                }
                stack.extend(values.values().map(|value| (value, child_depth)));
            }
            _ => {}
        }
    }

    true
}

fn encode_json_bounded(value: &serde_json::Value, max_bytes: usize) -> Option<Arc<[u8]>> {
    if !cache_json_shape_is_bounded(value) {
        return None;
    }
    let mut encoded = BoundedCacheBytes::new(max_bytes);
    serde_json::to_writer(&mut encoded, value).ok()?;
    Some(Arc::from(encoded.bytes.into_boxed_slice()))
}

fn decode_cached_json(encoded: &[u8]) -> Option<serde_json::Value> {
    serde_json::from_slice(encoded).ok()
}

#[derive(Clone, Copy)]
enum CachePartitionPhase {
    Request,
    Response,
}

fn context_cache_partition(ctx: &McpContext, phase: CachePartitionPhase) -> Option<Sha256Digest> {
    ctx.ensure_live().ok()?;
    // Authentication admission is write-once. An uncommitted request must
    // never consult or populate a cache merely because it has no session.
    let auth_partition = ctx.cache_auth_partition()?;
    let session_partition = match phase {
        CachePartitionPhase::Request => ctx.begin_session_cache_partition(),
        CachePartitionPhase::Response => ctx.complete_session_cache_partition(),
    };
    if let Some(auth) = auth_partition.as_ref() {
        if auth.scopes.len() > MAX_CACHE_JSON_NODES
            || auth
                .claims
                .as_ref()
                .is_some_and(|claims| !cache_json_shape_is_bounded(claims))
        {
            return None;
        }
    }

    let mut canonical = BoundedCacheBytes::new(MAX_CACHE_KEY_INPUT_BYTES);
    canonical.write_all(CACHE_PARTITION_KEY_DOMAIN).ok()?;
    if ctx.session_is_ephemeral() {
        // Per-POST modern HTTP state exists so disable_*/enable_* can
        // publish list_changed. It is not a durable cache identity.
        canonical.write_all(CACHE_STATELESS_PARTITION_DOMAIN).ok()?;
    } else {
        match session_partition {
            Some((opaque_session, state_revision)) => {
                canonical.write_all(&[1]).ok()?;
                canonical.write_all(&opaque_session).ok()?;
                canonical.write_all(&state_revision.to_be_bytes()).ok()?;
            }
            None if ctx.has_session_state() => return None,
            None => canonical.write_all(CACHE_STATELESS_PARTITION_DOMAIN).ok()?,
        }
    }
    match auth_partition {
        None => canonical.write_all(&[0]).ok()?,
        Some(auth) => {
            canonical.write_all(&[1]).ok()?;
            match auth.session_owner() {
                None => canonical.write_all(&[0]).ok()?,
                Some(owner) => {
                    canonical.write_all(&[1]).ok()?;
                    canonical.write_all(owner.as_bytes()).ok()?;
                }
            }
            serde_json::to_writer(&mut canonical, &auth).ok()?;
        }
    }
    sha256_bounded(&canonical.bytes, MAX_CACHE_KEY_INPUT_BYTES).ok()
}

fn context_cache_commit_is_admissible(ctx: &McpContext) -> bool {
    // Check the session partition before the final liveness read. In
    // particular, `has_session_state()` intentionally reports false after a
    // request lease closes; the final `ensure_live()` prevents that transition
    // from being mistaken for a genuinely stateless request.
    let session_partition_is_current =
        !ctx.has_session_state() || ctx.complete_session_cache_partition().is_some();
    session_partition_is_current && ctx.ensure_live().is_ok()
}

/// Returns whether request parameters carry state from a multi-round-trip
/// continuation. Such requests are never deterministic cache lookups, even
/// when their eventual result happens to be complete.
fn request_carries_uncacheable_continuation(params: Option<&serde_json::Value>) -> bool {
    let Some(serde_json::Value::Object(params)) = params else {
        return false;
    };
    params.contains_key("inputResponses") || params.contains_key("requestState")
}

/// Returns whether a response can be stored by the internal memoization cache.
///
/// Modern responses must explicitly be `complete`; input-required and task
/// branches never enter this cache. The absent discriminator remains accepted
/// for the current exact-2024 compatibility surface, which did not carry
/// `resultType`. Continuation-bearing payloads are rejected in either era.
fn response_is_cacheable_complete(response: &serde_json::Value) -> bool {
    let Some(response) = response.as_object() else {
        return false;
    };
    if [
        "inputResponses",
        "requestState",
        "task",
        "taskId",
        "taskStatus",
        "requestScopedNotifications",
        "notifications",
    ]
    .iter()
    .any(|field| response.contains_key(*field))
    {
        return false;
    }
    match response.get("resultType") {
        None => true,
        Some(serde_json::Value::String(kind)) => kind == "complete",
        Some(_) => false,
    }
}

/// The cache policy attached to an exact final discovery result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalDiscoveryCachePolicy {
    Private(Duration),
    Public,
}

/// Local cache-admission decision for a complete final method result.
///
/// This is deliberately separate from the wire cache hints: a valid final
/// `ttlMs` may exceed the bounded local duration domain and must then remain
/// deliverable without creating a local cache entry.
enum FinalCompleteCachePolicy {
    Private(Duration),
    Public,
}

/// Returns whether a result carries the exact final discovery shape relevant
/// to caching. Full discovery validation remains owned by the protocol/server
/// boundary; this narrower check prevents legacy lookalikes from selecting a
/// final cache policy.
fn is_final_discovery_result(response: &serde_json::Value) -> bool {
    let Some(response) = response.as_object() else {
        return false;
    };
    response
        .get("capabilities")
        .is_some_and(serde_json::Value::is_object)
        && response
            .get("supportedVersions")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|versions| {
                versions.len() == 1 && versions[0].as_str() == Some(FINAL_PROTOCOL_VERSION)
            })
}

/// Reads the final discovery cache policy exactly as it appears on the wire.
fn final_discovery_cache_policy(response: &serde_json::Value) -> Option<FinalDiscoveryCachePolicy> {
    if !is_final_discovery_result(response) {
        return None;
    }
    let response = response.as_object()?;
    let ttl_ms = serde_json::from_value::<CacheTtl>(response.get("ttlMs")?.clone()).ok()?;
    match response.get("cacheScope")?.as_str()? {
        "private" => Some(FinalDiscoveryCachePolicy::Private(Duration::from_millis(
            ttl_ms.try_as_millis().ok()?,
        ))),
        "public" => Some(FinalDiscoveryCachePolicy::Public),
        _ => None,
    }
}

fn final_complete_cache_policy(response: &serde_json::Value) -> Option<FinalCompleteCachePolicy> {
    let response = response.as_object()?;
    let ttl_ms = serde_json::from_value::<CacheTtl>(response.get("ttlMs")?.clone()).ok()?;
    match response.get("cacheScope")?.as_str()? {
        "private" => Some(FinalCompleteCachePolicy::Private(Duration::from_millis(
            ttl_ms.try_as_millis().ok()?,
        ))),
        "public" => Some(FinalCompleteCachePolicy::Public),
        _ => None,
    }
}

/// Returns whether a final discovery result has schema-valid cache hints.
///
/// A wire-valid TTL can exceed the local runtime duration domain. That is not
/// a reason to rewrite the peer response: callers must still observe its exact
/// JSON-integer spelling, while this process simply declines to cache it.
fn final_discovery_cache_hints_are_wire_valid(response: &serde_json::Value) -> bool {
    let Some(response) = response.as_object() else {
        return false;
    };
    let Some(ttl_ms) = response.get("ttlMs") else {
        return false;
    };
    serde_json::from_value::<CacheTtl>(ttl_ms.clone()).is_ok()
        && matches!(
            response
                .get("cacheScope")
                .and_then(serde_json::Value::as_str),
            Some("private" | "public")
        )
}

/// Selects the request era used by the discovery cache.
///
/// Final transports remove the recognized version metadata before middleware
/// runs, so missing metadata here represents an already-admitted final
/// request. An explicitly supplied legacy or unsupported version is never
/// allowed to reuse the final discovery cache.
fn discovery_request_protocol_era(request: &JsonRpcRequest) -> Option<ProtocolEra> {
    let version = request
        .params
        .as_ref()
        .and_then(|params| params.get("_meta"))
        .and_then(|metadata| metadata.get(FINAL_PROTOCOL_VERSION_META_KEY))
        .and_then(serde_json::Value::as_str);
    match version {
        None | Some(FINAL_PROTOCOL_VERSION) => Some(ProtocolEra::Modern2026),
        Some(version) if version == ProtocolEra::Legacy2024.version().as_str() => {
            Some(ProtocolEra::Legacy2024)
        }
        Some(_) => None,
    }
}

/// Returns whether a method requires `ttlMs` and `cacheScope` in a modern
/// complete result. This list is protocol-facing and deliberately independent
/// from the internal memoization allowlist.
fn method_requires_protocol_cache_hints(method: &str) -> bool {
    matches!(
        method,
        "server/discover"
            | "tools/list"
            | "prompts/list"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
    )
}

/// Configuration for caching specific methods.
#[derive(Debug, Clone)]
pub struct MethodCacheConfig {
    /// Whether caching is enabled for this method.
    pub enabled: bool,
    /// Time to live in seconds.
    pub ttl_secs: u64,
}

impl Default for MethodCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_secs: DEFAULT_CALL_TTL_SECS,
        }
    }
}

/// Configuration for `tools/call` caching.
///
/// A tool is cacheable only when [`MethodCacheConfig::enabled`] is `true`, its
/// name appears in [`Self::included_tools`], and its name does not appear in
/// [`Self::excluded_tools`].
#[derive(Debug, Clone, Default)]
pub struct ToolCallCacheConfig {
    /// Base configuration.
    pub base: MethodCacheConfig,
    /// Tools explicitly allowlisted for caching (empty disables tool caching).
    pub included_tools: Vec<String>,
    /// Tools to exclude (takes precedence over included).
    pub excluded_tools: Vec<String>,
}

impl ToolCallCacheConfig {
    /// Checks if a specific tool should be cached.
    fn should_cache_tool(&self, tool_name: &str) -> bool {
        if !self.base.enabled {
            return false;
        }

        // Check exclusions first (takes precedence)
        if self.excluded_tools.iter().any(|name| name == tool_name) {
            return false;
        }

        // Tool calls are stateful by default. Only an explicit allowlist entry
        // can opt a tool into caching.
        self.included_tools.iter().any(|name| name == tool_name)
    }
}

/// Simple LRU cache with TTL support.
#[derive(Debug)]
struct LruCache {
    /// Request-time admission epoch, advanced under the same lock as removal
    /// and insertion. Zero permanently disables new fills after exhaustion.
    fill_generation: u64,
    /// Map of keys to entries.
    entries: HashMap<CacheKey, CacheEntry>,
    /// Order of keys for LRU eviction (most recent at the end).
    order: Vec<CacheKey>,
    /// Maximum number of entries.
    max_entries: usize,
    /// Maximum total size in bytes.
    max_size_bytes: usize,
    /// Maximum size per item in bytes.
    max_item_size: usize,
    /// Current total size in bytes.
    current_size_bytes: usize,
}

impl LruCache {
    fn new(max_entries: usize, max_size_bytes: usize, max_item_size: usize) -> Self {
        Self {
            fill_generation: 1,
            entries: HashMap::new(),
            order: Vec::new(),
            max_entries,
            max_size_bytes,
            max_item_size,
            current_size_bytes: 0,
        }
    }

    fn get_encoded(&mut self, key: &CacheKey) -> Option<Arc<[u8]>> {
        // Check if entry exists and is not expired
        if let Some(entry) = self.entries.get(key) {
            if entry.is_expired() {
                // Remove expired entry
                self.remove(key);
                return None;
            }

            // Move to end of order (most recently used)
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                let k = self.order.remove(pos);
                self.order.push(k);
            }

            return Some(Arc::clone(&entry.encoded));
        }
        None
    }

    #[cfg(test)]
    fn get_value(&mut self, key: &CacheKey) -> Option<serde_json::Value> {
        self.get_encoded(key)
            .and_then(|encoded| decode_cached_json(&encoded))
    }

    fn insert(&mut self, key: CacheKey, value: serde_json::Value, ttl: Duration) {
        let admission_limit = self.max_item_size.min(
            self.max_size_bytes
                .saturating_sub(CACHE_ENTRY_METADATA_BYTES),
        );
        let Some(entry) = CacheEntry::new(value, ttl, admission_limit) else {
            // An unrepresentable expiration must not turn into a panic or an
            // accidentally immortal entry.
            return;
        };
        self.insert_entry(key, entry);
    }

    fn insert_encoded(&mut self, key: CacheKey, encoded: Arc<[u8]>, ttl: Duration) {
        if encoded.len() > self.max_item_size {
            return;
        }
        let Some(entry) = CacheEntry::new_encoded(encoded, ttl) else {
            return;
        };
        self.insert_entry(key, entry);
    }

    fn insert_entry(&mut self, key: CacheKey, entry: CacheEntry) {
        // Reject impossible configurations and entries that can never fit.
        // These checks happen before replacing an existing value, so a rejected
        // replacement cannot destroy a valid cached entry.
        if self.max_entries == 0
            || self.max_size_bytes == 0
            || entry.encoded.len() > self.max_item_size
            || entry.size_bytes > self.max_size_bytes
        {
            return;
        }

        // Expired entries should not force eviction of live entries.
        self.evict_expired();

        // Remove old entry if it exists
        if self.entries.contains_key(&key) {
            self.remove(&key);
        }

        // Evict entries if needed to make room
        while self.entries.len() >= self.max_entries
            || self
                .current_size_bytes
                .checked_add(entry.size_bytes)
                .is_none_or(|size| size > self.max_size_bytes)
        {
            if self.order.is_empty() {
                // An inconsistent accounting state must fail closed instead of
                // admitting an entry beyond a configured bound.
                return;
            }
            // Evict least recently used (first in order)
            let oldest_key = self.order.remove(0);
            if let Some(old_entry) = self.entries.remove(&oldest_key) {
                self.current_size_bytes =
                    self.current_size_bytes.saturating_sub(old_entry.size_bytes);
            }
        }

        let Some(new_size) = self.current_size_bytes.checked_add(entry.size_bytes) else {
            return;
        };
        if new_size > self.max_size_bytes || self.entries.len() >= self.max_entries {
            return;
        }

        // Insert new entry only after all bounds have been rechecked.
        self.current_size_bytes = new_size;
        self.entries.insert(key.clone(), entry);
        self.order.push(key);
    }

    fn remove(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.current_size_bytes = self.current_size_bytes.saturating_sub(entry.size_bytes);
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
        }
    }

    fn remove_invalidation_digest(&mut self, invalidation_digest: Sha256Digest) {
        let mut retained_size = self.current_size_bytes;
        self.entries.retain(|key, entry| {
            if key.invalidation_digest == invalidation_digest {
                retained_size = retained_size.saturating_sub(entry.size_bytes);
                false
            } else {
                true
            }
        });
        self.order
            .retain(|key| key.invalidation_digest != invalidation_digest);
        self.current_size_bytes = retained_size;
    }

    fn remove_discovery_entries(&mut self) {
        let mut retained_size = self.current_size_bytes;
        self.entries.retain(|key, entry| {
            if matches!(key.binding, CacheEntryBinding::Discovery(_)) {
                retained_size = retained_size.saturating_sub(entry.size_bytes);
                false
            } else {
                true
            }
        });
        self.order
            .retain(|key| !matches!(key.binding, CacheEntryBinding::Discovery(_)));
        self.current_size_bytes = retained_size;
    }

    fn evict_expired(&mut self) {
        let mut retained_size = self.current_size_bytes;
        self.entries.retain(|_, entry| {
            if entry.is_expired() {
                retained_size = retained_size.saturating_sub(entry.size_bytes);
                false
            } else {
                true
            }
        });
        let entries = &self.entries;
        self.order.retain(|key| entries.contains_key(key));
        self.current_size_bytes = retained_size;
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.current_size_bytes = 0;
    }

    fn advance_fill_generation(&mut self) {
        if self.fill_generation != 0 {
            self.fill_generation = self.fill_generation.checked_add(1).unwrap_or(0);
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Cache statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of hits from cache-eligible partitioned or standalone lookups.
    pub hits: u64,
    /// Number of misses from cache-eligible partitioned or standalone lookups.
    ///
    /// Requests bypassed due to an incomplete partition or method policy are
    /// not counted as misses.
    pub misses: u64,
    /// Number of entries currently in cache.
    pub entries: usize,
    /// Current cache size in bytes.
    pub size_bytes: usize,
}

impl CacheStats {
    /// Returns the hit rate as a percentage.
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let hits = self.hits as f64;
        let total = hits + self.misses as f64;
        if total == 0.0 {
            0.0
        } else {
            (hits / total) * 100.0
        }
    }
}

/// Response caching middleware for MCP servers.
///
/// Caches eligible responses with configurable TTL and bounded LRU eviction.
///
/// Production contexts are isolated by opaque session-state identity, state
/// mutation revision, and complete verified authentication facts. An
/// incomplete partition fails closed. `tools/call` is additionally disabled by
/// default and requires an explicit per-tool allowlist entry via
/// [`Self::include_tools`].
pub struct ResponseCachingMiddleware {
    /// Process-local identity for request admission and hit bookkeeping.
    instance_id: u64,
    /// Monotonic final-discovery invalidation generation.
    ///
    /// It is included in every final discovery key and is advanced under the
    /// cache lock, preventing a response captured before invalidation from
    /// becoming observable afterwards.
    discovery_generation: AtomicU64,
    /// Cache storage.
    cache: Mutex<LruCache>,
    /// TTL for list operations.
    list_ttl: Duration,
    /// TTL for allowlisted call/get/read operations.
    call_ttl: Duration,
    /// Configuration for tools/list caching.
    tools_list_config: MethodCacheConfig,
    /// Configuration for resources/list caching.
    resources_list_config: MethodCacheConfig,
    /// Configuration for prompts/list caching.
    prompts_list_config: MethodCacheConfig,
    /// Configuration for tools/call caching.
    tools_call_config: ToolCallCacheConfig,
    /// Configuration for resources/read caching.
    resources_read_config: MethodCacheConfig,
    /// Configuration for prompts/get caching.
    prompts_get_config: MethodCacheConfig,
    /// Statistics tracking.
    stats: Mutex<CacheStats>,
}

impl std::fmt::Debug for ResponseCachingMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseCachingMiddleware")
            .field("instance_available", &(self.instance_id != 0))
            .field(
                "discovery_generation",
                &self.discovery_generation.load(Ordering::Acquire),
            )
            .field("list_ttl", &self.list_ttl)
            .field("call_ttl", &self.call_ttl)
            .finish_non_exhaustive()
    }
}

impl Default for ResponseCachingMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseCachingMiddleware {
    /// Creates response caching middleware with default bounds and TTLs.
    ///
    /// `tools/call` caching remains off until [`Self::include_tools`] is used.
    #[must_use]
    pub fn new() -> Self {
        Self {
            instance_id: next_cache_instance_id(),
            discovery_generation: AtomicU64::new(1),
            cache: Mutex::new(LruCache::new(
                1000,
                100 * 1024 * 1024,
                DEFAULT_MAX_ITEM_SIZE,
            )),
            list_ttl: Duration::from_secs(DEFAULT_LIST_TTL_SECS),
            call_ttl: Duration::from_secs(DEFAULT_CALL_TTL_SECS),
            tools_list_config: MethodCacheConfig {
                enabled: true,
                ttl_secs: DEFAULT_LIST_TTL_SECS,
            },
            resources_list_config: MethodCacheConfig {
                enabled: true,
                ttl_secs: DEFAULT_LIST_TTL_SECS,
            },
            prompts_list_config: MethodCacheConfig {
                enabled: true,
                ttl_secs: DEFAULT_LIST_TTL_SECS,
            },
            tools_call_config: ToolCallCacheConfig::default(),
            resources_read_config: MethodCacheConfig {
                enabled: true,
                ttl_secs: DEFAULT_CALL_TTL_SECS,
            },
            prompts_get_config: MethodCacheConfig {
                enabled: true,
                ttl_secs: DEFAULT_CALL_TTL_SECS,
            },
            stats: Mutex::new(CacheStats::default()),
        }
    }

    /// Sets the maximum number of cache entries (`0` disables storage).
    #[must_use]
    pub fn max_entries(self, max: usize) -> Self {
        let max_size = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.max_size_bytes
        };
        let max_item_size = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.max_item_size
        };
        Self {
            instance_id: next_cache_instance_id(),
            cache: Mutex::new(LruCache::new(max, max_size, max_item_size)),
            ..self
        }
    }

    /// Sets the maximum cache size in bytes (`0` disables storage).
    #[must_use]
    pub fn max_size_bytes(self, max: usize) -> Self {
        let max_entries = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.max_entries
        };
        let max_item_size = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.max_item_size
        };
        Self {
            instance_id: next_cache_instance_id(),
            cache: Mutex::new(LruCache::new(max_entries, max, max_item_size)),
            ..self
        }
    }

    /// Sets the maximum size per cache item in bytes (`0` disables storage).
    #[must_use]
    pub fn max_item_size(self, max: usize) -> Self {
        let max_entries = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.max_entries
        };
        let max_size = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.max_size_bytes
        };
        Self {
            instance_id: next_cache_instance_id(),
            cache: Mutex::new(LruCache::new(max_entries, max_size, max)),
            ..self
        }
    }

    /// Sets the TTL for list operations (tools/list, resources/list, prompts/list).
    #[must_use]
    pub fn list_ttl_secs(mut self, secs: u64) -> Self {
        self.list_ttl = Duration::from_secs(secs);
        self.tools_list_config.ttl_secs = secs;
        self.resources_list_config.ttl_secs = secs;
        self.prompts_list_config.ttl_secs = secs;
        self
    }

    /// Sets the TTL for read/get operations and explicitly allowlisted calls.
    #[must_use]
    pub fn call_ttl_secs(mut self, secs: u64) -> Self {
        self.call_ttl = Duration::from_secs(secs);
        self.tools_call_config.base.ttl_secs = secs;
        self.resources_read_config.ttl_secs = secs;
        self.prompts_get_config.ttl_secs = secs;
        self
    }

    /// Disables caching for tools/list.
    #[must_use]
    pub fn disable_tools_list(mut self) -> Self {
        self.tools_list_config.enabled = false;
        self
    }

    /// Disables caching for resources/list.
    #[must_use]
    pub fn disable_resources_list(mut self) -> Self {
        self.resources_list_config.enabled = false;
        self
    }

    /// Disables caching for prompts/list.
    #[must_use]
    pub fn disable_prompts_list(mut self) -> Self {
        self.prompts_list_config.enabled = false;
        self
    }

    /// Disables caching for tools/call.
    #[must_use]
    pub fn disable_tools_call(mut self) -> Self {
        self.tools_call_config.base.enabled = false;
        self
    }

    /// Disables caching for resources/read.
    #[must_use]
    pub fn disable_resources_read(mut self) -> Self {
        self.resources_read_config.enabled = false;
        self
    }

    /// Disables caching for prompts/get.
    #[must_use]
    pub fn disable_prompts_get(mut self) -> Self {
        self.prompts_get_config.enabled = false;
        self
    }

    /// Explicitly allowlists tools for `tools/call` caching.
    ///
    /// An empty list disables `tools/call` caching. Exclusions configured with
    /// [`Self::exclude_tools`] take precedence over this allowlist.
    #[must_use]
    pub fn include_tools(mut self, tools: Vec<String>) -> Self {
        self.tools_call_config.included_tools = tools;
        self
    }

    /// Excludes tools from `tools/call` caching, overriding the allowlist.
    #[must_use]
    pub fn exclude_tools(mut self, tools: Vec<String>) -> Self {
        self.tools_call_config.excluded_tools = tools;
        self
    }

    /// Returns current cache statistics.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        stats.entries = cache.len();
        stats.size_bytes = cache.current_size_bytes;
        stats
    }

    fn cache_entry_binding(&self, request: &JsonRpcRequest) -> Option<CacheEntryBinding> {
        if request.method != SERVER_DISCOVER_METHOD {
            return Some(CacheEntryBinding::Ordinary);
        }

        let era = discovery_request_protocol_era(request)?;
        if era != ProtocolEra::Modern2026 {
            return None;
        }
        let generation = self.discovery_generation.load(Ordering::Acquire);
        (generation != 0).then_some(CacheEntryBinding::Discovery(DiscoveryCacheBinding {
            era,
            generation,
        }))
    }

    fn cache_entry_binding_is_current(&self, binding: CacheEntryBinding) -> bool {
        match binding {
            CacheEntryBinding::Ordinary => true,
            CacheEntryBinding::Discovery(binding) => {
                binding.era == ProtocolEra::Modern2026
                    && binding.generation == self.discovery_generation.load(Ordering::Acquire)
            }
        }
    }

    /// Advances the discovery generation, permanently disabling discovery
    /// caching if the counter can no longer advance without wrapping.
    fn advance_discovery_generation(&self) {
        if self
            .discovery_generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                (generation != 0)
                    .then(|| generation.checked_add(1))
                    .flatten()
            })
            .is_err()
        {
            self.discovery_generation.store(0, Ordering::Release);
        }
    }

    /// Clears the entire cache and fences every outstanding response fill.
    pub fn clear(&self) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.advance_discovery_generation();
        cache.advance_fill_generation();
        cache.clear();
    }

    /// Invalidates every final discovery response and advances its cache
    /// generation. A response or lookup holding an older generation cannot
    /// reuse or repopulate the invalidated discovery state.
    pub fn invalidate_discovery(&self) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.advance_discovery_generation();
        cache.advance_fill_generation();
        cache.remove_discovery_entries();
    }

    /// Invalidates every session/auth partition and every cursor page for a
    /// method and semantic-parameter set.
    ///
    /// All in-flight fills are conservatively fenced, including unrelated
    /// methods. Unrelated retained entries remain available to fresh requests.
    pub fn invalidate(&self, method: &str, params: Option<&serde_json::Value>) {
        if method == SERVER_DISCOVER_METHOD {
            self.invalidate_discovery();
            return;
        }
        let Some(invalidation_digest) = CacheKey::try_invalidation_digest(method, params) else {
            return;
        };
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.advance_fill_generation();
        cache.remove_invalidation_digest(invalidation_digest);
    }

    /// Checks if a method should be cached.
    fn should_cache_method(&self, method: &str, params: Option<&serde_json::Value>) -> bool {
        match method {
            "server/discover" | "tools/list" => self.tools_list_config.enabled,
            "resources/list" | "resources/templates/list" => self.resources_list_config.enabled,
            "prompts/list" => self.prompts_list_config.enabled,
            "resources/read" => self.resources_read_config.enabled,
            "prompts/get" => self.prompts_get_config.enabled,
            "tools/call" => {
                if !self.tools_call_config.base.enabled {
                    return false;
                }
                // Extract tool name from params
                if let Some(params) = params {
                    if let Some(tool_name) = params.get("name").and_then(|v| v.as_str()) {
                        return self.tools_call_config.should_cache_tool(tool_name);
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Gets the TTL for a specific method.
    fn get_ttl(&self, method: &str) -> Duration {
        match method {
            "server/discover" | "tools/list" => {
                Duration::from_secs(self.tools_list_config.ttl_secs)
            }
            "resources/list" | "resources/templates/list" => {
                Duration::from_secs(self.resources_list_config.ttl_secs)
            }
            "prompts/list" => Duration::from_secs(self.prompts_list_config.ttl_secs),
            "tools/call" => Duration::from_secs(self.tools_call_config.base.ttl_secs),
            "resources/read" => Duration::from_secs(self.resources_read_config.ttl_secs),
            "prompts/get" => Duration::from_secs(self.prompts_get_config.ttl_secs),
            _ => self.call_ttl,
        }
    }

    fn protocol_cache_ttl(&self, method: &str) -> CacheTtl {
        CacheTtl::milliseconds(u64::try_from(self.get_ttl(method).as_millis()).unwrap_or(u64::MAX))
    }

    /// Normalizes modern protocol cache hints at the server boundary.
    ///
    /// A wire-valid final cache policy is an upstream result field, not a
    /// bounded local expiry. Preserve it byte-for-byte through serialization;
    /// the cache admission path can independently decline a value outside its
    /// runtime duration domain.
    fn apply_protocol_cache_hints(&self, method: &str, response: &mut serde_json::Value) {
        if method == SERVER_DISCOVER_METHOD {
            if is_final_discovery_result(response) {
                if !final_discovery_cache_hints_are_wire_valid(response)
                    && let Some(response) = response.as_object_mut()
                {
                    response.remove("ttlMs");
                    response.remove("cacheScope");
                }
                return;
            }
        }
        let Some(response) = response.as_object_mut() else {
            return;
        };
        if !method_requires_protocol_cache_hints(method)
            || response
                .get("resultType")
                .and_then(serde_json::Value::as_str)
                != Some("complete")
        {
            // Cache hints are valid only on the explicitly cacheable modern
            // complete-result branches. Do not preserve a handler- or peer-
            // supplied lookalike on input-required, task, legacy, or
            // non-cacheable method results.
            response.remove("ttlMs");
            response.remove("cacheScope");
            return;
        }

        let has_wire_valid_cache_hints = response
            .get("ttlMs")
            .cloned()
            .and_then(|ttl| serde_json::from_value::<CacheTtl>(ttl).ok())
            .is_some()
            && matches!(
                response
                    .get("cacheScope")
                    .and_then(serde_json::Value::as_str),
                Some("private" | "public")
            );
        if has_wire_valid_cache_hints {
            return;
        }

        let ttl = self.protocol_cache_ttl(method);
        response.insert(
            "ttlMs".to_owned(),
            serde_json::to_value(ttl).expect("cache TTL serializes to JSON"),
        );
        response.insert(
            "cacheScope".to_owned(),
            serde_json::Value::String("private".to_owned()),
        );
    }

    fn record_hit(&self) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stats.hits = stats.hits.saturating_add(1);
    }

    fn record_miss(&self) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stats.misses = stats.misses.saturating_add(1);
    }
}

impl Middleware for ResponseCachingMiddleware {
    fn on_request(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        if self.instance_id == 0 {
            return Ok(MiddlewareDecision::Continue);
        }
        // Check if this method should be cached
        if !self.should_cache_method(&request.method, request.params.as_ref()) {
            return Ok(MiddlewareDecision::Continue);
        }
        if request_carries_uncacheable_continuation(request.params.as_ref()) {
            return Ok(MiddlewareDecision::Continue);
        }
        let Some(binding) = self.cache_entry_binding(request) else {
            return Ok(MiddlewareDecision::Continue);
        };

        let Some(partition_digest) = context_cache_partition(ctx, CachePartitionPhase::Request)
        else {
            return Ok(MiddlewareDecision::Continue);
        };

        // Try to get cached response
        let Some(key) = CacheKey::try_new_partitioned(
            &request.method,
            request.params.as_ref(),
            partition_digest,
            binding,
        ) else {
            return Ok(MiddlewareDecision::Continue);
        };
        let (encoded, fill_generation) = {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let binding_revision = match binding {
                CacheEntryBinding::Ordinary => 0,
                CacheEntryBinding::Discovery(binding) => binding.generation,
            };
            if !self.cache_entry_binding_is_current(binding)
                || !ctx.begin_response_cache_admission(
                    self.instance_id,
                    cache.fill_generation,
                    binding_revision,
                )
            {
                return Ok(MiddlewareDecision::Continue);
            }
            (cache.get_encoded(&key), cache.fill_generation)
        };

        if let Some(encoded) = encoded {
            if let Some(value) = decode_cached_json(&encoded) {
                // Invalidation may win after lookup releases the cache lock
                // or while the retained bytes are decoded. Admit delivery
                // under the same lock used to advance the fill epoch.
                let cache = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if cache.fill_generation != fill_generation
                    || !self.cache_entry_binding_is_current(binding)
                {
                    self.record_miss();
                    return Ok(MiddlewareDecision::Continue);
                }
                // Session state can change while this request waits for the
                // cache mutex or decodes a cached payload. Revalidate after
                // both operations so a hit linearizes against the admitted
                // revision instead of serving an entry made stale before the
                // lookup completed. The final liveness check applies the same
                // completion rule to cancellation and request-lease closure.
                if !context_cache_commit_is_admissible(ctx) {
                    return Ok(MiddlewareDecision::Continue);
                }
                if !ctx.mark_response_cache_hit(self.instance_id) {
                    self.record_miss();
                    return Ok(MiddlewareDecision::Continue);
                }
                drop(cache);
                self.record_hit();
                return Ok(MiddlewareDecision::Respond(value));
            }
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.remove(&key);
        }

        self.record_miss();
        Ok(MiddlewareDecision::Continue)
    }

    fn on_response(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
        mut response: serde_json::Value,
    ) -> McpResult<serde_json::Value> {
        if self.instance_id == 0 {
            return Ok(response);
        }
        self.apply_protocol_cache_hints(&request.method, &mut response);
        let final_discovery_policy = (request.method == SERVER_DISCOVER_METHOD
            && is_final_discovery_result(&response))
        .then(|| final_discovery_cache_policy(&response))
        .flatten();
        if request.method == SERVER_DISCOVER_METHOD
            && is_final_discovery_result(&response)
            && final_discovery_policy.is_none()
        {
            return Ok(response);
        }
        let is_final_complete = method_requires_protocol_cache_hints(&request.method)
            && response
                .get("resultType")
                .and_then(serde_json::Value::as_str)
                == Some("complete");
        let final_complete_policy = is_final_complete
            .then(|| final_complete_cache_policy(&response))
            .flatten();
        // A wire-valid arbitrary-width TTL is still delivered, but cannot be
        // represented by this process's bounded expiry clock. Never fall back
        // to a configured local TTL: that would create a cache entry whose
        // expiry has no relationship to the final result's wire policy.
        if is_final_complete && final_complete_policy.is_none() {
            return Ok(response);
        }
        // Only cache if this method is cacheable
        if !self.should_cache_method(&request.method, request.params.as_ref()) {
            return Ok(response);
        }
        if request_carries_uncacheable_continuation(request.params.as_ref())
            || !response_is_cacheable_complete(&response)
        {
            return Ok(response);
        }
        if ctx.response_was_cache_hit(self.instance_id) {
            return Ok(response);
        }
        let Some((fill_generation, binding_revision)) =
            ctx.response_cache_admission(self.instance_id)
        else {
            return Ok(response);
        };
        let binding = if request.method == SERVER_DISCOVER_METHOD {
            let Some(ProtocolEra::Modern2026) = discovery_request_protocol_era(request) else {
                return Ok(response);
            };
            if binding_revision == 0 {
                return Ok(response);
            }
            CacheEntryBinding::Discovery(DiscoveryCacheBinding {
                era: ProtocolEra::Modern2026,
                generation: binding_revision,
            })
        } else if binding_revision == 0 {
            CacheEntryBinding::Ordinary
        } else {
            return Ok(response);
        };

        let Some(partition_digest) = context_cache_partition(ctx, CachePartitionPhase::Response)
        else {
            return Ok(response);
        };

        // Store in cache
        let Some(key) = CacheKey::try_new_partitioned(
            &request.method,
            request.params.as_ref(),
            partition_digest,
            binding,
        ) else {
            return Ok(response);
        };
        let ttl = match final_discovery_policy {
            Some(FinalDiscoveryCachePolicy::Private(ttl)) => ttl,
            Some(FinalDiscoveryCachePolicy::Public) => return Ok(response),
            None => match final_complete_policy {
                Some(FinalCompleteCachePolicy::Private(ttl)) => ttl,
                Some(FinalCompleteCachePolicy::Public) => return Ok(response),
                None => self.get_ttl(&request.method),
            },
        };

        let admission_limit = {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.max_item_size.min(
                cache
                    .max_size_bytes
                    .saturating_sub(CACHE_ENTRY_METADATA_BYTES),
            )
        };
        let Some(encoded) = encode_json_bounded(&response, admission_limit) else {
            return Ok(response);
        };
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Encoding and waiting for the cache mutex may both be non-trivial for
        // a response near the configured limits. Revalidate at the commit
        // boundary so cancellation, lease closure, or session mutation cannot
        // populate the cache after winning either race.
        if !context_cache_commit_is_admissible(ctx) {
            return Ok(response);
        }
        if cache.fill_generation != fill_generation
            || fill_generation == 0
            || !self.cache_entry_binding_is_current(binding)
        {
            return Ok(response);
        }
        cache.insert_encoded(key, encoded, ttl);

        Ok(response)
    }

    fn on_error(&self, _ctx: &McpContext, _request: &JsonRpcRequest, error: McpError) -> McpError {
        // Don't cache errors, just pass them through
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;
    use fastmcp_core::{AuthContext, SessionState};

    fn maximum_geometric_growth_events(max_bytes: usize) -> usize {
        if max_bytes == 0 {
            return 0;
        }

        let mut capacity = CACHE_BYTES_GROWTH_CHUNK.min(max_bytes);
        let mut events = 1_usize;
        while capacity < max_bytes {
            capacity = capacity.checked_mul(2).unwrap_or(max_bytes).min(max_bytes);
            events = events.saturating_add(1);
        }
        events
    }

    fn test_context() -> McpContext {
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        assert!(ctx.commit_anonymous_auth());
        ctx
    }

    fn partitioned_context(state: &SessionState, request_id: u64, auth: AuthContext) -> McpContext {
        McpContext::with_state(Cx::for_testing(), request_id, state.clone()).with_auth(auth)
    }

    fn anonymous_partitioned_context(state: &SessionState, request_id: u64) -> McpContext {
        let ctx = McpContext::with_state(Cx::for_testing(), request_id, state.clone());
        assert!(ctx.commit_anonymous_auth());
        assert!(ctx.auth().is_none());
        ctx
    }

    fn test_request(method: &str, params: Option<serde_json::Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            method: method.to_string(),
            params,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        }
    }

    fn final_discovery_request(protocol_version: &str) -> JsonRpcRequest {
        test_request(
            SERVER_DISCOVER_METHOD,
            Some(serde_json::json!({
                "_meta": {
                    FINAL_PROTOCOL_VERSION_META_KEY: protocol_version,
                },
            })),
        )
    }

    fn final_discovery_response(ttl_ms: u64, cache_scope: &str) -> serde_json::Value {
        serde_json::json!({
            "supportedVersions": [FINAL_PROTOCOL_VERSION],
            "capabilities": {},
            "ttlMs": ttl_ms,
            "cacheScope": cache_scope,
        })
    }

    // ========================================
    // LruCache tests
    // ========================================

    #[test]
    fn test_lru_cache_basic_operations() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);

        let key = CacheKey::new("test", None);
        let value = serde_json::json!({"result": "cached"});

        // Insert and retrieve
        cache.insert(key.clone(), value.clone(), Duration::from_secs(60));
        let retrieved = cache.get_value(&key);
        assert_eq!(retrieved, Some(value));
    }

    #[test]
    fn test_lru_cache_expiration() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);

        let key = CacheKey::new("test", None);
        let value = serde_json::json!({"result": "cached"});

        // Insert with very short TTL
        cache.insert(key.clone(), value, Duration::from_millis(1));

        // Wait for expiration
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Should be expired
        assert!(cache.get_value(&key).is_none());
    }

    #[test]
    fn test_lru_cache_eviction() {
        let mut cache = LruCache::new(2, 1024 * 1024, 1024);

        let key1 = CacheKey::new("test1", None);
        let key2 = CacheKey::new("test2", None);
        let key3 = CacheKey::new("test3", None);

        cache.insert(
            key1.clone(),
            serde_json::json!("v1"),
            Duration::from_secs(60),
        );
        cache.insert(
            key2.clone(),
            serde_json::json!("v2"),
            Duration::from_secs(60),
        );

        // Should evict key1 (LRU)
        cache.insert(
            key3.clone(),
            serde_json::json!("v3"),
            Duration::from_secs(60),
        );

        assert!(cache.get_value(&key1).is_none());
        assert!(cache.get_value(&key2).is_some());
        assert!(cache.get_value(&key3).is_some());
    }

    #[test]
    fn test_lru_cache_size_limit() {
        let mut cache = LruCache::new(100, CACHE_ENTRY_METADATA_BYTES + 16, 1024);

        let key1 = CacheKey::new("test1", None);
        let key2 = CacheKey::new("test2", None);

        // First entry should fit
        cache.insert(
            key1.clone(),
            serde_json::json!("short"),
            Duration::from_secs(60),
        );
        assert_eq!(cache.len(), 1);

        // Second entry should cause eviction
        cache.insert(
            key2.clone(),
            serde_json::json!("another"),
            Duration::from_secs(60),
        );
        assert!(cache.get_value(&key1).is_none());
        assert_eq!(cache.get_value(&key2), Some(serde_json::json!("another")));
    }

    #[test]
    fn test_lru_cache_oversized_item_rejected() {
        let mut cache = LruCache::new(10, 1024 * 1024, 10); // max 10 bytes per item

        let key = CacheKey::new("test", None);
        let large_value = serde_json::json!({"data": "this is much longer than 10 bytes"});

        cache.insert(key.clone(), large_value, Duration::from_secs(60));

        // Should not be stored
        assert!(cache.get_value(&key).is_none());
    }

    #[test]
    fn lru_cache_zero_entry_limit_rejects_every_insert() {
        let mut cache = LruCache::new(0, 1024, 1024);
        let key = CacheKey::new("test", None);

        cache.insert(
            key.clone(),
            serde_json::json!("value"),
            Duration::from_secs(60),
        );

        assert!(cache.get_value(&key).is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_size_bytes, 0);
    }

    #[test]
    fn lru_cache_zero_total_size_rejects_every_insert() {
        let mut cache = LruCache::new(10, 0, 1024);
        let key = CacheKey::new("test", None);

        cache.insert(
            key.clone(),
            serde_json::json!("value"),
            Duration::from_secs(60),
        );

        assert!(cache.get_value(&key).is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_size_bytes, 0);
    }

    #[test]
    fn lru_cache_item_larger_than_total_capacity_is_rejected() {
        let value = serde_json::json!("larger than capacity");
        let value_size = value.to_string().len();
        let mut cache = LruCache::new(10, value_size - 1, value_size + 100);
        let key = CacheKey::new("test", None);

        cache.insert(key.clone(), value, Duration::from_secs(60));

        assert!(cache.get_value(&key).is_none());
        assert_eq!(cache.current_size_bytes, 0);
    }

    #[test]
    fn lru_cache_rejected_replacement_preserves_existing_entry_and_accounting() {
        let mut cache = LruCache::new(10, 1024, 12);
        let key = CacheKey::new("test", None);
        let original = serde_json::json!("small");

        cache.insert(key.clone(), original.clone(), Duration::from_secs(60));
        let original_size = cache.current_size_bytes;

        cache.insert(
            key.clone(),
            serde_json::json!("this replacement is too large"),
            Duration::from_secs(60),
        );

        assert_eq!(cache.get_value(&key), Some(original));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.current_size_bytes, original_size);
    }

    #[test]
    fn lru_cache_unrepresentable_ttl_is_rejected_without_mutation() {
        let mut cache = LruCache::new(10, 1024, 1024);
        let key = CacheKey::new("test", None);

        cache.insert(key.clone(), serde_json::json!("value"), Duration::MAX);

        assert!(cache.get_value(&key).is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_size_bytes, 0);
    }

    #[test]
    fn lru_cache_zero_ttl_is_never_observable() {
        let mut cache = LruCache::new(10, 1024, 1024);
        let key = CacheKey::new("test", None);

        cache.insert(key.clone(), serde_json::json!("value"), Duration::ZERO);

        assert!(cache.get_value(&key).is_none());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_size_bytes, 0);
    }

    // ========================================
    // ResponseCachingMiddleware tests
    // ========================================

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FillInvalidation {
        None,
        Method,
        Clear,
        Discovery,
    }

    fn assert_response_cache_fill_invalidation(invalidation: FillInvalidation) {
        for method in [
            "tools/list",
            "resources/list",
            "resources/templates/list",
            "prompts/list",
            "resources/read",
            "prompts/get",
            "tools/call",
            SERVER_DISCOVER_METHOD,
        ] {
            let middleware = ResponseCachingMiddleware::new()
                .include_tools(vec!["deterministic".to_owned()]);
            let request = match method {
                SERVER_DISCOVER_METHOD => final_discovery_request(FINAL_PROTOCOL_VERSION),
                "resources/read" => test_request(
                    method,
                    Some(serde_json::json!({"uri": "file:///generation-test"})),
                ),
                "prompts/get" | "tools/call" => test_request(
                    method,
                    Some(serde_json::json!({"name": "deterministic"})),
                ),
                _ => test_request(method, Some(serde_json::json!({"cursor": "page-two"}))),
            };
            let mut response = if method == SERVER_DISCOVER_METHOD {
                final_discovery_response(30_000, "private")
            } else if matches!(method, "prompts/get" | "tools/call") {
                serde_json::json!({"resultType": "complete"})
            } else {
                serde_json::json!({
                    "resultType": "complete",
                    "ttlMs": 30_000,
                    "cacheScope": "private",
                })
            };
            let member = match method {
                "tools/list" => Some("tools"),
                "resources/list" => Some("resources"),
                "resources/templates/list" => Some("resourceTemplates"),
                "prompts/list" => Some("prompts"),
                "resources/read" => Some("contents"),
                "prompts/get" => Some("messages"),
                "tools/call" => Some("content"),
                _ => None,
            };
            if let Some(member) = member {
                response[member] = serde_json::json!([]);
            }
            response["fixtureRevision"] = serde_json::json!("before-invalidation");

            let request_ctx = test_context();
            let handler_ctx = request_ctx.clone();
            assert!(matches!(
                middleware.on_request(&request_ctx, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            let captured = request_ctx
                .response_cache_admission(middleware.instance_id)
                .expect("cacheable request captures its generation before the handler");

            match invalidation {
                FillInvalidation::None => {}
                FillInvalidation::Method => {
                    middleware.invalidate(method, request.params.as_ref());
                }
                FillInvalidation::Clear => middleware.clear(),
                FillInvalidation::Discovery => middleware.invalidate_discovery(),
            }

            assert_eq!(
                middleware
                    .on_response(&handler_ctx, &request, response.clone())
                    .unwrap(),
                response,
                "{method}: cache admission never changes the handler's delivered value"
            );
            assert_eq!(
                handler_ctx.response_cache_admission(middleware.instance_id),
                Some(captured),
                "response completion must preserve the captured generation"
            );

            let lookup = middleware.on_request(&test_context(), &request).unwrap();
            if invalidation == FillInvalidation::None {
                assert_eq!(middleware.stats().entries, 1);
                assert!(matches!(
                    lookup,
                    MiddlewareDecision::Respond(value) if value == response
                ));
                continue;
            }
            assert_eq!(middleware.stats().entries, 0, "{method}: late fill rejected");
            assert_eq!(middleware.stats().size_bytes, 0);
            assert!(matches!(lookup, MiddlewareDecision::Continue));
            assert!(matches!(
                middleware.on_request(&handler_ctx, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            assert_eq!(
                handler_ctx.response_cache_admission(middleware.instance_id),
                Some(captured),
                "re-entering request middleware cannot refresh an old admission"
            );

            let successor = test_context();
            assert!(matches!(
                middleware.on_request(&successor, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            let mut fresh = response.clone();
            fresh["fixtureRevision"] = serde_json::json!("after-invalidation");
            middleware
                .on_response(&successor, &request, fresh.clone())
                .unwrap();
            assert_eq!(middleware.stats().entries, 1);
            let before_late_retry = middleware.stats();
            middleware
                .on_response(&handler_ctx, &request, response)
                .unwrap();
            assert_eq!(middleware.stats(), before_late_retry);
            assert!(matches!(
                middleware.on_request(&test_context(), &request).unwrap(),
                MiddlewareDecision::Respond(value) if value == fresh
            ));
        }
    }

    #[test]
    fn response_cache_unchanged_generation_allows_fill() {
        assert_response_cache_fill_invalidation(FillInvalidation::None);
    }

    #[test]
    fn response_cache_invalidation_rejects_late_fill() {
        assert_response_cache_fill_invalidation(FillInvalidation::Method);
    }

    #[test]
    fn response_cache_clear_rejects_late_fill() {
        assert_response_cache_fill_invalidation(FillInvalidation::Clear);
    }

    #[test]
    fn response_cache_discovery_invalidation_rejects_late_fill() {
        assert_response_cache_fill_invalidation(FillInvalidation::Discovery);
    }

    #[test]
    fn response_cache_invalidation_preserves_unrelated_entries_and_instances() {
        let first = ResponseCachingMiddleware::new();
        let second = ResponseCachingMiddleware::new();
        let request = test_request("tools/list", None);
        let unrelated = test_request("prompts/list", None);
        let retained = serde_json::json!({"prompts": []});
        let fill = test_context();
        assert!(matches!(
            first.on_request(&fill, &unrelated).unwrap(),
            MiddlewareDecision::Continue
        ));
        first
            .on_response(&fill, &unrelated, retained.clone())
            .unwrap();

        let shared = test_context();
        assert!(matches!(
            first.on_request(&shared, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        assert!(matches!(
            second.on_request(&shared, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        first.invalidate("tools/list", None);
        let response = serde_json::json!({"tools": []});
        first
            .on_response(&shared, &request, response.clone())
            .unwrap();
        second
            .on_response(&shared.clone(), &request, response.clone())
            .unwrap();

        assert_eq!(first.stats().entries, 1);
        assert_eq!(second.stats().entries, 1);
        assert!(matches!(
            first.on_request(&test_context(), &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        assert!(matches!(
            first.on_request(&test_context(), &unrelated).unwrap(),
            MiddlewareDecision::Respond(value) if value == retained
        ));
        assert!(matches!(
            second.on_request(&test_context(), &request).unwrap(),
            MiddlewareDecision::Respond(value) if value == response
        ));
    }

    #[test]
    fn response_cache_requires_request_admission() {
        let middleware = ResponseCachingMiddleware::new();
        let request = test_request("tools/list", None);
        let response = serde_json::json!({"tools": []});
        let ctx = test_context();
        let before = middleware.stats();
        assert_eq!(
            middleware
                .on_response(&ctx, &request, response.clone())
                .unwrap(),
            response
        );
        assert_eq!(middleware.stats(), before);
        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&ctx, &request, response.clone())
            .unwrap();
        assert!(matches!(
            middleware.on_request(&test_context(), &request).unwrap(),
            MiddlewareDecision::Respond(value) if value == response
        ));
    }

    #[test]
    fn response_cache_generation_exhaustion_stays_disabled() {
        let middleware = ResponseCachingMiddleware::new();
        middleware.cache.lock().unwrap().fill_generation = u64::MAX;
        let request = test_request("tools/list", None);
        let old = test_context();
        assert!(matches!(
            middleware.on_request(&old, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware.clear();
        assert_eq!(middleware.cache.lock().unwrap().fill_generation, 0);
        middleware
            .on_response(&old, &request, serde_json::json!({"tools": []}))
            .unwrap();
        for _ in 0..2 {
            let fresh = test_context();
            middleware.invalidate("tools/list", None);
            assert!(matches!(
                middleware.on_request(&fresh, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            assert_eq!(fresh.response_cache_admission(middleware.instance_id), None);
            middleware
                .on_response(&fresh, &request, serde_json::json!({"tools": []}))
                .unwrap();
            assert_eq!(middleware.stats().entries, 0);
            assert_eq!(middleware.cache.lock().unwrap().fill_generation, 0);
        }
    }

    #[test]
    fn response_cache_capacity_reconfiguration_rejects_old_admission() {
        let middleware = ResponseCachingMiddleware::new();
        let request = test_request("tools/list", None);
        let old = test_context();
        assert!(matches!(
            middleware.on_request(&old, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let middleware = middleware.max_entries(10);
        middleware
            .on_response(&old, &request, serde_json::json!({"tools": ["old"]}))
            .unwrap();
        assert_eq!(middleware.stats().entries, 0);
        let fresh = test_context();
        assert!(matches!(
            middleware.on_request(&fresh, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let response = serde_json::json!({"tools": ["fresh"]});
        middleware
            .on_response(&fresh, &request, response.clone())
            .unwrap();
        assert!(matches!(
            middleware.on_request(&test_context(), &request).unwrap(),
            MiddlewareDecision::Respond(value) if value == response
        ));
    }

    #[test]
    fn test_caching_middleware_caches_tools_list() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = test_request("tools/list", None);

        // First request: miss, continue
        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));

        // Simulate response
        let response = serde_json::json!({"tools": []});
        middleware
            .on_response(&ctx, &request, response.clone())
            .unwrap();

        // Second request: hit, respond from cache
        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(
            matches!(decision, MiddlewareDecision::Respond(_)),
            "Expected cache hit"
        );
        let MiddlewareDecision::Respond(cached) = decision else {
            return;
        };
        assert_eq!(cached, response);

        // Check stats
        let stats = middleware.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn cache_hit_response_does_not_refresh_absolute_expiration() {
        let middleware = ResponseCachingMiddleware::new().list_ttl_secs(60);
        let ctx = test_context();
        let request = test_request("tools/list", None);
        let response = serde_json::json!({"tools": []});

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&ctx, &request, response.clone())
            .unwrap();
        let expires_before_hit = middleware
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .values()
            .next()
            .expect("cached entry")
            .expires_at;

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Respond(_)
        ));
        middleware.on_response(&ctx, &request, response).unwrap();

        let expires_after_hit = middleware
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .values()
            .next()
            .expect("cache hit must retain the original entry")
            .expires_at;
        assert_eq!(expires_after_hit, expires_before_hit);
    }

    #[test]
    fn downstream_cache_hit_does_not_prevent_upstream_cache_warming() {
        let upstream = ResponseCachingMiddleware::new();
        let downstream = ResponseCachingMiddleware::new();
        let request = test_request("tools/list", None);
        let response = serde_json::json!({"tools": ["warm"]});

        let prewarm = test_context();
        assert!(matches!(
            downstream.on_request(&prewarm, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        downstream
            .on_response(&prewarm, &request, response.clone())
            .unwrap();

        let shared_request = test_context();
        assert!(matches!(
            upstream.on_request(&shared_request, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let MiddlewareDecision::Respond(cached) =
            downstream.on_request(&shared_request, &request).unwrap()
        else {
            panic!("downstream cache was not prewarmed");
        };
        downstream
            .on_response(&shared_request, &request, cached.clone())
            .unwrap();
        upstream
            .on_response(&shared_request, &request, cached)
            .unwrap();

        assert!(matches!(
            upstream.on_request(&test_context(), &request).unwrap(),
            MiddlewareDecision::Respond(value) if value == response
        ));
    }

    #[test]
    fn test_caching_middleware_skips_non_cacheable_methods() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = test_request("initialize", None);

        // Should continue (not cached)
        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));

        // Even after response, next request should not hit cache
        middleware
            .on_response(&ctx, &request, serde_json::json!({}))
            .unwrap();

        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));
    }

    #[test]
    fn test_caching_middleware_different_params_different_keys() {
        let middleware = ResponseCachingMiddleware::new()
            .include_tools(vec!["tool_a".to_string(), "tool_b".to_string()]);
        let ctx = test_context();

        let request1 = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "tool_a", "arguments": {}})),
        );
        let request2 = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "tool_b", "arguments": {}})),
        );

        // Cache response for request1
        middleware.on_request(&ctx, &request1).unwrap();
        let response1 = serde_json::json!({"result": "a"});
        middleware
            .on_response(&ctx, &request1, response1.clone())
            .unwrap();

        // Request2 should not hit cache
        let decision = middleware.on_request(&ctx, &request2).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));

        // Request1 should hit cache
        let decision = middleware.on_request(&ctx, &request1).unwrap();
        assert!(
            matches!(decision, MiddlewareDecision::Respond(_)),
            "Expected cache hit"
        );
        let MiddlewareDecision::Respond(cached) = decision else {
            return;
        };
        assert_eq!(cached, response1);
    }

    #[test]
    fn test_caching_middleware_tool_exclusion() {
        let middleware = ResponseCachingMiddleware::new()
            .include_tools(vec![
                "excluded_tool".to_string(),
                "included_tool".to_string(),
            ])
            .exclude_tools(vec!["excluded_tool".to_string()]);
        let ctx = test_context();

        let excluded_request = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "excluded_tool", "arguments": {}})),
        );
        let included_request = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "included_tool", "arguments": {}})),
        );

        // Excluded tool should not be cached
        middleware.on_request(&ctx, &excluded_request).unwrap();
        middleware
            .on_response(&ctx, &excluded_request, serde_json::json!({}))
            .unwrap();

        let decision = middleware.on_request(&ctx, &excluded_request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));

        // Included tool should be cached
        middleware.on_request(&ctx, &included_request).unwrap();
        let response = serde_json::json!({"result": "included"});
        middleware
            .on_response(&ctx, &included_request, response.clone())
            .unwrap();

        let decision = middleware.on_request(&ctx, &included_request).unwrap();
        assert!(
            matches!(decision, MiddlewareDecision::Respond(_)),
            "Expected cache hit for included tool"
        );
        let MiddlewareDecision::Respond(cached) = decision else {
            return;
        };
        assert_eq!(cached, response);
    }

    #[test]
    fn test_caching_middleware_disable_method() {
        let middleware = ResponseCachingMiddleware::new().disable_tools_list();
        let ctx = test_context();
        let request = test_request("tools/list", None);

        // Should not cache
        middleware.on_request(&ctx, &request).unwrap();
        middleware
            .on_response(&ctx, &request, serde_json::json!({}))
            .unwrap();

        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));
    }

    #[test]
    fn tools_call_is_not_cached_without_an_explicit_allowlist() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "stateful_tool", "arguments": {}})),
        );

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(
                &ctx,
                &request,
                serde_json::json!({"result": "must not be stored"}),
            )
            .unwrap();

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        assert_eq!(middleware.stats().entries, 0);
    }

    #[test]
    fn explicitly_allowlisted_tool_can_cache_in_unpartitioned_context() {
        let middleware =
            ResponseCachingMiddleware::new().include_tools(vec!["pure_tool".to_string()]);
        let ctx = test_context();
        let request = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "pure_tool", "arguments": {"x": 1}})),
        );
        let response = serde_json::json!({"result": 2});

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&ctx, &request, response.clone())
            .unwrap();

        let MiddlewareDecision::Respond(cached) = middleware.on_request(&ctx, &request).unwrap()
        else {
            panic!("explicitly allowlisted tool did not produce a cache hit");
        };
        assert_eq!(cached, response);
    }

    #[test]
    fn stateless_cache_partitions_anonymous_and_authenticated_requests() {
        let middleware = ResponseCachingMiddleware::new();
        let anonymous_ctx = test_context();
        let authenticated_ctx = McpContext::new(Cx::for_testing(), 2)
            .with_auth(AuthContext::with_subject("principal-a"));
        let request = test_request("tools/list", None);
        let public_response = serde_json::json!({"tools": ["public"]});
        let private_response = serde_json::json!({"tools": ["private"]});
        assert!(authenticated_ctx.auth().is_some());

        assert!(matches!(
            middleware.on_request(&anonymous_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&anonymous_ctx, &request, public_response.clone())
            .unwrap();

        // An authenticated request must not read an anonymous entry.
        assert!(matches!(
            middleware.on_request(&authenticated_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));

        // Its response receives a separate stateless authorization partition.
        middleware
            .on_response(&authenticated_ctx, &request, private_response.clone())
            .unwrap();
        let MiddlewareDecision::Respond(cached) =
            middleware.on_request(&anonymous_ctx, &request).unwrap()
        else {
            panic!("authenticated response unexpectedly replaced the public entry");
        };
        assert_eq!(cached, public_response);

        let authenticated_retry = McpContext::new(Cx::for_testing(), 3)
            .with_auth(AuthContext::with_subject("principal-a"));
        let MiddlewareDecision::Respond(cached) = middleware
            .on_request(&authenticated_retry, &request)
            .unwrap()
        else {
            panic!("same authenticated stateless partition did not hit");
        };
        assert_eq!(cached, private_response);
        assert_eq!(middleware.stats().entries, 2);
    }

    #[test]
    fn stateless_cache_partitions_identical_auth_facts_by_session_owner() {
        let middleware = ResponseCachingMiddleware::new();
        let first_auth = AuthContext::with_subject("same-display")
            .with_session_owner(Sha256Digest::from_bytes([1; 32]));
        let second_auth = AuthContext::with_subject("same-display")
            .with_session_owner(Sha256Digest::from_bytes([2; 32]));
        let first_ctx = McpContext::new(Cx::for_testing(), 1).with_auth(first_auth.clone());
        let second_ctx = McpContext::new(Cx::for_testing(), 2).with_auth(second_auth);
        let request = test_request("tools/list", None);
        let first_response = serde_json::json!({"tools": ["owner-one"]});

        assert!(matches!(
            middleware.on_request(&first_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&first_ctx, &request, first_response.clone())
            .unwrap();
        assert!(matches!(
            middleware.on_request(&second_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));

        let same_owner_retry = McpContext::new(Cx::for_testing(), 3).with_auth(first_auth);
        let MiddlewareDecision::Respond(cached) =
            middleware.on_request(&same_owner_retry, &request).unwrap()
        else {
            panic!("the same stateless owner partition did not hit");
        };
        assert_eq!(cached, first_response);
    }

    #[test]
    fn stateless_cache_frames_absent_owner_separately_from_zero_owner() {
        let middleware = ResponseCachingMiddleware::new();
        let ownerless_auth = AuthContext::with_subject("same-display");
        let zero_owner_auth = ownerless_auth
            .clone()
            .with_session_owner(Sha256Digest::from_bytes([0; 32]));
        let ownerless_ctx = McpContext::new(Cx::for_testing(), 1).with_auth(ownerless_auth);
        let zero_owner_ctx = McpContext::new(Cx::for_testing(), 2).with_auth(zero_owner_auth);
        let request = test_request("tools/list", None);
        let ownerless_response = serde_json::json!({"tools": ["ownerless"]});

        assert!(matches!(
            middleware.on_request(&ownerless_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&ownerless_ctx, &request, ownerless_response.clone())
            .unwrap();

        assert!(matches!(
            middleware.on_request(&zero_owner_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let MiddlewareDecision::Respond(cached) =
            middleware.on_request(&ownerless_ctx, &request).unwrap()
        else {
            panic!("the ownerless cache partition was no longer retrievable");
        };
        assert_eq!(cached, ownerless_response);
        assert_eq!(middleware.stats().entries, 1);
    }

    #[test]
    fn session_without_committed_auth_bypasses_lookup_and_storage() {
        let middleware = ResponseCachingMiddleware::new();
        let anonymous_ctx = test_context();
        let session_ctx = McpContext::with_state(Cx::for_testing(), 2, SessionState::new());
        let request = test_request("resources/list", None);
        let public_response = serde_json::json!({"resources": ["public"]});
        assert!(session_ctx.has_session_state());

        assert!(matches!(
            middleware.on_request(&anonymous_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&anonymous_ctx, &request, public_response.clone())
            .unwrap();

        // A session-backed request must not read an unpartitioned entry.
        assert!(matches!(
            middleware.on_request(&session_ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));

        // Its response must not overwrite the unpartitioned entry either.
        middleware
            .on_response(
                &session_ctx,
                &request,
                serde_json::json!({"resources": ["session-private"]}),
            )
            .unwrap();
        let MiddlewareDecision::Respond(cached) =
            middleware.on_request(&anonymous_ctx, &request).unwrap()
        else {
            panic!("session response unexpectedly replaced the public entry");
        };
        assert_eq!(cached, public_response);
        assert_eq!(middleware.stats().entries, 1);
    }

    #[test]
    fn allowlisted_tool_still_bypasses_uncommitted_context_partitions() {
        let middleware =
            ResponseCachingMiddleware::new().include_tools(vec!["pure_tool".to_string()]);
        let stateless_ctx = McpContext::new(Cx::for_testing(), 1);
        let session_ctx = McpContext::with_state(Cx::for_testing(), 2, SessionState::new());
        let request = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "pure_tool", "arguments": {}})),
        );

        for ctx in [&stateless_ctx, &session_ctx] {
            assert!(matches!(
                middleware.on_request(ctx, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            middleware
                .on_response(ctx, &request, serde_json::json!({"result": "private"}))
                .unwrap();
        }

        assert_eq!(middleware.stats().entries, 0);
        assert!(matches!(
            middleware.on_request(&test_context(), &request).unwrap(),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn production_context_caches_within_one_session_and_auth_partition() {
        let middleware = ResponseCachingMiddleware::new();
        let state = SessionState::new();
        let first = anonymous_partitioned_context(&state, 10);
        let second = anonymous_partitioned_context(&state, 11);
        let request = test_request("tools/list", None);
        let response = serde_json::json!({"tools": ["session-tool"]});

        assert!(matches!(
            middleware.on_request(&first, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&first, &request, response.clone())
            .unwrap();

        let MiddlewareDecision::Respond(cached) = middleware.on_request(&second, &request).unwrap()
        else {
            panic!("same session/auth partition did not produce a cache hit");
        };
        assert_eq!(cached, response);
    }

    #[test]
    fn request_local_sessions_share_the_stateless_cache_partition() {
        let middleware =
            ResponseCachingMiddleware::new().include_tools(vec!["pure_tool".to_string()]);
        let first = anonymous_partitioned_context(&SessionState::ephemeral(), 30);
        let second = anonymous_partitioned_context(&SessionState::ephemeral(), 31);
        let request = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "pure_tool", "arguments": {"n": 1}})),
        );
        let response = serde_json::json!({"resultType": "complete", "content": [{"type": "text", "text": "1"}]});

        assert!(first.session_is_ephemeral());
        assert!(second.session_is_ephemeral());
        assert!(matches!(
            middleware.on_request(&first, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&first, &request, response.clone())
            .unwrap();

        let MiddlewareDecision::Respond(cached) = middleware.on_request(&second, &request).unwrap()
        else {
            panic!("a second request-local modern HTTP session must hit the first complete result");
        };
        assert_eq!(cached, response);
        assert!(second.response_was_served_from_cache());
    }

    #[test]
    fn mutated_request_local_response_does_not_poison_stateless_cache_partition() {
        let middleware =
            ResponseCachingMiddleware::new().include_tools(vec!["pure_tool".to_string()]);
        let state = SessionState::ephemeral();
        let mutating = anonymous_partitioned_context(&state, 32);
        let request = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "pure_tool", "arguments": {"n": 1}})),
        );

        assert!(matches!(
            middleware.on_request(&mutating, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        assert!(state.set("changed-during-dispatch", true));
        middleware
            .on_response(
                &mutating,
                &request,
                serde_json::json!({"resultType": "complete", "content": []}),
            )
            .unwrap();

        assert_eq!(middleware.stats().entries, 0);
        let fresh = anonymous_partitioned_context(&SessionState::ephemeral(), 33);
        assert!(matches!(
            middleware.on_request(&fresh, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn durable_sessions_do_not_share_request_local_cache_entries() {
        let middleware = ResponseCachingMiddleware::new();
        let ephemeral = anonymous_partitioned_context(&SessionState::ephemeral(), 40);
        let durable = anonymous_partitioned_context(&SessionState::new(), 41);
        let request = test_request("tools/list", None);
        let response = serde_json::json!({"tools": ["request-local"]});

        assert!(matches!(
            middleware.on_request(&ephemeral, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&ephemeral, &request, response)
            .unwrap();
        assert_eq!(middleware.stats().entries, 1);
        assert!(matches!(
            middleware.on_request(&durable, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn production_cache_isolates_sessions_and_complete_auth_facts() {
        let middleware = ResponseCachingMiddleware::new();
        let first_state = SessionState::new();
        let second_state = SessionState::new();
        let mut alice_auth = AuthContext::with_subject("alice");
        alice_auth.scopes = vec!["read".to_string()];
        alice_auth.claims = Some(serde_json::json!({"tenant": "one"}));
        let mut changed_claims = alice_auth.clone();
        changed_claims.claims = Some(serde_json::json!({"tenant": "two"}));
        let alice = partitioned_context(&first_state, 20, alice_auth.clone());
        let other_session = partitioned_context(&second_state, 21, alice_auth);
        let other_claims = partitioned_context(&first_state, 22, changed_claims);
        let request = test_request("resources/list", None);

        assert!(matches!(
            middleware.on_request(&alice, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(
                &alice,
                &request,
                serde_json::json!({"resources": ["alice-only"]}),
            )
            .unwrap();

        for isolated in [&other_session, &other_claims] {
            assert!(matches!(
                middleware.on_request(isolated, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
        }
        assert_eq!(middleware.stats().entries, 1);
    }

    #[test]
    fn session_state_mutation_invalidates_prior_revision() {
        let middleware = ResponseCachingMiddleware::new();
        let state = SessionState::new();
        let before = anonymous_partitioned_context(&state, 30);
        let request = test_request("prompts/list", None);

        assert!(matches!(
            middleware.on_request(&before, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(
                &before,
                &request,
                serde_json::json!({"prompts": ["before"]}),
            )
            .unwrap();
        assert!(state.set("feature", "changed"));
        let after = anonymous_partitioned_context(&state, 31);

        assert!(matches!(
            middleware.on_request(&after, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn response_is_not_cached_when_state_changes_during_dispatch() {
        let middleware = ResponseCachingMiddleware::new();
        let state = SessionState::new();
        let mutating_request = anonymous_partitioned_context(&state, 35);
        let request = test_request("resources/read", Some(serde_json::json!({"uri": "x"})));

        assert!(matches!(
            middleware.on_request(&mutating_request, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        assert!(state.set("handler-mutation", true));
        middleware
            .on_response(
                &mutating_request,
                &request,
                serde_json::json!({"contents": ["computed-before-or-during-mutation"]}),
            )
            .unwrap();

        assert_eq!(middleware.stats().entries, 0);
        let next = anonymous_partitioned_context(&state, 36);
        assert!(matches!(
            middleware.on_request(&next, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn cache_hit_is_rejected_if_state_changes_while_lookup_waits() {
        let middleware = Arc::new(ResponseCachingMiddleware::new());
        let state = SessionState::new();
        let populate = anonymous_partitioned_context(&state, 37);
        let request = test_request("resources/list", None);
        let response = serde_json::json!({"resources": ["before-mutation"]});

        assert!(matches!(
            middleware.on_request(&populate, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&populate, &request, response)
            .unwrap();

        let lookup_ctx = anonymous_partitioned_context(&state, 38);
        let admission_observer = lookup_ctx.clone();
        let lookup_middleware = Arc::clone(&middleware);
        let lookup_request = request.clone();
        let cache_guard = middleware
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lookup = std::thread::spawn(move || {
            lookup_middleware
                .on_request(&lookup_ctx, &lookup_request)
                .expect("cache lookup should not fail")
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while admission_observer
            .complete_session_cache_partition()
            .is_none()
        {
            assert!(
                Instant::now() < deadline,
                "lookup did not capture its session partition"
            );
            std::thread::yield_now();
        }

        assert!(state.set("changed-while-cache-locked", true));
        drop(cache_guard);

        let decision = lookup.join().expect("cache lookup thread");
        assert!(matches!(decision, MiddlewareDecision::Continue));
        assert_eq!(middleware.stats().hits, 0);
    }

    #[test]
    fn invalidate_removes_every_partition_for_request_identity() {
        let middleware = ResponseCachingMiddleware::new();
        let first_state = SessionState::new();
        let second_state = SessionState::new();
        let first = anonymous_partitioned_context(&first_state, 40);
        let second = anonymous_partitioned_context(&second_state, 41);
        let request = test_request("tools/list", Some(serde_json::json!({"cursor": "same"})));

        for (ctx, response) in [
            (&first, serde_json::json!({"tools": ["first"]})),
            (&second, serde_json::json!({"tools": ["second"]})),
        ] {
            assert!(matches!(
                middleware.on_request(ctx, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            middleware.on_response(ctx, &request, response).unwrap();
        }
        assert_eq!(middleware.stats().entries, 2);

        middleware.invalidate("tools/list", request.params.as_ref());

        assert_eq!(middleware.stats().entries, 0);
        for ctx in [&first, &second] {
            assert!(matches!(
                middleware.on_request(ctx, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
        }
    }

    #[test]
    fn test_caching_middleware_clear() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = test_request("tools/list", None);

        // Cache a response
        middleware.on_request(&ctx, &request).unwrap();
        middleware
            .on_response(&ctx, &request, serde_json::json!({}))
            .unwrap();

        // Verify cached
        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Respond(_)));

        // Clear cache
        middleware.clear();

        // Should miss now
        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));
    }

    #[test]
    fn test_caching_middleware_invalidate() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = test_request("tools/list", None);

        // Cache a response
        middleware.on_request(&ctx, &request).unwrap();
        middleware
            .on_response(&ctx, &request, serde_json::json!({}))
            .unwrap();

        // Invalidate specific entry
        let semantic_page_set = serde_json::json!({});
        middleware.invalidate("tools/list", Some(&semantic_page_set));

        // Should miss now
        let decision = middleware.on_request(&ctx, &request).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));
    }

    #[test]
    fn test_cache_stats_hit_rate() {
        let stats = CacheStats {
            hits: 75,
            misses: 25,
            entries: 10,
            size_bytes: 1000,
        };

        assert!((stats.hit_rate() - 75.0).abs() < 0.001);
    }

    // ── CacheStats edge cases ──────────────────────────────────────────

    #[test]
    fn cache_stats_hit_rate_zero_total() {
        let stats = CacheStats::default();
        assert!(stats.hit_rate().abs() < f64::EPSILON);
    }

    #[test]
    fn cache_stats_hit_rate_does_not_overflow_saturated_counters() {
        let stats = CacheStats {
            hits: u64::MAX,
            misses: u64::MAX,
            entries: 0,
            size_bytes: 0,
        };
        assert!((stats.hit_rate() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cache_stats_debug() {
        let stats = CacheStats::default();
        let debug = format!("{:?}", stats);
        assert!(debug.contains("CacheStats"));
    }

    // ── CacheKey ───────────────────────────────────────────────────────

    #[test]
    fn cache_key_same_method_same_params_are_equal() {
        let k1 = CacheKey::new("tools/list", Some(&serde_json::json!({"a": 1})));
        let k2 = CacheKey::new("tools/list", Some(&serde_json::json!({"a": 1})));
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_different_params_differ() {
        let k1 = CacheKey::new("tools/list", Some(&serde_json::json!({"a": 1})));
        let k2 = CacheKey::new("tools/list", Some(&serde_json::json!({"a": 2})));
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_method_and_param_presence_are_domain_separated() {
        let no_params = CacheKey::new("test", None);
        let null_params = CacheKey::new("test", Some(&serde_json::Value::Null));
        let other_method = CacheKey::new("other", None);
        assert_ne!(no_params, null_params);
        assert_ne!(no_params, other_method);
    }

    #[test]
    fn cache_key_debug_and_clone() {
        let k = CacheKey::new("test", None);
        let debug = format!("{:?}", k);
        assert!(debug.contains("CacheKey"));
        assert!(!debug.contains("test"));
        let cloned = k.clone();
        assert_eq!(k, cloned);
    }

    // ── bounded cache-key derivation ───────────────────────────────────

    #[test]
    fn cache_key_derivation_is_deterministic() {
        let v = serde_json::json!({"key": "value", "num": 42});
        let h1 = CacheKey::new("tools/list", Some(&v));
        let h2 = CacheKey::new("tools/list", Some(&v));
        assert_eq!(h1, h2);
    }

    #[test]
    fn cache_key_derivation_distinguishes_values() {
        let h1 = CacheKey::new("tools/list", Some(&serde_json::json!(1)));
        let h2 = CacheKey::new("tools/list", Some(&serde_json::json!(2)));
        assert_ne!(h1, h2);
    }

    #[test]
    fn cache_key_derivation_rejects_oversized_input_before_retention() {
        let oversized_method = "x".repeat(MAX_CACHE_KEY_INPUT_BYTES + 1);
        assert!(CacheKey::try_new(&oversized_method, None).is_none());

        let mut exact = BoundedCacheBytes::new(4);
        exact.write_all(b"1234").expect("exact boundary fits");
        assert!(exact.write_all(b"5").is_err());
        assert_eq!(exact.bytes, b"1234");
        assert!(exact.bytes.capacity() <= exact.max_bytes);
    }

    #[test]
    fn bounded_cache_bytes_fragmented_writes_grow_geometrically_within_limit() {
        const LIMIT: usize = 128 * 1024 + 37;
        let mut encoded = BoundedCacheBytes::new(LIMIT);

        for _ in 0..LIMIT {
            encoded
                .write_all(b"x")
                .expect("each byte remains inside the logical limit");
        }

        assert_eq!(encoded.bytes.len(), LIMIT);
        assert!(encoded.bytes.capacity() <= LIMIT);
        assert!(
            encoded.growth_events <= maximum_geometric_growth_events(LIMIT),
            "{} growth events exceeded the logarithmic bound",
            encoded.growth_events
        );
        assert_eq!(encoded.bytes[0], b'x');
        assert_eq!(encoded.bytes[LIMIT - 1], b'x');

        let length_before_rejection = encoded.bytes.len();
        let capacity_before_rejection = encoded.bytes.capacity();
        let growth_before_rejection = encoded.growth_events;
        assert!(encoded.write_all(b"x").is_err());
        assert_eq!(encoded.bytes.len(), length_before_rejection);
        assert_eq!(encoded.bytes.capacity(), capacity_before_rejection);
        assert_eq!(encoded.growth_events, growth_before_rejection);
    }

    #[test]
    fn bounded_cache_bytes_large_flat_json_has_bounded_growth() {
        let value = serde_json::json!({"data": "x".repeat(768 * 1024)});
        let expected = serde_json::to_vec(&value).expect("test JSON serializes");
        let logical_limit = expected.len();
        let mut encoded = BoundedCacheBytes::new(logical_limit);

        serde_json::to_writer(&mut encoded, &value).expect("flat JSON fits exact limit");

        assert_eq!(encoded.bytes, expected);
        assert!(encoded.bytes.capacity() <= logical_limit);
        assert!(
            encoded.growth_events <= maximum_geometric_growth_events(logical_limit),
            "{} growth events exceeded the logarithmic bound",
            encoded.growth_events
        );
    }

    #[test]
    fn cache_entry_size_measurement_stops_at_item_limit() {
        let value = serde_json::json!("0123456789");
        assert!(encode_json_bounded(&value, value.to_string().len()).is_some());
        assert!(encode_json_bounded(&value, value.to_string().len() - 1).is_none());
    }

    #[test]
    fn cache_serialization_rejects_excessive_json_depth() {
        let mut value = serde_json::Value::Null;
        for _ in 0..=MAX_CACHE_JSON_DEPTH {
            value = serde_json::Value::Array(vec![value]);
        }

        assert!(encode_json_bounded(&value, DEFAULT_MAX_ITEM_SIZE).is_none());
    }

    // ── LruCache additional tests ──────────────────────────────────────

    #[test]
    fn lru_cache_clear() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);
        cache.insert(
            CacheKey::new("a", None),
            serde_json::json!(1),
            Duration::from_secs(60),
        );
        cache.insert(
            CacheKey::new("b", None),
            serde_json::json!(2),
            Duration::from_secs(60),
        );
        assert_eq!(cache.len(), 2);
        assert!(!cache.is_empty());

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        assert_eq!(cache.current_size_bytes, 0);
    }

    #[test]
    fn lru_cache_remove_nonexistent() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);
        let key = CacheKey::new("nonexistent", None);
        cache.remove(&key); // should not panic
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn lru_cache_insert_duplicate_replaces() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);
        let key = CacheKey::new("test", None);
        cache.insert(
            key.clone(),
            serde_json::json!("v1"),
            Duration::from_secs(60),
        );
        cache.insert(
            key.clone(),
            serde_json::json!("v2"),
            Duration::from_secs(60),
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get_value(&key), Some(serde_json::json!("v2")));
    }

    #[test]
    fn lru_cache_get_miss_returns_none() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);
        assert!(cache.get_value(&CacheKey::new("missing", None)).is_none());
    }

    #[test]
    fn lru_cache_lru_order_updated_on_access() {
        let mut cache = LruCache::new(2, 1024 * 1024, 1024);
        let k1 = CacheKey::new("a", None);
        let k2 = CacheKey::new("b", None);
        cache.insert(k1.clone(), serde_json::json!(1), Duration::from_secs(60));
        cache.insert(k2.clone(), serde_json::json!(2), Duration::from_secs(60));

        // Access k1, making k2 the LRU
        let _ = cache.get_value(&k1);

        // Insert k3, should evict k2 (LRU)
        let k3 = CacheKey::new("c", None);
        cache.insert(k3.clone(), serde_json::json!(3), Duration::from_secs(60));
        assert!(cache.get_value(&k1).is_some()); // k1 was accessed recently
        assert!(cache.get_value(&k2).is_none()); // k2 was evicted
        assert!(cache.get_value(&k3).is_some());
    }

    // ── ToolCallCacheConfig ────────────────────────────────────────────

    #[test]
    fn should_cache_tool_disabled_returns_false() {
        let config = ToolCallCacheConfig {
            base: MethodCacheConfig {
                enabled: false,
                ttl_secs: 60,
            },
            ..ToolCallCacheConfig::default()
        };
        assert!(!config.should_cache_tool("any_tool"));
    }

    #[test]
    fn should_cache_tool_excluded_returns_false() {
        let config = ToolCallCacheConfig {
            base: MethodCacheConfig {
                enabled: true,
                ttl_secs: 60,
            },
            excluded_tools: vec!["excluded".to_string()],
            included_tools: vec!["excluded".to_string(), "other".to_string()],
        };
        assert!(!config.should_cache_tool("excluded"));
        assert!(config.should_cache_tool("other"));
    }

    #[test]
    fn should_cache_tool_include_list_filters() {
        let config = ToolCallCacheConfig {
            base: MethodCacheConfig {
                enabled: true,
                ttl_secs: 60,
            },
            included_tools: vec!["allowed".to_string()],
            excluded_tools: vec![],
        };
        assert!(config.should_cache_tool("allowed"));
        assert!(!config.should_cache_tool("not_allowed"));
    }

    #[test]
    fn should_cache_tool_exclude_takes_precedence_over_include() {
        let config = ToolCallCacheConfig {
            base: MethodCacheConfig {
                enabled: true,
                ttl_secs: 60,
            },
            included_tools: vec!["tool".to_string()],
            excluded_tools: vec!["tool".to_string()],
        };
        assert!(!config.should_cache_tool("tool"));
    }

    // ── MethodCacheConfig ──────────────────────────────────────────────

    #[test]
    fn method_cache_config_default() {
        let config = MethodCacheConfig::default();
        assert!(config.enabled);
        assert_eq!(config.ttl_secs, DEFAULT_CALL_TTL_SECS);
    }

    #[test]
    fn method_cache_config_debug() {
        let config = MethodCacheConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("MethodCacheConfig"));
    }

    // ── ResponseCachingMiddleware construction ──────────────────────────

    #[test]
    fn default_equals_new() {
        let d = ResponseCachingMiddleware::default();
        let n = ResponseCachingMiddleware::new();
        assert_eq!(d.list_ttl, n.list_ttl);
        assert_eq!(d.call_ttl, n.call_ttl);
    }

    #[test]
    fn debug_output() {
        let m = ResponseCachingMiddleware::new();
        let debug = format!("{:?}", m);
        assert!(debug.contains("ResponseCachingMiddleware"));
        assert!(debug.contains("list_ttl"));
        assert!(debug.contains("call_ttl"));
    }

    // ── Fluent setters ─────────────────────────────────────────────────

    #[test]
    fn list_ttl_secs_updates_all_list_configs() {
        let m = ResponseCachingMiddleware::new().list_ttl_secs(600);
        assert_eq!(m.list_ttl, Duration::from_secs(600));
        assert_eq!(m.tools_list_config.ttl_secs, 600);
        assert_eq!(m.resources_list_config.ttl_secs, 600);
        assert_eq!(m.prompts_list_config.ttl_secs, 600);
    }

    #[test]
    fn call_ttl_secs_updates_all_call_configs() {
        let m = ResponseCachingMiddleware::new().call_ttl_secs(7200);
        assert_eq!(m.call_ttl, Duration::from_secs(7200));
        assert_eq!(m.tools_call_config.base.ttl_secs, 7200);
        assert_eq!(m.resources_read_config.ttl_secs, 7200);
        assert_eq!(m.prompts_get_config.ttl_secs, 7200);
    }

    #[test]
    fn max_entries_setter() {
        let m = ResponseCachingMiddleware::new().max_entries(50);
        let cache = m
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(cache.max_entries, 50);
    }

    #[test]
    fn max_size_bytes_setter() {
        let m = ResponseCachingMiddleware::new().max_size_bytes(2048);
        let cache = m
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(cache.max_size_bytes, 2048);
    }

    #[test]
    fn max_item_size_setter() {
        let m = ResponseCachingMiddleware::new().max_item_size(512);
        let cache = m
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(cache.max_item_size, 512);
    }

    // ── Disable method variants ────────────────────────────────────────

    #[test]
    fn disable_resources_list() {
        let m = ResponseCachingMiddleware::new().disable_resources_list();
        assert!(!m.resources_list_config.enabled);
        assert!(m.tools_list_config.enabled); // others unchanged
    }

    #[test]
    fn disable_prompts_list() {
        let m = ResponseCachingMiddleware::new().disable_prompts_list();
        assert!(!m.prompts_list_config.enabled);
    }

    #[test]
    fn disable_tools_call() {
        let m = ResponseCachingMiddleware::new().disable_tools_call();
        assert!(!m.tools_call_config.base.enabled);
    }

    #[test]
    fn disable_resources_read() {
        let m = ResponseCachingMiddleware::new().disable_resources_read();
        assert!(!m.resources_read_config.enabled);
    }

    #[test]
    fn disable_prompts_get() {
        let m = ResponseCachingMiddleware::new().disable_prompts_get();
        assert!(!m.prompts_get_config.enabled);
    }

    // ── include_tools / exclude_tools ──────────────────────────────────

    #[test]
    fn include_tools_restricts_caching() {
        let m = ResponseCachingMiddleware::new().include_tools(vec!["allowed_tool".to_string()]);
        let _ctx = test_context();

        // allowed_tool should be cached
        let req = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "allowed_tool"})),
        );
        assert!(m.should_cache_method(&req.method, req.params.as_ref()));

        // other_tool should not be cached
        let req2 = test_request(
            "tools/call",
            Some(serde_json::json!({"name": "other_tool"})),
        );
        assert!(!m.should_cache_method(&req2.method, req2.params.as_ref()));

        // non-tool methods still work
        let req3 = test_request("tools/list", None);
        assert!(m.should_cache_method(&req3.method, req3.params.as_ref()));
    }

    // ── should_cache_method edge cases ─────────────────────────────────

    #[test]
    fn should_cache_tools_call_without_name_returns_false() {
        let m = ResponseCachingMiddleware::new();
        // tools/call with params but no "name" field
        assert!(!m.should_cache_method("tools/call", Some(&serde_json::json!({"arguments": {}}))));
    }

    #[test]
    fn should_cache_tools_call_with_no_params_returns_false() {
        let m = ResponseCachingMiddleware::new();
        assert!(!m.should_cache_method("tools/call", None));
    }

    #[test]
    fn should_cache_unknown_method_returns_false() {
        let m = ResponseCachingMiddleware::new();
        assert!(!m.should_cache_method("unknown/method", None));
    }

    #[test]
    fn should_cache_all_known_cacheable_methods() {
        let m = ResponseCachingMiddleware::new();
        assert!(m.should_cache_method("tools/list", None));
        assert!(m.should_cache_method("resources/list", None));
        assert!(m.should_cache_method("prompts/list", None));
        assert!(m.should_cache_method("resources/read", None));
        assert!(m.should_cache_method("prompts/get", None));
    }

    // ── get_ttl ────────────────────────────────────────────────────────

    #[test]
    fn get_ttl_list_methods() {
        let m = ResponseCachingMiddleware::new().list_ttl_secs(120);
        assert_eq!(m.get_ttl("tools/list"), Duration::from_secs(120));
        assert_eq!(m.get_ttl("resources/list"), Duration::from_secs(120));
        assert_eq!(m.get_ttl("prompts/list"), Duration::from_secs(120));
    }

    #[test]
    fn get_ttl_call_methods() {
        let m = ResponseCachingMiddleware::new().call_ttl_secs(900);
        assert_eq!(m.get_ttl("tools/call"), Duration::from_mins(15));
        assert_eq!(m.get_ttl("resources/read"), Duration::from_mins(15));
        assert_eq!(m.get_ttl("prompts/get"), Duration::from_mins(15));
    }

    #[test]
    fn get_ttl_unknown_method_uses_call_ttl() {
        let m = ResponseCachingMiddleware::new().call_ttl_secs(999);
        assert_eq!(m.get_ttl("unknown/method"), Duration::from_secs(999));
    }

    // ── on_error passes through ────────────────────────────────────────

    #[test]
    fn on_error_passes_through() {
        let m = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let req = test_request("tools/list", None);
        let err = McpError::internal_error("test error");
        let result = m.on_error(&ctx, &req, err);
        assert!(result.message.contains("test error"));
    }

    // ── stats tracks entries and size ──────────────────────────────────

    #[test]
    fn stats_tracks_entries_and_size() {
        let m = ResponseCachingMiddleware::new();
        let ctx = test_context();

        let stats = m.stats();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.size_bytes, 0);

        let req = test_request("tools/list", None);
        m.on_request(&ctx, &req).unwrap();
        m.on_response(&ctx, &req, serde_json::json!({"tools": []}))
            .unwrap();

        let stats = m.stats();
        assert_eq!(stats.entries, 1);
        assert!(stats.size_bytes > 0);
        assert_eq!(stats.misses, 1);
    }

    // ── Middleware caches resources/list and prompts/list ───────────────

    #[test]
    fn caches_resources_list() {
        let m = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let req = test_request("resources/list", None);

        m.on_request(&ctx, &req).unwrap();
        m.on_response(&ctx, &req, serde_json::json!({"resources": []}))
            .unwrap();

        let decision = m.on_request(&ctx, &req).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Respond(_)));
    }

    #[test]
    fn caches_prompts_list() {
        let m = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let req = test_request("prompts/list", None);

        m.on_request(&ctx, &req).unwrap();
        m.on_response(&ctx, &req, serde_json::json!({"prompts": []}))
            .unwrap();

        let decision = m.on_request(&ctx, &req).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Respond(_)));
    }

    // ── CacheEntry debug/clone ─────────────────────────────────────────

    #[test]
    fn cache_entry_debug_and_clone() {
        let value = serde_json::json!("CACHE-SECRET-CANARY");
        let entry = CacheEntry::new(
            value.clone(),
            Duration::from_secs(60),
            DEFAULT_MAX_ITEM_SIZE,
        )
        .expect("short test TTL must be representable");
        let debug = format!("{:?}", entry);
        assert!(debug.contains("CacheEntry"));
        assert!(
            !debug.contains("CACHE-SECRET-CANARY"),
            "cached payloads must stay out of Debug"
        );
        let cloned = entry.clone();
        assert_eq!(decode_cached_json(&cloned.encoded), Some(value));
        assert_eq!(
            cloned.size_bytes,
            cloned.encoded.len() + CACHE_ENTRY_METADATA_BYTES
        );
    }

    #[test]
    fn cache_entry_not_expired_initially() {
        let entry = CacheEntry::new(
            serde_json::json!(1),
            Duration::from_secs(60),
            DEFAULT_MAX_ITEM_SIZE,
        )
        .expect("short test TTL must be representable");
        assert!(!entry.is_expired());
    }

    #[test]
    fn caches_resources_read() {
        let m = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let req = test_request(
            "resources/read",
            Some(serde_json::json!({"uri": "file:///a.txt"})),
        );

        m.on_request(&ctx, &req).unwrap();
        m.on_response(&ctx, &req, serde_json::json!({"contents": []}))
            .unwrap();

        let decision = m.on_request(&ctx, &req).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Respond(_)));
    }

    #[test]
    fn caches_prompts_get() {
        let m = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let req = test_request("prompts/get", Some(serde_json::json!({"name": "greeting"})));

        m.on_request(&ctx, &req).unwrap();
        m.on_response(&ctx, &req, serde_json::json!({"messages": []}))
            .unwrap();

        let decision = m.on_request(&ctx, &req).unwrap();
        assert!(matches!(decision, MiddlewareDecision::Respond(_)));
    }

    #[test]
    fn lru_cache_evict_expired_frees_entries() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);
        // Insert two entries with tiny TTL
        cache.insert(
            CacheKey::new("a", None),
            serde_json::json!(1),
            Duration::from_millis(1),
        );
        cache.insert(
            CacheKey::new("b", None),
            serde_json::json!(2),
            Duration::from_millis(1),
        );
        assert_eq!(cache.len(), 2);

        std::thread::sleep(std::time::Duration::from_millis(10));
        cache.evict_expired();

        assert_eq!(cache.len(), 0);
        assert_eq!(cache.current_size_bytes, 0);
    }

    #[test]
    fn lru_cache_insert_replaces_updates_size() {
        let mut cache = LruCache::new(10, 1024 * 1024, 1024);
        let key = CacheKey::new("k", None);
        cache.insert(
            key.clone(),
            serde_json::json!("short"),
            Duration::from_secs(60),
        );
        let size_after_first = cache.current_size_bytes;

        cache.insert(
            key.clone(),
            serde_json::json!("much longer value here"),
            Duration::from_secs(60),
        );
        let size_after_second = cache.current_size_bytes;

        // Size should reflect only the new entry (old was removed first)
        assert_ne!(size_after_first, size_after_second);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn tool_call_cache_config_debug_and_clone() {
        let config = ToolCallCacheConfig {
            base: MethodCacheConfig {
                enabled: true,
                ttl_secs: 120,
            },
            included_tools: vec!["t1".to_string()],
            excluded_tools: vec!["t2".to_string()],
        };
        let debug = format!("{:?}", config);
        assert!(debug.contains("ToolCallCacheConfig"));
        let cloned = config.clone();
        assert_eq!(cloned.included_tools, vec!["t1".to_string()]);
        assert_eq!(cloned.excluded_tools, vec!["t2".to_string()]);
    }

    #[test]
    fn cache_stats_clone() {
        let stats = CacheStats {
            hits: 10,
            misses: 5,
            entries: 3,
            size_bytes: 100,
        };
        let cloned = stats.clone();
        assert_eq!(cloned.hits, 10);
        assert_eq!(cloned.misses, 5);
        assert_eq!(cloned.entries, 3);
        assert_eq!(cloned.size_bytes, 100);
    }

    #[test]
    fn should_cache_tool_empty_allowlist_disables_all() {
        let config = ToolCallCacheConfig {
            base: MethodCacheConfig {
                enabled: true,
                ttl_secs: 60,
            },
            included_tools: vec![],
            excluded_tools: vec![],
        };
        assert!(!config.should_cache_tool("any_tool"));
        assert!(!config.should_cache_tool("another_tool"));
    }

    #[test]
    fn cache_01_a_positive() {
        let middleware = ResponseCachingMiddleware::new()
            .list_ttl_secs(120)
            .call_ttl_secs(900);
        let methods = [
            ("server/discover", None, 120_000_u64),
            ("tools/list", None, 120_000),
            ("prompts/list", None, 120_000),
            ("resources/list", None, 120_000),
            (
                "resources/read",
                Some(serde_json::json!({"uri": "file:///catalog"})),
                900_000,
            ),
            ("resources/templates/list", None, 120_000),
        ];

        for (method, params, expected_ttl_ms) in methods {
            let ctx = test_context();
            let request = test_request(method, params);
            assert!(matches!(
                middleware.on_request(&ctx, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            let response = middleware
                .on_response(
                    &ctx,
                    &request,
                    serde_json::json!({"resultType": "complete", "items": []}),
                )
                .expect("the middleware must preserve a complete result");

            assert_eq!(
                response.get("ttlMs"),
                Some(&serde_json::json!(expected_ttl_ms))
            );
            assert_eq!(
                response.get("cacheScope"),
                Some(&serde_json::json!("private"))
            );
        }
    }

    #[test]
    fn cache_01_final_wire_valid_hints_remain_lossless_outside_runtime_expiry() {
        let middleware = ResponseCachingMiddleware::new().list_ttl_secs(120);
        let ctx = test_context();
        let request = test_request("tools/list", None);
        let huge_ttl: serde_json::Value =
            serde_json::from_str("922337203685477580812345678901234567890")
                .expect("arbitrary-width JSON integer fixture");
        let response = serde_json::json!({
            "resultType": "complete",
            "tools": [],
            "ttlMs": huge_ttl,
            "cacheScope": "private",
        });

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let delivered = middleware
            .on_response(&ctx, &request, response.clone())
            .expect("a wire-valid but uncacheable final TTL remains deliverable");

        assert_eq!(
            delivered, response,
            "runtime expiry bounds must not rewrite upstream final cache hints"
        );
        assert_eq!(
            delivered["ttlMs"].to_string(),
            "922337203685477580812345678901234567890"
        );
        assert_eq!(
            middleware.stats().entries,
            0,
            "an unrepresentable private final TTL must not create a local-expiry entry"
        );

        let fractional = serde_json::json!({
            "resultType": "complete",
            "tools": [],
            "ttlMs": 120_000.5,
            "cacheScope": "private",
        });
        let normalized = middleware
            .on_response(&ctx, &request, fractional)
            .expect("invalid cache hints are replaced by the local policy");
        assert_eq!(normalized["ttlMs"], serde_json::json!(120_000));
        assert_eq!(normalized["cacheScope"], serde_json::json!("private"));
        assert_eq!(
            middleware.stats().entries,
            1,
            "the paired representable local policy remains cacheable"
        );
    }

    #[test]
    fn every_final_result_with_an_unrepresentable_private_ttl_skips_local_cache_state() {
        let middleware = ResponseCachingMiddleware::new();
        let huge_ttl: serde_json::Value = serde_json::from_str("18446744073709551616000")
            .expect("arbitrary-width JSON integer fixture");
        let methods = [
            ("tools/list", None),
            ("resources/list", None),
            ("resources/templates/list", None),
            ("prompts/list", None),
            (
                "resources/read",
                Some(serde_json::json!({"uri": "file:///huge-ttl"})),
            ),
        ];

        for (method, params) in methods {
            let ctx = test_context();
            let request = test_request(method, params);
            let response = serde_json::json!({
                "resultType": "complete",
                "items": [],
                "ttlMs": huge_ttl.clone(),
                "cacheScope": "private",
            });
            assert!(matches!(
                middleware.on_request(&ctx, &request).unwrap(),
                MiddlewareDecision::Continue
            ));
            let delivered = middleware
                .on_response(&ctx, &request, response.clone())
                .expect("wire-valid final result remains deliverable");

            assert_eq!(delivered, response);
            assert_eq!(
                middleware.stats().entries,
                0,
                "{method} must not create a local entry for an unrepresentable TTL"
            );
        }

        let discovery = serde_json::json!({
            "supportedVersions": [FINAL_PROTOCOL_VERSION],
            "capabilities": {},
            "ttlMs": huge_ttl.clone(),
            "cacheScope": "private",
        });
        let ctx = test_context();
        let request = final_discovery_request(FINAL_PROTOCOL_VERSION);
        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let delivered = middleware
            .on_response(&ctx, &request, discovery.clone())
            .expect("wire-valid discovery result remains deliverable");
        assert_eq!(delivered, discovery);
        assert_eq!(
            middleware.stats().entries,
            0,
            "server/discover must also skip local state for an unrepresentable TTL"
        );
    }

    #[test]
    fn cache_01_a_planted_negative() {
        let middleware = ResponseCachingMiddleware::new().list_ttl_secs(120);
        let ctx = test_context();
        let request = test_request("tools/list", None);

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        // The sole forbidden dimension differs from the positive case: this
        // is an input-required result, not a cacheable complete result.
        let response = middleware
            .on_response(
                &ctx,
                &request,
                serde_json::json!({
                    "resultType": "inputRequired",
                    "ttlMs": 120_000,
                    "cacheScope": "public",
                    "items": []
                }),
            )
            .expect("input-required results remain ordinary middleware output");

        assert_eq!(
            response.get("resultType"),
            Some(&serde_json::json!("inputRequired"))
        );
        assert!(response.get("ttlMs").is_none());
        assert!(response.get("cacheScope").is_none());
        assert_eq!(middleware.stats().entries, 0);
        assert!(matches!(
            middleware
                .on_request(&ctx, &request)
                .expect("lookup is safe"),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn cache_01_b_positive() {
        let middleware = ResponseCachingMiddleware::new();
        let first_page = test_request("tools/list", Some(serde_json::json!({"cursor": "a"})));
        let second_page = test_request("tools/list", Some(serde_json::json!({"cursor": "b"})));

        for (request, tool_name) in [(&first_page, "first"), (&second_page, "second")] {
            let ctx = test_context();
            assert!(matches!(
                middleware.on_request(&ctx, request).unwrap(),
                MiddlewareDecision::Continue
            ));
            middleware
                .on_response(
                    &ctx,
                    request,
                    serde_json::json!({
                        "resultType": "complete",
                        "tools": [{"name": tool_name}]
                    }),
                )
                .expect("each page is individually cacheable");
        }

        assert!(matches!(
            middleware
                .on_request(&test_context(), &first_page)
                .expect("first lookup is safe"),
            MiddlewareDecision::Respond(_)
        ));
        assert!(matches!(
            middleware
                .on_request(&test_context(), &second_page)
                .expect("second lookup is safe"),
            MiddlewareDecision::Respond(_)
        ));

        middleware.invalidate("tools/list", None);

        assert_eq!(middleware.stats().entries, 0);
        assert!(matches!(
            middleware
                .on_request(&test_context(), &first_page)
                .expect("first post-invalidation lookup is safe"),
            MiddlewareDecision::Continue
        ));
        assert!(matches!(
            middleware
                .on_request(&test_context(), &second_page)
                .expect("second post-invalidation lookup is safe"),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn cache_01_b_planted_negative() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = test_request(
            "tools/list",
            Some(serde_json::json!({
                "cursor": "a",
                "requestState": {"opaque": "continuation"}
            })),
        );

        // The only semantic change from the cacheable page is continuation
        // state. It must neither read nor populate an internal cache entry.
        assert!(matches!(
            middleware
                .on_request(&ctx, &request)
                .expect("continuation lookup is safe"),
            MiddlewareDecision::Continue
        ));
        let response = middleware
            .on_response(
                &ctx,
                &request,
                serde_json::json!({"resultType": "complete", "tools": []}),
            )
            .expect("the continuation result remains deliverable");

        assert_eq!(
            response.get("cacheScope"),
            Some(&serde_json::json!("private"))
        );
        assert_eq!(middleware.stats().entries, 0);
        assert!(matches!(
            middleware
                .on_request(&ctx, &request)
                .expect("repeat continuation lookup is safe"),
            MiddlewareDecision::Continue
        ));
    }

    #[test]
    fn cache_discovery_final_policy_positive() {
        let middleware = ResponseCachingMiddleware::new().list_ttl_secs(300);
        let ctx = test_context();
        let request = final_discovery_request(FINAL_PROTOCOL_VERSION);
        let ttl = Duration::from_secs(30);
        let response = final_discovery_response(30_000, "private");

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let delivered = middleware
            .on_response(&ctx, &request, response.clone())
            .expect("final discovery response remains deliverable");

        assert_eq!(delivered, response, "final hints remain public-observable");
        let cache = middleware
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = cache
            .entries
            .values()
            .next()
            .expect("private final discovery response is cached");
        assert!(
            entry.expires_at <= Instant::now().checked_add(ttl).expect("short TTL is valid"),
            "the stored expiry must use final ttlMs rather than the list default"
        );
        drop(cache);

        assert!(matches!(
            middleware
                .on_request(&ctx, &request)
                .expect("final discovery lookup is safe"),
            MiddlewareDecision::Respond(value) if value == response
        ));
    }

    #[test]
    fn cache_discovery_public_scope_planted_negative() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = final_discovery_request(FINAL_PROTOCOL_VERSION);
        let response = final_discovery_response(30_000, "public");

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        // This differs from the cacheable final response only in cacheScope.
        // A public wire claim is delivered faithfully but cannot authorize this
        // private middleware cache.
        let delivered = middleware
            .on_response(&ctx, &request, response.clone())
            .expect("public discovery response remains deliverable");

        assert_eq!(delivered, response);
        let before_lookup = middleware.stats();
        assert_eq!(before_lookup.entries, 0);
        assert_eq!(before_lookup.size_bytes, 0);
        assert!(matches!(
            middleware
                .on_request(&ctx, &request)
                .expect("public-scope lookup is safe"),
            MiddlewareDecision::Continue
        ));
        let after_lookup = middleware.stats();
        assert_eq!(after_lookup.entries, before_lookup.entries);
        assert_eq!(after_lookup.size_bytes, before_lookup.size_bytes);
    }

    #[test]
    fn cache_discovery_huge_and_fractional_ttls_leave_existing_entry_unchanged() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = final_discovery_request(FINAL_PROTOCOL_VERSION);
        let cached = final_discovery_response(30_000, "private");

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&ctx, &request, cached.clone())
            .expect("baseline final discovery response is cacheable");
        let before = middleware.stats();
        assert_eq!(before.entries, 1);

        let mut huge = cached.clone();
        huge["ttlMs"] =
            serde_json::from_str("18446744073709551616").expect("unbounded JSON integer fixture");
        let delivered = middleware
            .on_response(&ctx, &request, huge.clone())
            .expect("huge final discovery TTL remains deliverable");

        assert_eq!(
            delivered, huge,
            "a wire-valid TTL outside the local runtime domain stays lossless"
        );
        assert_eq!(
            delivered["ttlMs"].to_string(),
            "18446744073709551616",
            "the huge TTL retains its exact peer spelling"
        );
        assert_eq!(
            middleware.stats(),
            before,
            "an uncacheable huge TTL does not replace the cached response"
        );

        let mut fractional = huge;
        fractional["ttlMs"] = serde_json::from_str("18446744073709551616.5")
            .expect("fractional paired-negative fixture");
        let delivered = middleware
            .on_response(&ctx, &request, fractional)
            .expect("invalid final discovery hints remain deliverable");

        assert!(delivered.get("ttlMs").is_none());
        assert!(delivered.get("cacheScope").is_none());
        assert_eq!(
            middleware.stats(),
            before,
            "changing only the TTL to a fractional value leaves cached state unchanged"
        );
        assert!(matches!(
            middleware
                .on_request(&ctx, &request)
                .expect("baseline cache lookup is safe"),
            MiddlewareDecision::Respond(value) if value == cached
        ));
    }

    #[test]
    fn cache_discovery_stale_generation_planted_negative() {
        let middleware = ResponseCachingMiddleware::new();
        let ctx = test_context();
        let request = final_discovery_request(FINAL_PROTOCOL_VERSION);
        let response = final_discovery_response(30_000, "private");

        assert!(matches!(
            middleware.on_request(&ctx, &request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&ctx, &request, response)
            .expect("initial final discovery response is cacheable");
        let binding_before_invalidation = middleware
            .cache_entry_binding(&request)
            .expect("final discovery has a cache binding");
        assert_eq!(middleware.stats().entries, 1);

        // The request and result are unchanged; advancing only the discovery
        // generation makes the previous entry stale.
        middleware.invalidate(SERVER_DISCOVER_METHOD, request.params.as_ref());
        let binding_after_invalidation = middleware
            .cache_entry_binding(&request)
            .expect("fresh generation has a cache binding");
        assert_ne!(binding_after_invalidation, binding_before_invalidation);

        let before_lookup = middleware.stats();
        assert_eq!(before_lookup.entries, 0);
        assert_eq!(before_lookup.size_bytes, 0);
        assert!(matches!(
            middleware
                .on_request(&ctx, &request)
                .expect("stale discovery lookup is safe"),
            MiddlewareDecision::Continue
        ));
        let after_lookup = middleware.stats();
        assert_eq!(after_lookup.entries, before_lookup.entries);
        assert_eq!(after_lookup.size_bytes, before_lookup.size_bytes);
    }

    #[test]
    fn cache_discovery_cross_era_planted_negative() {
        let middleware = ResponseCachingMiddleware::new();
        let final_ctx = test_context();
        let legacy_ctx = test_context();
        let final_request = final_discovery_request(FINAL_PROTOCOL_VERSION);
        let mut legacy_request = final_request.clone();
        legacy_request
            .params
            .as_mut()
            .expect("test request has metadata")["_meta"][FINAL_PROTOCOL_VERSION_META_KEY] =
            serde_json::json!(ProtocolEra::Legacy2024.version().as_str());
        let response = final_discovery_response(30_000, "private");

        assert!(matches!(
            middleware.on_request(&final_ctx, &final_request).unwrap(),
            MiddlewareDecision::Continue
        ));
        middleware
            .on_response(&final_ctx, &final_request, response.clone())
            .expect("final discovery response is cacheable");
        let before_legacy = middleware.stats();
        assert_eq!(before_legacy.entries, 1);

        // The only changed input is the exact protocol era. It must neither
        // reuse the final entry nor add an entry in the final generation.
        assert!(matches!(
            middleware.on_request(&legacy_ctx, &legacy_request).unwrap(),
            MiddlewareDecision::Continue
        ));
        let delivered = middleware
            .on_response(&legacy_ctx, &legacy_request, response.clone())
            .expect("legacy response remains deliverable without caching");
        assert_eq!(delivered, response);
        assert!(matches!(
            middleware
                .on_request(&legacy_ctx, &legacy_request)
                .expect("legacy lookup is safe"),
            MiddlewareDecision::Continue
        ));
        let after_legacy = middleware.stats();
        assert_eq!(after_legacy.entries, before_legacy.entries);
        assert_eq!(after_legacy.size_bytes, before_legacy.size_bytes);
        assert!(matches!(
            middleware
                .on_request(&final_ctx, &final_request)
                .expect("final lookup remains safe"),
            MiddlewareDecision::Respond(value) if value == response
        ));
    }
}
