//! Rate limiting middleware for protecting FastMCP servers from abuse.
//!
//! This module provides two rate limiting strategies:
//!
//! - [`RateLimitingMiddleware`]: Token bucket algorithm for burst-friendly limits
//! - [`SlidingWindowRateLimitingMiddleware`]: Sliding window for precise tracking
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_rust::{modern::ServerBuilder, prelude::*};
//! use fastmcp_rust::rate_limiting::RateLimitingMiddleware;
//!
//! // Allow 10 requests per second with bursts up to 20
//! let rate_limiter = RateLimitingMiddleware::new(10.0)
//!     .burst_capacity(20);
//!
//! // Inside the application's async entry point, with its supplied `cx`.
//! ServerBuilder::new("my-server", "1.0.0")
//!     .middleware(rate_limiter)
//!     .build()
//!     .run_stdio_with_cx(cx)
//!     .await;
//! ```

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fastmcp_core::{
    McpContext, McpError, McpErrorCode, McpResult, SHA256_DIGEST_BYTES, Sha256Digest,
    sha256_bounded,
};
use fastmcp_protocol::JsonRpcRequest;

use crate::{Middleware, MiddlewareDecision};

/// Error code for rate limit exceeded (-32005).
///
/// This is in the MCP server error range (-32000 to -32099).
pub const RATE_LIMIT_ERROR_CODE: i32 = -32005;

/// Maximum accepted byte length for a custom client identifier.
///
/// Identifiers are hashed immediately after this bound is enforced. The raw
/// value is never retained by the middleware or included in diagnostics.
const MAX_CLIENT_ID_BYTES: usize = 4096;

/// Maximum number of limiter partitions, including the shared overflow
/// partition.
const MAX_CLIENT_PARTITIONS: usize = 4096;

/// One partition is permanently reserved for identifiers first observed after
/// the named-partition cap has been reached.
const MAX_NAMED_CLIENT_PARTITIONS: usize = MAX_CLIENT_PARTITIONS - 1;

/// Minimum inactivity period before a dedicated partition may be reclaimed.
/// Reclamation additionally requires the limiter to have naturally returned
/// to its initial state, so eviction cannot reset an active limit.
const CLIENT_PARTITION_IDLE_TTL: Duration = Duration::from_secs(60);

const DEFAULT_CLIENT_ID: &[u8] = b"fastmcp-default-rate-limit-partition";
const RATE_LIMIT_EXCEEDED_MESSAGE: &str = "Rate limit exceeded";
const RATE_LIMIT_METHOD_PARTITION_DOMAIN: &[u8] = b"fastmcp-rate-limit-method-partition-v1\0";
const MAX_RATE_LIMIT_METHOD_BYTES: usize = 512;
const MAX_RATE_LIMIT_PARTITION_INPUT_BYTES: usize = RATE_LIMIT_METHOD_PARTITION_DOMAIN.len()
    + SHA256_DIGEST_BYTES
    + std::mem::size_of::<u64>()
    + MAX_RATE_LIMIT_METHOD_BYTES;

// Token counts are stored as `f64`. Above 2^53 - 1, subtracting one can round
// back to the original value and silently turn a configured bucket into an
// effectively unlimited one. On 32-bit targets this cast naturally yields
// `usize::MAX`, where every `usize` remains exactly representable for the
// integer operations used here.
const MAX_EXACT_TOKEN_CAPACITY: usize = ((1_u64 << 53) - 1) as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateLimitAdmission {
    Allowed,
    Rejected { retry_after_ms: u64 },
}

impl RateLimitAdmission {
    const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

fn sanitized_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        0.0
    }
}

fn default_burst_capacity(rate: f64) -> usize {
    if rate <= 0.0 {
        return 0;
    }

    let doubled = rate * 2.0;
    if !doubled.is_finite() || doubled > MAX_EXACT_TOKEN_CAPACITY as f64 {
        0
    } else {
        // Every accepted positive rate must admit at least one initial
        // request. Truncating a fractional default below 0.5 requests/second
        // to zero otherwise creates a limiter that can never recover.
        (doubled as usize).max(1)
    }
}

fn default_client_partition() -> McpResult<Sha256Digest> {
    sha256_bounded(DEFAULT_CLIENT_ID, MAX_CLIENT_ID_BYTES)
        .map_err(|_| rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE))
}

fn rate_limit_method_partition(
    client_partition: Sha256Digest,
    method: &str,
) -> McpResult<Sha256Digest> {
    if method.len() > MAX_RATE_LIMIT_METHOD_BYTES {
        return Err(rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE));
    }

    let method_len =
        u64::try_from(method.len()).map_err(|_| rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE))?;
    let mut input = Vec::with_capacity(MAX_RATE_LIMIT_PARTITION_INPUT_BYTES);
    input.extend_from_slice(RATE_LIMIT_METHOD_PARTITION_DOMAIN);
    input.extend_from_slice(client_partition.as_bytes());
    input.extend_from_slice(&method_len.to_be_bytes());
    input.extend_from_slice(method.as_bytes());
    sha256_bounded(&input, MAX_RATE_LIMIT_PARTITION_INPUT_BYTES)
        .map_err(|_| rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE))
}

fn retry_after_millis(deficit: f64, refill_rate: f64) -> u64 {
    if !deficit.is_finite() || !refill_rate.is_finite() || deficit <= 0.0 || refill_rate <= 0.0 {
        return u64::MAX;
    }

    let millis = (deficit / refill_rate) * 1_000.0;
    if !millis.is_finite() || millis >= u64::MAX as f64 {
        u64::MAX
    } else {
        (millis.ceil() as u64).max(1)
    }
}

fn rate_limit_retry_error(request: &JsonRpcRequest, retry_after_ms: u64) -> McpError {
    McpError::with_data(
        McpErrorCode::Custom(RATE_LIMIT_ERROR_CODE),
        RATE_LIMIT_EXCEEDED_MESSAGE,
        serde_json::json!({
            "method": request.method.clone(),
            "requestId": request.id.clone(),
            "retryAfterMs": retry_after_ms,
        }),
    )
}

#[derive(Debug)]
struct ClientPartition<L> {
    limiter: L,
    last_seen: Instant,
}

/// Bounded partition storage with an ordered recency index.
///
/// The index makes the full-and-live churn path logarithmic instead of
/// scanning every partition for each unseen identifier. Digest bytes provide
/// a deterministic tie-breaker when the monotonic clock returns equal values.
#[derive(Debug)]
struct PartitionStore<L> {
    entries: HashMap<Sha256Digest, ClientPartition<L>>,
    recency: BTreeSet<(Instant, [u8; SHA256_DIGEST_BYTES])>,
}

impl<L> PartitionStore<L> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            recency: BTreeSet::new(),
        }
    }

    fn use_existing<T, F>(&mut self, key: Sha256Digest, operation: F) -> Option<T>
    where
        F: FnOnce(&L) -> T,
    {
        self.use_existing_at(key, Instant::now(), operation)
    }

    fn use_existing_at<T, F>(&mut self, key: Sha256Digest, now: Instant, operation: F) -> Option<T>
    where
        F: FnOnce(&L) -> T,
    {
        let key_bytes = key.into_bytes();
        let (old_last_seen, new_last_seen, result) = {
            let entry = self.entries.get_mut(&key)?;
            let old_last_seen = entry.last_seen;
            let result = operation(&entry.limiter);
            let new_last_seen = now.max(old_last_seen);
            entry.last_seen = new_last_seen;
            (old_last_seen, new_last_seen, result)
        };

        let removed = self.recency.remove(&(old_last_seen, key_bytes));
        let inserted = self.recency.insert((new_last_seen, key_bytes));
        debug_assert!(removed, "existing partition must have a recency entry");
        debug_assert!(inserted, "updated recency entry must be unique");
        Some(result)
    }

    fn insert(&mut self, key: Sha256Digest, limiter: L) {
        self.insert_at(key, limiter, Instant::now());
    }

    fn insert_at(&mut self, key: Sha256Digest, limiter: L, last_seen: Instant) {
        let key_bytes = key.into_bytes();
        let previous = self
            .entries
            .insert(key, ClientPartition { limiter, last_seen });
        debug_assert!(previous.is_none(), "partition insertion must be unique");
        let inserted = self.recency.insert((last_seen, key_bytes));
        debug_assert!(inserted, "new recency entry must be unique");
    }

    fn reclaim_oldest_if<F>(&mut self, now: Instant, idle_ttl: Duration, mut is_reset: F) -> bool
    where
        F: FnMut(&L) -> bool,
    {
        let mut candidate = None;
        for &(last_seen, key_bytes) in &self.recency {
            let Some(idle_for) = now.checked_duration_since(last_seen) else {
                break;
            };
            if idle_for < idle_ttl {
                break;
            }

            let key = Sha256Digest::from_bytes(key_bytes);
            let Some(entry) = self.entries.get(&key) else {
                continue;
            };
            if is_reset(&entry.limiter) {
                candidate = Some((last_seen, key_bytes, key));
                break;
            }
        }

        let Some((last_seen, key_bytes, key)) = candidate else {
            return false;
        };
        let removed_recency = self.recency.remove(&(last_seen, key_bytes));
        let removed_entry = self.entries.remove(&key);
        debug_assert!(removed_recency, "recency entry must exist");
        debug_assert!(removed_entry.is_some(), "partition entry must exist");
        true
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    fn contains_key(&self, key: &Sha256Digest) -> bool {
        self.entries.contains_key(key)
    }

    #[cfg(test)]
    fn set_last_seen(&mut self, key: Sha256Digest, last_seen: Instant) -> bool {
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        let key_bytes = key.into_bytes();
        let removed = self.recency.remove(&(entry.last_seen, key_bytes));
        entry.last_seen = last_seen;
        let inserted = self.recency.insert((last_seen, key_bytes));
        debug_assert!(removed, "test partition must have a recency entry");
        debug_assert!(inserted, "test recency entry must be unique");
        true
    }
}

/// Creates a rate limit exceeded error.
#[must_use]
pub fn rate_limit_error(message: impl Into<String>) -> McpError {
    McpError::new(McpErrorCode::Custom(RATE_LIMIT_ERROR_CODE), message)
}

/// Token bucket implementation for rate limiting.
///
/// The token bucket algorithm allows for burst traffic while maintaining
/// a sustainable long-term rate. Tokens are added at a constant rate and
/// consumed when requests arrive.
#[derive(Debug)]
pub struct TokenBucketRateLimiter {
    /// Maximum number of tokens in the bucket.
    capacity: usize,
    /// Tokens added per second.
    refill_rate: f64,
    /// Current number of tokens (as f64 for fractional tokens).
    tokens: Mutex<f64>,
    /// Last time tokens were refilled.
    last_refill: Mutex<Instant>,
}

impl TokenBucketRateLimiter {
    /// Creates a new token bucket rate limiter.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of tokens (burst capacity)
    /// * `refill_rate` - Tokens added per second (sustained rate)
    #[must_use]
    pub fn new(capacity: usize, refill_rate: f64) -> Self {
        Self::new_at(capacity, refill_rate, Instant::now())
    }

    fn new_at(capacity: usize, refill_rate: f64, now: Instant) -> Self {
        let refill_rate = sanitized_rate(refill_rate);
        let capacity = if refill_rate > 0.0 && capacity <= MAX_EXACT_TOKEN_CAPACITY {
            capacity
        } else {
            0
        };
        Self {
            capacity,
            refill_rate,
            tokens: Mutex::new(capacity as f64),
            last_refill: Mutex::new(now),
        }
    }

    /// Tries to consume tokens from the bucket.
    ///
    /// Returns `true` if tokens were available and consumed, `false` otherwise.
    pub fn try_consume(&self, tokens: usize) -> bool {
        self.try_consume_with_retry(tokens).is_allowed()
    }

    fn try_consume_with_retry(&self, tokens: usize) -> RateLimitAdmission {
        self.try_consume_with_retry_at(tokens, Instant::now())
    }

    fn try_consume_with_retry_at(&self, tokens: usize, now: Instant) -> RateLimitAdmission {
        let mut current_tokens = self
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut last_refill = self
            .last_refill
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Requests can sample time before contending for the limiter lock.
        // Never rewind refill state when a later observation acquired it first.
        let now = now.max(*last_refill);
        let elapsed = now.duration_since(*last_refill).as_secs_f64();

        // Add tokens based on elapsed time
        *current_tokens = (*current_tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        *last_refill = now;

        let tokens_needed = tokens as f64;
        if *current_tokens >= tokens_needed {
            *current_tokens -= tokens_needed;
            RateLimitAdmission::Allowed
        } else {
            RateLimitAdmission::Rejected {
                retry_after_ms: retry_after_millis(
                    tokens_needed - *current_tokens,
                    self.refill_rate,
                ),
            }
        }
    }

    /// Returns the current number of available tokens.
    #[must_use]
    pub fn available_tokens(&self) -> f64 {
        self.available_tokens_at(Instant::now())
    }

    fn available_tokens_at(&self, now: Instant) -> f64 {
        let mut current_tokens = self
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut last_refill = self
            .last_refill
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let now = now.max(*last_refill);
        let elapsed = now.duration_since(*last_refill).as_secs_f64();

        // Update tokens without consuming
        *current_tokens = (*current_tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        *last_refill = now;

        *current_tokens
    }

    #[cfg(test)]
    fn is_fully_refilled(&self) -> bool {
        self.available_tokens() >= self.capacity as f64
    }
}

/// Sliding window rate limiter implementation.
///
/// Tracks individual request timestamps within a time window for precise
/// rate limiting. More memory-intensive than token bucket but provides
/// exact request counting.
#[derive(Debug)]
pub struct SlidingWindowRateLimiter {
    /// Maximum requests allowed in the time window.
    max_requests: usize,
    /// Time window in seconds.
    window_seconds: u64,
    /// Request timestamps (as durations from a fixed start time).
    requests: Mutex<VecDeque<Instant>>,
}

impl SlidingWindowRateLimiter {
    /// Creates a new sliding window rate limiter.
    ///
    /// # Arguments
    ///
    /// * `max_requests` - Maximum requests allowed in the time window
    /// * `window_seconds` - Time window duration in seconds
    #[must_use]
    pub fn new(max_requests: usize, window_seconds: u64) -> Self {
        Self {
            max_requests,
            window_seconds,
            requests: Mutex::new(VecDeque::new()),
        }
    }

    /// Checks if a request is allowed under the rate limit.
    ///
    /// If allowed, records the request timestamp and returns `true`.
    /// Otherwise returns `false`.
    pub fn is_allowed(&self) -> bool {
        self.is_allowed_with_retry().is_allowed()
    }

    fn is_allowed_with_retry(&self) -> RateLimitAdmission {
        if self.window_seconds == 0 {
            return RateLimitAdmission::Rejected {
                retry_after_ms: u64::MAX,
            };
        }

        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let now = Instant::now();
        let cutoff = now.checked_sub(std::time::Duration::from_secs(self.window_seconds));

        // Remove old requests outside the window
        if let Some(cutoff) = cutoff {
            while let Some(&oldest) = requests.front() {
                if oldest < cutoff {
                    requests.pop_front();
                } else {
                    break;
                }
            }
        }

        if requests.len() < self.max_requests {
            requests.push_back(now);
            RateLimitAdmission::Allowed
        } else {
            let retry_after_ms = requests.front().map_or(u64::MAX, |oldest| {
                let elapsed = now.saturating_duration_since(*oldest);
                let window = Duration::from_secs(self.window_seconds);
                if elapsed >= window {
                    1
                } else {
                    u64::try_from((window - elapsed).as_millis())
                        .unwrap_or(u64::MAX)
                        .saturating_add(1)
                }
            });
            RateLimitAdmission::Rejected { retry_after_ms }
        }
    }

    /// Returns the current number of requests in the window.
    #[must_use]
    pub fn current_requests(&self) -> usize {
        if self.window_seconds == 0 {
            return 0;
        }

        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let now = Instant::now();
        let cutoff = now.checked_sub(std::time::Duration::from_secs(self.window_seconds));

        // Remove old requests outside the window
        if let Some(cutoff) = cutoff {
            while let Some(&oldest) = requests.front() {
                if oldest < cutoff {
                    requests.pop_front();
                } else {
                    break;
                }
            }
        }

        requests.len()
    }
}

/// Function type for extracting client ID from request context.
pub type ClientIdExtractor =
    Box<dyn Fn(&McpContext, &JsonRpcRequest) -> Option<String> + Send + Sync>;

/// Rate limiting middleware using token bucket algorithm.
///
/// Uses a token bucket algorithm by default, allowing for burst traffic
/// while maintaining a sustainable long-term rate.
///
/// # Example
///
/// ```ignore
/// use fastmcp_server::rate_limiting::RateLimitingMiddleware;
///
/// // Allow 10 requests per second with bursts up to 20
/// let rate_limiter = RateLimitingMiddleware::new(10.0)
///     .burst_capacity(20);
/// ```
pub struct RateLimitingMiddleware {
    /// Sustained requests per second allowed.
    max_requests_per_second: f64,
    /// Maximum burst capacity.
    burst_capacity: usize,
    /// Function to extract client ID for method-scoped client partitions.
    get_client_id: Option<ClientIdExtractor>,
    /// If true, apply limit globally; if false, per-client.
    global_limit: bool,
    /// Storage for a bounded number of fixed-width client partitions.
    limiters: Mutex<PartitionStore<TokenBucketRateLimiter>>,
    /// Minimum inactivity required before safe least-recently-used eviction.
    partition_idle_ttl: Duration,
    /// Shared partition for new identifiers after the named-partition cap.
    overflow_limiter: TokenBucketRateLimiter,
    /// Global rate limiter (used when global_limit is true).
    global_limiter: Option<TokenBucketRateLimiter>,
}

impl std::fmt::Debug for RateLimitingMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimitingMiddleware")
            .field("max_requests_per_second", &self.max_requests_per_second)
            .field("burst_capacity", &self.burst_capacity)
            .field("global_limit", &self.global_limit)
            .finish()
    }
}

impl RateLimitingMiddleware {
    /// Creates a new rate limiting middleware with the specified rate.
    ///
    /// # Arguments
    ///
    /// * `max_requests_per_second` - Sustained requests per second allowed
    ///
    /// Burst capacity defaults to 2x the sustained rate.
    #[must_use]
    pub fn new(max_requests_per_second: f64) -> Self {
        let max_requests_per_second = sanitized_rate(max_requests_per_second);
        let burst_capacity = default_burst_capacity(max_requests_per_second);
        Self {
            max_requests_per_second,
            burst_capacity,
            get_client_id: None,
            global_limit: false,
            limiters: Mutex::new(PartitionStore::new()),
            partition_idle_ttl: CLIENT_PARTITION_IDLE_TTL,
            overflow_limiter: TokenBucketRateLimiter::new(burst_capacity, max_requests_per_second),
            global_limiter: None,
        }
    }

    /// Sets the burst capacity (maximum tokens in the bucket).
    #[must_use]
    pub fn burst_capacity(mut self, capacity: usize) -> Self {
        let capacity = if capacity <= MAX_EXACT_TOKEN_CAPACITY {
            capacity
        } else {
            0
        };
        self.burst_capacity = capacity;
        self.overflow_limiter = TokenBucketRateLimiter::new(capacity, self.max_requests_per_second);
        // Re-create global limiter if it exists
        if self.global_limit {
            self.global_limiter = Some(TokenBucketRateLimiter::new(
                capacity,
                self.max_requests_per_second,
            ));
        }
        self
    }

    /// Sets a custom function to extract client ID from the request context.
    ///
    /// Identifiers longer than 4096 bytes are rejected. Accepted identifiers
    /// are immediately reduced to fixed-width SHA-256 partition keys; their raw
    /// values are neither retained nor included in rate-limit errors or debug
    /// output. At most 4095 distinct keys receive dedicated partitions. At the
    /// cap, a least-recently-used partition idle for at least 60 seconds is
    /// reclaimed only after its limiter has naturally reset; otherwise new
    /// keys share one overflow partition.
    ///
    /// If not set, all clients share the default identity while each method
    /// retains an independent rate-limit partition.
    #[must_use]
    pub fn client_id_extractor<F>(mut self, extractor: F) -> Self
    where
        F: Fn(&McpContext, &JsonRpcRequest) -> Option<String> + Send + Sync + 'static,
    {
        self.get_client_id = Some(Box::new(extractor));
        self
    }

    #[cfg(test)]
    fn with_partition_idle_ttl(mut self, idle_ttl: Duration) -> Self {
        self.partition_idle_ttl = idle_ttl;
        self
    }

    /// Enables global rate limiting (all clients share one limit).
    ///
    /// When enabled, all requests count against a single rate limit
    /// regardless of client identity.
    #[must_use]
    pub fn global(mut self) -> Self {
        self.global_limit = true;
        self.global_limiter = Some(TokenBucketRateLimiter::new(
            self.burst_capacity,
            self.max_requests_per_second,
        ));
        self
    }

    fn client_partition_key(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<Sha256Digest> {
        if let Some(ref extractor) = self.get_client_id {
            if let Some(id) = extractor(ctx, request) {
                return sha256_bounded(id.as_bytes(), MAX_CLIENT_ID_BYTES)
                    .map_err(|_| rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE));
            }
        }
        default_client_partition()
    }

    fn request_partition_key(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<Sha256Digest> {
        rate_limit_method_partition(self.client_partition_key(ctx, request)?, &request.method)
    }

    fn get_or_create_limiter_with_retry_at(
        &self,
        partition: Sha256Digest,
        now: Instant,
    ) -> RateLimitAdmission {
        let mut limiters = self
            .limiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(admission) = limiters.use_existing_at(partition, now, |limiter| {
            limiter.try_consume_with_retry_at(1, now)
        }) {
            return admission;
        }

        if limiters.len() >= MAX_NAMED_CLIENT_PARTITIONS
            && !limiters.reclaim_oldest_if(now, self.partition_idle_ttl, |limiter| {
                limiter.available_tokens_at(now) >= limiter.capacity as f64
            })
        {
            return self.overflow_limiter.try_consume_with_retry_at(1, now);
        }

        let limiter =
            TokenBucketRateLimiter::new_at(self.burst_capacity, self.max_requests_per_second, now);
        let admission = limiter.try_consume_with_retry_at(1, now);
        limiters.insert_at(partition, limiter, now);
        admission
    }

    #[cfg(test)]
    fn get_or_create_limiter(&self, partition: Sha256Digest) -> bool {
        self.get_or_create_limiter_with_retry_at(partition, Instant::now())
            .is_allowed()
    }
}

impl RateLimitingMiddleware {
    fn on_request_at(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
        now: Instant,
    ) -> McpResult<MiddlewareDecision> {
        ctx.ensure_live().map_err(McpError::from)?;
        if self.max_requests_per_second <= 0.0 || self.burst_capacity == 0 {
            return Err(rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE));
        }

        let admission = if self.global_limit {
            // Global rate limiting
            if let Some(ref limiter) = self.global_limiter {
                limiter.try_consume_with_retry_at(1, now)
            } else {
                RateLimitAdmission::Rejected {
                    retry_after_ms: u64::MAX,
                }
            }
        } else {
            // The request ID remains correlation-only. Including it in the
            // bucket key would let a retry with a fresh JSON-RPC ID evade the
            // method's configured admission budget.
            let partition = self.request_partition_key(ctx, request)?;
            self.get_or_create_limiter_with_retry_at(partition, now)
        };

        ctx.ensure_live().map_err(McpError::from)?;
        match admission {
            RateLimitAdmission::Allowed => Ok(MiddlewareDecision::Continue),
            RateLimitAdmission::Rejected { retry_after_ms } => {
                Err(rate_limit_retry_error(request, retry_after_ms))
            }
        }
    }
}

impl Middleware for RateLimitingMiddleware {
    fn on_request(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        self.on_request_at(ctx, request, Instant::now())
    }
}

/// Rate limiting middleware using sliding window algorithm.
///
/// Uses a sliding window approach which provides more precise rate limiting
/// but uses more memory to track individual request timestamps.
///
/// # Example
///
/// ```ignore
/// use fastmcp_server::rate_limiting::SlidingWindowRateLimitingMiddleware;
///
/// // Allow 100 requests per minute
/// let rate_limiter = SlidingWindowRateLimitingMiddleware::new(100, 60);
/// ```
pub struct SlidingWindowRateLimitingMiddleware {
    /// Maximum requests allowed in the time window.
    max_requests: usize,
    /// Time window in seconds.
    window_seconds: u64,
    /// Function to extract client ID for method-scoped client partitions.
    get_client_id: Option<ClientIdExtractor>,
    /// Storage for a bounded number of fixed-width client partitions.
    limiters: Mutex<PartitionStore<SlidingWindowRateLimiter>>,
    /// Minimum inactivity required before safe least-recently-used eviction.
    partition_idle_ttl: Duration,
    /// Shared partition for new identifiers after the named-partition cap.
    overflow_limiter: SlidingWindowRateLimiter,
}

impl std::fmt::Debug for SlidingWindowRateLimitingMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlidingWindowRateLimitingMiddleware")
            .field("max_requests", &self.max_requests)
            .field("window_seconds", &self.window_seconds)
            .finish()
    }
}

impl SlidingWindowRateLimitingMiddleware {
    /// Creates a new sliding window rate limiting middleware.
    ///
    /// # Arguments
    ///
    /// * `max_requests` - Maximum requests allowed in the time window
    /// * `window_seconds` - Time window duration in seconds
    #[must_use]
    pub fn new(max_requests: usize, window_seconds: u64) -> Self {
        Self {
            max_requests,
            window_seconds,
            get_client_id: None,
            limiters: Mutex::new(PartitionStore::new()),
            partition_idle_ttl: CLIENT_PARTITION_IDLE_TTL,
            overflow_limiter: SlidingWindowRateLimiter::new(max_requests, window_seconds),
        }
    }

    /// Creates a sliding window rate limiter with minutes-based window.
    ///
    /// # Arguments
    ///
    /// * `max_requests` - Maximum requests allowed in the time window
    /// * `window_minutes` - Time window duration in minutes
    #[must_use]
    pub fn per_minute(max_requests: usize, window_minutes: u64) -> Self {
        Self::new(max_requests, window_minutes.checked_mul(60).unwrap_or(0))
    }

    /// Sets a custom function to extract client ID from the request context.
    ///
    /// Identifiers longer than 4096 bytes are rejected. Accepted identifiers
    /// are immediately reduced to fixed-width SHA-256 partition keys; their raw
    /// values are neither retained nor included in rate-limit errors or debug
    /// output. At most 4095 distinct keys receive dedicated partitions. At the
    /// cap, a least-recently-used partition idle for at least 60 seconds is
    /// reclaimed only after its limiter has naturally reset; otherwise new
    /// keys share one overflow partition.
    #[must_use]
    pub fn client_id_extractor<F>(mut self, extractor: F) -> Self
    where
        F: Fn(&McpContext, &JsonRpcRequest) -> Option<String> + Send + Sync + 'static,
    {
        self.get_client_id = Some(Box::new(extractor));
        self
    }

    #[cfg(test)]
    fn with_partition_idle_ttl(mut self, idle_ttl: Duration) -> Self {
        self.partition_idle_ttl = idle_ttl;
        self
    }

    fn client_partition_key(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<Sha256Digest> {
        if let Some(ref extractor) = self.get_client_id {
            if let Some(id) = extractor(ctx, request) {
                return sha256_bounded(id.as_bytes(), MAX_CLIENT_ID_BYTES)
                    .map_err(|_| rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE));
            }
        }
        default_client_partition()
    }

    fn request_partition_key(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<Sha256Digest> {
        rate_limit_method_partition(self.client_partition_key(ctx, request)?, &request.method)
    }

    fn is_request_allowed_with_retry(&self, partition: Sha256Digest) -> RateLimitAdmission {
        let mut limiters = self
            .limiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(admission) =
            limiters.use_existing(partition, |limiter| limiter.is_allowed_with_retry())
        {
            return admission;
        }

        if limiters.len() >= MAX_NAMED_CLIENT_PARTITIONS
            && !limiters.reclaim_oldest_if(Instant::now(), self.partition_idle_ttl, |limiter| {
                limiter.current_requests() == 0
            })
        {
            return self.overflow_limiter.is_allowed_with_retry();
        }

        let limiter = SlidingWindowRateLimiter::new(self.max_requests, self.window_seconds);
        let admission = limiter.is_allowed_with_retry();
        limiters.insert(partition, limiter);
        admission
    }

    fn is_request_allowed(&self, partition: Sha256Digest) -> bool {
        self.is_request_allowed_with_retry(partition).is_allowed()
    }
}

impl Middleware for SlidingWindowRateLimitingMiddleware {
    fn on_request(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        ctx.ensure_live().map_err(McpError::from)?;
        if self.max_requests == 0 || self.window_seconds == 0 {
            return Err(rate_limit_error(RATE_LIMIT_EXCEEDED_MESSAGE));
        }

        let partition = self.request_partition_key(ctx, request)?;
        let admission = self.is_request_allowed_with_retry(partition);

        ctx.ensure_live().map_err(McpError::from)?;
        match admission {
            RateLimitAdmission::Allowed => Ok(MiddlewareDecision::Continue),
            RateLimitAdmission::Rejected { retry_after_ms } => {
                Err(rate_limit_retry_error(request, retry_after_ms))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;

    fn test_context() -> McpContext {
        let cx = Cx::for_testing();
        McpContext::new(cx, 1)
    }

    fn test_request(method: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: std::borrow::Cow::Borrowed(fastmcp_protocol::JSONRPC_VERSION),
            method: method.to_string(),
            params: None,
            id: Some(fastmcp_protocol::RequestId::Number(1)),
        }
    }

    /// Live partitions are scoped by client AND method (aced504); tests that
    /// address stored partitions directly must compose the same key. These
    /// suites extract the client id from the request method, so both digest
    /// stages consume the same string.
    fn method_scoped_test_key(id: &str) -> Sha256Digest {
        let client = sha256_bounded(id.as_bytes(), MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        rate_limit_method_partition(client, id)
            .expect("test method partition input is within the bound")
    }

    fn modern_test_request(method: &str, id: &str) -> JsonRpcRequest {
        JsonRpcRequest::new(
            method,
            None,
            fastmcp_protocol::RequestId::String(id.to_string()),
        )
    }

    // ========================================
    // TokenBucketRateLimiter tests
    // ========================================

    #[test]
    fn test_token_bucket_allows_burst() {
        let limiter = TokenBucketRateLimiter::new(5, 1.0);

        // Should allow burst up to capacity
        assert!(limiter.try_consume(1));
        assert!(limiter.try_consume(1));
        assert!(limiter.try_consume(1));
        assert!(limiter.try_consume(1));
        assert!(limiter.try_consume(1));

        // Should deny once capacity exhausted
        assert!(!limiter.try_consume(1));
    }

    #[test]
    fn test_token_bucket_refills_over_time() {
        let limiter = TokenBucketRateLimiter::new(2, 100.0); // 100 tokens per second

        // Exhaust tokens
        assert!(limiter.try_consume(1));
        assert!(limiter.try_consume(1));
        assert!(!limiter.try_consume(1));

        // Wait for refill (10ms should add ~1 token at 100 t/s)
        std::thread::sleep(std::time::Duration::from_millis(15));

        // Should have refilled
        assert!(limiter.try_consume(1));
    }

    #[test]
    fn test_token_bucket_available_tokens() {
        let limiter = TokenBucketRateLimiter::new(10, 1.0);
        assert!((limiter.available_tokens() - 10.0).abs() < 0.1);

        limiter.try_consume(5);
        assert!((limiter.available_tokens() - 5.0).abs() < 0.1);
    }

    // ========================================
    // SlidingWindowRateLimiter tests
    // ========================================

    #[test]
    fn test_sliding_window_allows_up_to_limit() {
        let limiter = SlidingWindowRateLimiter::new(3, 60);

        assert!(limiter.is_allowed());
        assert!(limiter.is_allowed());
        assert!(limiter.is_allowed());
        assert!(!limiter.is_allowed()); // Fourth request denied
    }

    #[test]
    fn test_sliding_window_current_requests() {
        let limiter = SlidingWindowRateLimiter::new(10, 60);

        assert_eq!(limiter.current_requests(), 0);
        limiter.is_allowed();
        assert_eq!(limiter.current_requests(), 1);
        limiter.is_allowed();
        assert_eq!(limiter.current_requests(), 2);
    }

    // ========================================
    // RateLimitingMiddleware tests
    // ========================================

    #[test]
    fn test_rate_limiting_middleware_allows_initial_requests() {
        let middleware = RateLimitingMiddleware::new(10.0).global();
        let ctx = test_context();
        let request = test_request("tools/call");

        let result = middleware.on_request(&ctx, &request);
        assert!(matches!(result, Ok(MiddlewareDecision::Continue)));
    }

    #[test]
    fn test_rate_limiting_middleware_denies_after_burst() {
        let middleware = RateLimitingMiddleware::new(10.0).burst_capacity(2).global();
        let ctx = test_context();
        let request = test_request("tools/call");

        // First two should succeed (burst capacity = 2)
        assert!(middleware.on_request(&ctx, &request).is_ok());
        assert!(middleware.on_request(&ctx, &request).is_ok());

        // Third should fail
        let result = middleware.on_request(&ctx, &request);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(i32::from(err.code), RATE_LIMIT_ERROR_CODE);
        assert_eq!(err.message, RATE_LIMIT_EXCEEDED_MESSAGE);
    }

    #[test]
    fn test_rate_limiting_middleware_per_client() {
        let middleware = RateLimitingMiddleware::new(10.0)
            .burst_capacity(1)
            .client_id_extractor(|_ctx, req| {
                req.params
                    .as_ref()?
                    .get("client")?
                    .as_str()
                    .map(str::to_owned)
            });
        let ctx = test_context();
        let mut request1 = test_request("tools/call");
        request1.params = Some(serde_json::json!({"client": "first"}));
        let mut request2 = test_request("tools/call");
        request2.params = Some(serde_json::json!({"client": "second"}));
        let now = Instant::now();

        // Same method, different clients: method partitioning alone cannot
        // satisfy this assertion. No scheduler speed assumption controls refill.
        assert!(middleware.on_request_at(&ctx, &request1, now).is_ok());
        assert!(middleware.on_request_at(&ctx, &request2, now).is_ok());
        assert!(middleware.on_request_at(&ctx, &request1, now).is_err());
        assert!(middleware.on_request_at(&ctx, &request2, now).is_err());
    }

    fn rate_limit_clock_boundary_probe(refilled: bool) {
        let middleware = RateLimitingMiddleware::new(10.0).burst_capacity(1);
        let ctx = test_context();
        let request = test_request("tools/call");
        let now = Instant::now();
        assert!(middleware.on_request_at(&ctx, &request, now).is_ok());
        let observed = now + Duration::from_millis(if refilled { 100 } else { 99 });
        let result = middleware.on_request_at(&ctx, &request, observed);
        if refilled {
            assert!(matches!(result, Ok(MiddlewareDecision::Continue)));
        } else {
            let error = result.unwrap_err();
            assert_eq!(i32::from(error.code), RATE_LIMIT_ERROR_CODE);
            assert_eq!(error.message, RATE_LIMIT_EXCEEDED_MESSAGE);
            // Millisecond rounding may conservatively add one millisecond.
            let retry = error.data.unwrap()["retryAfterMs"].as_u64().unwrap();
            assert!((1..=2).contains(&retry));
        }
        // An observation taken before the previous request acquired the lock
        // cannot rewind refill state and mint another token at the same time.
        assert!(middleware.on_request_at(&ctx, &request, now).is_err());
        assert!(middleware.on_request_at(&ctx, &request, observed).is_err());
        let partition = middleware.request_partition_key(&ctx, &request).unwrap();
        let limiters = middleware.limiters.lock().unwrap();
        let entry = &limiters.entries[&partition];
        assert_eq!(entry.last_seen, observed);
        assert_eq!(*entry.limiter.last_refill.lock().unwrap(), observed);
    }

    #[test]
    fn rate_limit_clock_boundary_positive() {
        rate_limit_clock_boundary_probe(true);
    }

    #[test]
    fn rate_limit_clock_boundary_planted_negative() {
        rate_limit_clock_boundary_probe(false);
    }

    // ========================================
    // SlidingWindowRateLimitingMiddleware tests
    // ========================================

    #[test]
    fn test_sliding_window_middleware_allows_up_to_limit() {
        let middleware = SlidingWindowRateLimitingMiddleware::new(2, 60);
        let ctx = test_context();
        let request = test_request("tools/call");

        assert!(middleware.on_request(&ctx, &request).is_ok());
        assert!(middleware.on_request(&ctx, &request).is_ok());

        let result = middleware.on_request(&ctx, &request);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(i32::from(err.code), RATE_LIMIT_ERROR_CODE);
    }

    #[test]
    fn test_sliding_window_middleware_per_minute() {
        let middleware = SlidingWindowRateLimitingMiddleware::per_minute(100, 1);
        let ctx = test_context();
        let request = test_request("tools/call");

        // Should allow many requests
        for _ in 0..100 {
            assert!(middleware.on_request(&ctx, &request).is_ok());
        }

        // 101st should fail
        assert!(middleware.on_request(&ctx, &request).is_err());
    }

    #[test]
    fn test_rate_limit_error_code() {
        let err = rate_limit_error("test");
        assert_eq!(i32::from(err.code), RATE_LIMIT_ERROR_CODE);
        assert_eq!(err.message, "test");
    }

    // ========================================
    // rate_limit_error / RATE_LIMIT_ERROR_CODE
    // ========================================

    #[test]
    fn rate_limit_error_code_value() {
        assert_eq!(RATE_LIMIT_ERROR_CODE, -32005);
    }

    #[test]
    fn rate_limit_error_from_string() {
        let err = rate_limit_error(String::from("custom message"));
        assert_eq!(err.message, "custom message");
        assert_eq!(i32::from(err.code), RATE_LIMIT_ERROR_CODE);
    }

    // ========================================
    // TokenBucketRateLimiter — additional
    // ========================================

    #[test]
    fn token_bucket_debug() {
        let limiter = TokenBucketRateLimiter::new(10, 5.0);
        let debug = format!("{:?}", limiter);
        assert!(debug.contains("TokenBucketRateLimiter"));
        assert!(debug.contains("10"));
    }

    #[test]
    fn token_bucket_consume_multiple_at_once() {
        let limiter = TokenBucketRateLimiter::new(10, 1.0);
        // Consume 5 at once — should succeed
        assert!(limiter.try_consume(5));
        // Consume another 5 — should succeed (exactly 10 tokens)
        assert!(limiter.try_consume(5));
        // No tokens left
        assert!(!limiter.try_consume(1));
    }

    #[test]
    fn token_bucket_consume_more_than_capacity() {
        let limiter = TokenBucketRateLimiter::new(5, 1.0);
        // Request more than capacity — should fail immediately
        assert!(!limiter.try_consume(6));
        // Bucket still has tokens (nothing was consumed on failure)
        assert!(limiter.try_consume(5));
    }

    #[test]
    fn token_bucket_available_tokens_caps_at_capacity() {
        let limiter = TokenBucketRateLimiter::new(5, 1000.0); // Very high refill
        // Even with high refill rate, wait a bit — should not exceed capacity
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(limiter.available_tokens() <= 5.0 + 0.1);
    }

    #[test]
    fn token_bucket_available_tokens_after_full_drain() {
        let limiter = TokenBucketRateLimiter::new(3, 1.0);
        limiter.try_consume(3);
        assert!(limiter.available_tokens() < 1.0);
    }

    // ========================================
    // SlidingWindowRateLimiter — additional
    // ========================================

    #[test]
    fn sliding_window_debug() {
        let limiter = SlidingWindowRateLimiter::new(100, 60);
        let debug = format!("{:?}", limiter);
        assert!(debug.contains("SlidingWindowRateLimiter"));
        assert!(debug.contains("100"));
    }

    #[test]
    fn sliding_window_current_requests_starts_at_zero() {
        let limiter = SlidingWindowRateLimiter::new(10, 60);
        assert_eq!(limiter.current_requests(), 0);
    }

    #[test]
    fn sliding_window_denied_request_not_counted() {
        let limiter = SlidingWindowRateLimiter::new(2, 60);
        assert!(limiter.is_allowed());
        assert!(limiter.is_allowed());
        assert!(!limiter.is_allowed()); // denied
        // Only 2 requests counted (not the denied one)
        assert_eq!(limiter.current_requests(), 2);
    }

    // ========================================
    // RateLimitingMiddleware — construction/Debug
    // ========================================

    #[test]
    fn rate_limiting_middleware_default_burst_capacity() {
        let m = RateLimitingMiddleware::new(10.0);
        // Default burst capacity is 2x rate = 20
        assert_eq!(m.burst_capacity, 20);
        assert!(!m.global_limit);
        assert!(m.global_limiter.is_none());
        assert!(m.get_client_id.is_none());
    }

    #[test]
    fn fractional_positive_rates_have_a_nonzero_default_burst() {
        for rate in [0.1, 0.49] {
            let middleware = RateLimitingMiddleware::new(rate).global();
            assert_eq!(middleware.burst_capacity, 1);

            let ctx = test_context();
            let request = test_request("tools/call");
            assert!(middleware.on_request(&ctx, &request).is_ok());
            assert!(middleware.on_request(&ctx, &request).is_err());
        }
    }

    #[test]
    fn invalid_rates_remain_fail_closed() {
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let middleware = RateLimitingMiddleware::new(rate).global();
            assert_eq!(middleware.burst_capacity, 0);

            let ctx = test_context();
            let request = test_request("tools/call");
            assert!(middleware.on_request(&ctx, &request).is_err());
        }
    }

    #[test]
    fn rate_limiting_middleware_debug() {
        let m = RateLimitingMiddleware::new(10.0)
            .burst_capacity(30)
            .global();
        let debug = format!("{:?}", m);
        assert!(debug.contains("RateLimitingMiddleware"));
        assert!(debug.contains("30"));
        assert!(debug.contains("true")); // global_limit
    }

    #[test]
    fn rate_limiting_middleware_global_creates_limiter() {
        let m = RateLimitingMiddleware::new(5.0).global();
        assert!(m.global_limit);
        assert!(m.global_limiter.is_some());
    }

    #[test]
    fn rate_limiting_middleware_burst_capacity_without_global() {
        let m = RateLimitingMiddleware::new(10.0).burst_capacity(50);
        // No global limiter created when not in global mode
        assert!(m.global_limiter.is_none());
        assert_eq!(m.burst_capacity, 50);
    }

    #[test]
    fn rate_limiting_middleware_burst_capacity_with_global_recreates_limiter() {
        let m = RateLimitingMiddleware::new(10.0).global().burst_capacity(3);
        assert_eq!(m.burst_capacity, 3);
        // Global limiter should exist with new capacity
        assert!(m.global_limiter.is_some());

        let ctx = test_context();
        let req = test_request("test");
        // Should allow exactly 3 requests (burst capacity)
        assert!(m.on_request(&ctx, &req).is_ok());
        assert!(m.on_request(&ctx, &req).is_ok());
        assert!(m.on_request(&ctx, &req).is_ok());
        assert!(m.on_request(&ctx, &req).is_err());
    }

    // ========================================
    // RateLimitingMiddleware — client ID extraction
    // ========================================

    #[test]
    fn rate_limiting_middleware_no_extractor_uses_global_key() {
        let m = RateLimitingMiddleware::new(10.0);
        let ctx = test_context();
        let req = test_request("tools/call");
        let partition = m
            .client_partition_key(&ctx, &req)
            .expect("default partition must be valid");
        assert_eq!(
            partition,
            default_client_partition().expect("default partition must be valid")
        );
    }

    #[test]
    fn rate_limiting_middleware_extractor_returning_none_uses_global() {
        let m = RateLimitingMiddleware::new(10.0).client_id_extractor(|_ctx, _req| None);
        let ctx = test_context();
        let req = test_request("tools/call");
        let partition = m
            .client_partition_key(&ctx, &req)
            .expect("default partition must be valid");
        assert_eq!(
            partition,
            default_client_partition().expect("default partition must be valid")
        );
    }

    #[test]
    fn rate_limiting_middleware_extractor_returning_some() {
        let m = RateLimitingMiddleware::new(10.0)
            .client_id_extractor(|_ctx, _req| Some("user-42".to_string()));
        let ctx = test_context();
        let req = test_request("tools/call");
        let partition = m
            .client_partition_key(&ctx, &req)
            .expect("bounded custom partition must be valid");
        let expected = sha256_bounded(b"user-42", MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        assert_eq!(partition, expected);
    }

    // ========================================
    // RateLimitingMiddleware — per-client without extractor
    // ========================================

    #[test]
    fn rate_limiting_middleware_without_extractor_is_method_scoped() {
        // Without an extractor, all clients share the default identity, but
        // each validated method retains its own admission budget.
        let m = RateLimitingMiddleware::new(10.0).burst_capacity(2);
        let ctx = test_context();
        let req_a = test_request("method_a");
        let req_b = test_request("method_b");

        // Distinct methods use distinct partitions.
        assert!(m.on_request(&ctx, &req_a).is_ok());
        assert!(m.on_request(&ctx, &req_b).is_ok());
        assert!(m.on_request(&ctx, &req_a).is_ok());
        assert!(m.on_request(&ctx, &req_b).is_ok());
        // Each method exhausts only its own bucket.
        assert!(m.on_request(&ctx, &req_a).is_err());
        assert!(m.on_request(&ctx, &req_b).is_err());
    }

    #[test]
    fn modern_rate_limit_allows_a_distinct_method_for_the_same_client() {
        let middleware = RateLimitingMiddleware::new(1.0e-300)
            .burst_capacity(1)
            .client_id_extractor(|_ctx, _request| Some("modern-tenant".to_string()));
        let first_ctx = McpContext::new(Cx::for_testing(), 41);
        let retry_ctx = McpContext::new(Cx::for_testing(), 42);
        let first = modern_test_request("tools/call", "first-id");
        let distinct_method = modern_test_request("resources/read", "retry-id");

        assert!(middleware.on_request(&first_ctx, &first).is_ok());
        assert!(middleware.on_request(&retry_ctx, &distinct_method).is_ok());
    }

    #[test]
    fn modern_rate_limit_rejects_a_new_id_retry_for_the_same_method() {
        let middleware = RateLimitingMiddleware::new(1.0e-300)
            .burst_capacity(1)
            .client_id_extractor(|_ctx, _request| Some("modern-tenant".to_string()));
        let first_ctx = McpContext::new(Cx::for_testing(), 41);
        let retry_ctx = McpContext::new(Cx::for_testing(), 42);
        let first = modern_test_request("tools/call", "first-id");
        let retry = modern_test_request("tools/call", "retry-id");

        assert!(middleware.on_request(&first_ctx, &first).is_ok());
        let error = middleware
            .on_request(&retry_ctx, &retry)
            .expect_err("RH-5 planted negative: a fresh request ID must not reset a method limit");
        assert_eq!(error.code, McpErrorCode::Custom(RATE_LIMIT_ERROR_CODE));
        assert_eq!(error.message, RATE_LIMIT_EXCEEDED_MESSAGE);
        assert_eq!(
            error.data,
            Some(serde_json::json!({
                "method": "tools/call",
                "requestId": "retry-id",
                "retryAfterMs": u64::MAX,
            }))
        );
    }

    #[test]
    fn cancelled_modern_request_does_not_consume_a_method_limit() {
        let middleware = RateLimitingMiddleware::new(1.0e-300).burst_capacity(1);
        let cancelled_cx = Cx::for_testing();
        cancelled_cx.set_cancel_requested(true);
        let cancelled_ctx = McpContext::new(cancelled_cx, 41);
        let cancelled = modern_test_request("tools/call", "cancelled-id");

        let error = middleware
            .on_request(&cancelled_ctx, &cancelled)
            .expect_err("cancelled requests must not receive an admission token");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);

        let live_ctx = McpContext::new(Cx::for_testing(), 42);
        let live = modern_test_request("tools/call", "live-id");
        assert!(middleware.on_request(&live_ctx, &live).is_ok());
    }

    #[test]
    fn rate_limiting_middleware_error_is_generic_per_client() {
        let m = RateLimitingMiddleware::new(10.0)
            .burst_capacity(1)
            .client_id_extractor(|_ctx, _req| Some("alice".to_string()));
        let ctx = test_context();
        let req = test_request("tools/call");

        m.on_request(&ctx, &req).unwrap();
        let err = m.on_request(&ctx, &req).unwrap_err();
        assert_eq!(err.message, RATE_LIMIT_EXCEEDED_MESSAGE);
        assert!(!err.message.contains("alice"));
    }

    #[test]
    fn rate_limiting_middleware_error_msg_global() {
        let m = RateLimitingMiddleware::new(10.0).burst_capacity(1).global();
        let ctx = test_context();
        let req = test_request("tools/call");

        m.on_request(&ctx, &req).unwrap();
        let err = m.on_request(&ctx, &req).unwrap_err();
        assert_eq!(err.message, RATE_LIMIT_EXCEEDED_MESSAGE);
    }

    // ========================================
    // SlidingWindowRateLimitingMiddleware — construction/Debug
    // ========================================

    #[test]
    fn sliding_window_middleware_new_fields() {
        let m = SlidingWindowRateLimitingMiddleware::new(50, 120);
        assert_eq!(m.max_requests, 50);
        assert_eq!(m.window_seconds, 120);
        assert!(m.get_client_id.is_none());
    }

    #[test]
    fn sliding_window_middleware_per_minute_converts() {
        let m = SlidingWindowRateLimitingMiddleware::per_minute(100, 5);
        assert_eq!(m.max_requests, 100);
        assert_eq!(m.window_seconds, 300); // 5 * 60
    }

    #[test]
    fn sliding_window_middleware_debug() {
        let m = SlidingWindowRateLimitingMiddleware::new(50, 120);
        let debug = format!("{:?}", m);
        assert!(debug.contains("SlidingWindowRateLimitingMiddleware"));
        assert!(debug.contains("50"));
        assert!(debug.contains("120"));
    }

    // ========================================
    // SlidingWindowRateLimitingMiddleware — client ID
    // ========================================

    #[test]
    fn sliding_window_middleware_no_extractor_uses_global() {
        let m = SlidingWindowRateLimitingMiddleware::new(10, 60);
        let ctx = test_context();
        let req = test_request("tools/call");
        let partition = m
            .client_partition_key(&ctx, &req)
            .expect("default partition must be valid");
        assert_eq!(
            partition,
            default_client_partition().expect("default partition must be valid")
        );
    }

    #[test]
    fn sliding_window_middleware_extractor_returning_none_uses_global() {
        let m =
            SlidingWindowRateLimitingMiddleware::new(10, 60).client_id_extractor(|_ctx, _req| None);
        let ctx = test_context();
        let req = test_request("tools/call");
        let partition = m
            .client_partition_key(&ctx, &req)
            .expect("default partition must be valid");
        assert_eq!(
            partition,
            default_client_partition().expect("default partition must be valid")
        );
    }

    #[test]
    fn sliding_window_middleware_extractor_returning_some() {
        let m = SlidingWindowRateLimitingMiddleware::new(10, 60)
            .client_id_extractor(|_ctx, _req| Some("bob".to_string()));
        let ctx = test_context();
        let req = test_request("tools/call");
        let partition = m
            .client_partition_key(&ctx, &req)
            .expect("bounded custom partition must be valid");
        let expected = sha256_bounded(b"bob", MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        assert_eq!(partition, expected);
    }

    // ========================================
    // SlidingWindowRateLimitingMiddleware — per-client
    // ========================================

    #[test]
    fn sliding_window_middleware_per_client() {
        let m = SlidingWindowRateLimitingMiddleware::new(1, 60)
            .client_id_extractor(|_ctx, req| Some(req.method.clone()));
        let ctx = test_context();
        let req_a = test_request("method_a");
        let req_b = test_request("method_b");

        // Each client gets their own window
        assert!(m.on_request(&ctx, &req_a).is_ok());
        assert!(m.on_request(&ctx, &req_b).is_ok());

        // Both exhausted
        assert!(m.on_request(&ctx, &req_a).is_err());
        assert!(m.on_request(&ctx, &req_b).is_err());
    }

    // ========================================
    // SlidingWindowRateLimitingMiddleware — error messages
    // ========================================

    #[test]
    fn sliding_window_middleware_error_is_generic_for_seconds_window() {
        let m = SlidingWindowRateLimitingMiddleware::new(1, 30);
        let ctx = test_context();
        let req = test_request("tools/call");

        m.on_request(&ctx, &req).unwrap();
        let err = m.on_request(&ctx, &req).unwrap_err();
        assert_eq!(err.message, RATE_LIMIT_EXCEEDED_MESSAGE);
    }

    #[test]
    fn sliding_window_middleware_error_is_generic_for_minutes_window() {
        let m = SlidingWindowRateLimitingMiddleware::new(1, 120);
        let ctx = test_context();
        let req = test_request("tools/call");

        m.on_request(&ctx, &req).unwrap();
        let err = m.on_request(&ctx, &req).unwrap_err();
        assert_eq!(err.message, RATE_LIMIT_EXCEEDED_MESSAGE);
    }

    #[test]
    fn sliding_window_middleware_error_omits_client_id() {
        let m = SlidingWindowRateLimitingMiddleware::new(1, 60)
            .client_id_extractor(|_ctx, _req| Some("alice".to_string()));
        let ctx = test_context();
        let req = test_request("tools/call");

        m.on_request(&ctx, &req).unwrap();
        let err = m.on_request(&ctx, &req).unwrap_err();
        assert_eq!(err.message, RATE_LIMIT_EXCEEDED_MESSAGE);
        assert!(!err.message.contains("alice"));
        assert_eq!(i32::from(err.code), RATE_LIMIT_ERROR_CODE);
    }

    // ========================================
    // Edge cases
    // ========================================

    #[test]
    fn rate_limiting_middleware_get_or_create_limiter_creates_new() {
        let m = RateLimitingMiddleware::new(10.0).burst_capacity(2);
        let partition = sha256_bounded(b"new-client", MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        // First call for a new client creates a limiter
        assert!(m.get_or_create_limiter(partition));
        // Second call reuses the same limiter
        assert!(m.get_or_create_limiter(partition));
        // Third call exhausts it
        assert!(!m.get_or_create_limiter(partition));
    }

    #[test]
    fn sliding_window_middleware_is_request_allowed_creates_new() {
        let m = SlidingWindowRateLimitingMiddleware::new(2, 60);
        let c1 = sha256_bounded(b"c1", MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        let c2 = sha256_bounded(b"c2", MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        assert!(m.is_request_allowed(c1));
        assert!(m.is_request_allowed(c1));
        assert!(!m.is_request_allowed(c1));

        // Different client gets its own limiter
        assert!(m.is_request_allowed(c2));
    }

    #[test]
    fn sliding_window_requests_expire_after_window() {
        let limiter = SlidingWindowRateLimiter::new(2, 1); // 2 requests per 1 second
        assert!(limiter.is_allowed());
        assert!(limiter.is_allowed());
        assert!(!limiter.is_allowed()); // exhausted

        // Wait for window to expire
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Requests should be allowed again
        assert!(limiter.is_allowed());
    }

    #[test]
    fn sliding_window_current_requests_resets_after_window() {
        let limiter = SlidingWindowRateLimiter::new(5, 1); // 1 second window
        limiter.is_allowed();
        limiter.is_allowed();
        assert_eq!(limiter.current_requests(), 2);

        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Old requests should have expired
        assert_eq!(limiter.current_requests(), 0);
    }

    #[test]
    fn sliding_window_error_exactly_60_seconds_is_generic() {
        let m = SlidingWindowRateLimitingMiddleware::new(1, 60);
        let ctx = test_context();
        let req = test_request("tools/call");

        m.on_request(&ctx, &req).unwrap();
        let err = m.on_request(&ctx, &req).unwrap_err();
        assert_eq!(err.message, RATE_LIMIT_EXCEEDED_MESSAGE);
    }

    #[test]
    fn token_bucket_try_consume_zero_always_succeeds() {
        let limiter = TokenBucketRateLimiter::new(3, 1.0);
        // Drain all tokens
        limiter.try_consume(3);
        assert!(!limiter.try_consume(1)); // exhausted

        // Consuming zero should still succeed
        assert!(limiter.try_consume(0));
    }

    #[test]
    fn token_bucket_refill_rate_zero_fails_closed() {
        let limiter = TokenBucketRateLimiter::new(2, 0.0); // zero refill rate
        assert!(!limiter.try_consume(2));
        assert!(!limiter.try_consume(1));

        // Even after waiting, no refill
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!limiter.try_consume(1));
    }

    #[test]
    fn token_bucket_invalid_rates_fail_closed() {
        for rate in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let limiter = TokenBucketRateLimiter::new(10, rate);
            assert!(
                !limiter.try_consume(1),
                "invalid rate {rate:?} admitted traffic"
            );
            assert!(limiter.available_tokens().abs() <= f64::EPSILON);

            let middleware = RateLimitingMiddleware::new(rate)
                .burst_capacity(10)
                .global();
            let result = middleware.on_request(&test_context(), &test_request("tools/call"));
            assert!(
                result.is_err(),
                "invalid rate {rate:?} admitted middleware traffic"
            );
        }
    }

    #[test]
    fn token_bucket_exact_integer_capacity_still_decrements() {
        let limiter = TokenBucketRateLimiter::new(MAX_EXACT_TOKEN_CAPACITY, f64::MIN_POSITIVE);

        assert!(limiter.try_consume(1));
        let expected_tokens = (MAX_EXACT_TOKEN_CAPACITY - 1) as f64;
        assert_eq!(
            limiter.available_tokens().total_cmp(&expected_tokens),
            std::cmp::Ordering::Equal
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn token_bucket_inexact_integer_capacity_fails_closed() {
        let inexact_capacity = MAX_EXACT_TOKEN_CAPACITY + 1;
        let limiter = TokenBucketRateLimiter::new(inexact_capacity, 1.0);
        assert!(!limiter.try_consume(1));
        assert!(limiter.available_tokens().abs() <= f64::EPSILON);

        let middleware = RateLimitingMiddleware::new(1.0)
            .burst_capacity(inexact_capacity)
            .global();
        assert_eq!(middleware.burst_capacity, 0);
        assert!(
            middleware
                .on_request(&test_context(), &test_request("tools/call"))
                .is_err()
        );
    }

    #[test]
    fn reclamation_skips_oldest_penalized_partition_for_later_reset_candidate() {
        let oldest_key = sha256_bounded(b"oldest-penalized", MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        let reset_key = sha256_bounded(b"later-reset", MAX_CLIENT_ID_BYTES)
            .expect("test identifier is within the bound");
        let oldest_limiter = TokenBucketRateLimiter::new(1, 1.0e-300);
        assert!(oldest_limiter.try_consume(1));
        let reset_limiter = TokenBucketRateLimiter::new(1, 1.0e-300);

        let mut partitions = PartitionStore::new();
        partitions.insert(oldest_key, oldest_limiter);
        partitions.insert(reset_key, reset_limiter);

        let idle_ttl = Duration::from_millis(1);
        let now = Instant::now();
        let oldest_last_seen = now
            .checked_sub(Duration::from_millis(3))
            .expect("the test idle interval must fit in Instant");
        let reset_last_seen = now
            .checked_sub(Duration::from_millis(2))
            .expect("the test idle interval must fit in Instant");
        assert!(partitions.set_last_seen(oldest_key, oldest_last_seen));
        assert!(partitions.set_last_seen(reset_key, reset_last_seen));

        assert!(partitions.reclaim_oldest_if(
            now,
            idle_ttl,
            TokenBucketRateLimiter::is_fully_refilled,
        ));
        assert!(partitions.contains_key(&oldest_key));
        assert!(!partitions.contains_key(&reset_key));
        assert_eq!(partitions.len(), 1);
        assert_eq!(partitions.recency.len(), 1);
    }

    #[test]
    fn token_bucket_partitions_are_bounded_and_overflow_is_shared() {
        let middleware = RateLimitingMiddleware::new(1.0e-300)
            .burst_capacity(1)
            .client_id_extractor(|_ctx, request| Some(request.method.clone()));
        let ctx = test_context();

        for index in 0..MAX_NAMED_CLIENT_PARTITIONS {
            let request = test_request(&format!("named-client-{index}"));
            assert!(middleware.on_request(&ctx, &request).is_ok());
        }
        assert_eq!(
            middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            MAX_NAMED_CLIENT_PARTITIONS
        );

        assert!(
            middleware
                .on_request(&ctx, &test_request("overflow-client-a"))
                .is_ok()
        );
        assert!(
            middleware
                .on_request(&ctx, &test_request("overflow-client-b"))
                .is_err(),
            "a fresh identifier must not reset the shared overflow limit"
        );
        assert_eq!(
            middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            MAX_NAMED_CLIENT_PARTITIONS
        );
    }

    #[test]
    fn sliding_window_partitions_are_bounded_and_overflow_is_shared() {
        let middleware = SlidingWindowRateLimitingMiddleware::new(1, u64::MAX)
            .client_id_extractor(|_ctx, request| Some(request.method.clone()));
        let ctx = test_context();

        for index in 0..MAX_NAMED_CLIENT_PARTITIONS {
            let request = test_request(&format!("named-client-{index}"));
            assert!(middleware.on_request(&ctx, &request).is_ok());
        }
        assert_eq!(
            middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            MAX_NAMED_CLIENT_PARTITIONS
        );

        assert!(
            middleware
                .on_request(&ctx, &test_request("overflow-client-a"))
                .is_ok()
        );
        assert!(
            middleware
                .on_request(&ctx, &test_request("overflow-client-b"))
                .is_err(),
            "a fresh identifier must not reset the shared overflow limit"
        );
        assert_eq!(
            middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            MAX_NAMED_CLIENT_PARTITIONS
        );
    }

    #[test]
    fn token_bucket_reclaims_reset_stale_partition_and_preserves_recent_client() {
        let idle_ttl = Duration::from_millis(1);
        let middleware = RateLimitingMiddleware::new(1.0e-300)
            .burst_capacity(1)
            .client_id_extractor(|_ctx, request| Some(request.method.clone()))
            .with_partition_idle_ttl(idle_ttl);
        let ctx = test_context();
        let legitimate_id = "recent-legitimate-token-client";

        assert!(
            middleware
                .on_request(&ctx, &test_request(legitimate_id))
                .is_ok()
        );
        for index in 0..(MAX_NAMED_CLIENT_PARTITIONS - 1) {
            assert!(
                middleware
                    .on_request(
                        &ctx,
                        &test_request(&format!("stale-token-attacker-{index}"))
                    )
                    .is_ok()
            );
        }
        assert!(
            middleware
                .on_request(&ctx, &test_request(legitimate_id))
                .is_err(),
            "touching an exhausted legitimate partition must preserve its limit"
        );

        let stale_key = method_scoped_test_key("stale-token-attacker-0");
        let legitimate_key = method_scoped_test_key(legitimate_id);
        let stale_last_seen = Instant::now()
            .checked_sub(idle_ttl + idle_ttl)
            .expect("the test idle interval must fit in Instant");
        {
            let mut partitions = middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(partitions.set_last_seen(stale_key, stale_last_seen));
            assert!(partitions.set_last_seen(legitimate_key, Instant::now()));
        }

        let active_state_probe = "token-active-state-overflow-probe";
        assert!(
            middleware
                .on_request(&ctx, &test_request(active_state_probe))
                .is_ok(),
            "stale but non-reset state must use the shared overflow limiter"
        );
        let active_state_probe_key = method_scoped_test_key(active_state_probe);
        {
            let partitions = middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(!partitions.contains_key(&active_state_probe_key));
        }
        {
            let partitions = middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let stale = partitions
                .entries
                .get(&stale_key)
                .expect("attacker partition must exist");
            let mut tokens = stale
                .limiter
                .tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *tokens = stale.limiter.capacity as f64;
        }

        let newcomer_id = "new-legitimate-token-client";
        assert!(
            middleware
                .on_request(&ctx, &test_request(newcomer_id))
                .is_ok(),
            "a safely reset stale attacker partition should be reclaimed"
        );
        let newcomer_key = method_scoped_test_key(newcomer_id);
        let partitions = middleware
            .limiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!partitions.contains_key(&stale_key));
        assert!(partitions.contains_key(&legitimate_key));
        assert!(partitions.contains_key(&newcomer_key));
        assert_eq!(partitions.len(), MAX_NAMED_CLIENT_PARTITIONS);
        assert_eq!(partitions.recency.len(), MAX_NAMED_CLIENT_PARTITIONS);
        drop(partitions);

        assert!(
            middleware
                .on_request(&ctx, &test_request(legitimate_id))
                .is_err(),
            "the recent legitimate client's exhausted limiter must not be reset"
        );
    }

    #[test]
    fn sliding_window_reclaims_empty_stale_partition_and_preserves_recent_client() {
        let idle_ttl = Duration::from_millis(1);
        let middleware = SlidingWindowRateLimitingMiddleware::new(1, 60)
            .client_id_extractor(|_ctx, request| Some(request.method.clone()))
            .with_partition_idle_ttl(idle_ttl);
        let ctx = test_context();
        let legitimate_id = "recent-legitimate-window-client";

        assert!(
            middleware
                .on_request(&ctx, &test_request(legitimate_id))
                .is_ok()
        );
        for index in 0..(MAX_NAMED_CLIENT_PARTITIONS - 1) {
            assert!(
                middleware
                    .on_request(
                        &ctx,
                        &test_request(&format!("stale-window-attacker-{index}"))
                    )
                    .is_ok()
            );
        }
        assert!(
            middleware
                .on_request(&ctx, &test_request(legitimate_id))
                .is_err(),
            "touching a limited legitimate partition must preserve its window"
        );

        let stale_key = method_scoped_test_key("stale-window-attacker-0");
        let legitimate_key = method_scoped_test_key(legitimate_id);
        let stale_last_seen = Instant::now()
            .checked_sub(idle_ttl + idle_ttl)
            .expect("the test idle interval must fit in Instant");
        {
            let mut partitions = middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(partitions.set_last_seen(stale_key, stale_last_seen));
            assert!(partitions.set_last_seen(legitimate_key, Instant::now()));
        }

        let active_state_probe = "window-active-state-overflow-probe";
        assert!(
            middleware
                .on_request(&ctx, &test_request(active_state_probe))
                .is_ok(),
            "stale but active window state must use the shared overflow limiter"
        );
        let active_state_probe_key = method_scoped_test_key(active_state_probe);
        {
            let partitions = middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(!partitions.contains_key(&active_state_probe_key));
        }
        {
            let partitions = middleware
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            partitions
                .entries
                .get(&stale_key)
                .expect("attacker partition must exist")
                .limiter
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }

        let newcomer_id = "new-legitimate-window-client";
        assert!(
            middleware
                .on_request(&ctx, &test_request(newcomer_id))
                .is_ok(),
            "an empty stale attacker partition should be reclaimed"
        );
        let newcomer_key = method_scoped_test_key(newcomer_id);
        let partitions = middleware
            .limiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!partitions.contains_key(&stale_key));
        assert!(partitions.contains_key(&legitimate_key));
        assert!(partitions.contains_key(&newcomer_key));
        assert_eq!(partitions.len(), MAX_NAMED_CLIENT_PARTITIONS);
        assert_eq!(partitions.recency.len(), MAX_NAMED_CLIENT_PARTITIONS);
        drop(partitions);

        assert!(
            middleware
                .on_request(&ctx, &test_request(legitimate_id))
                .is_err(),
            "the recent legitimate client's active window must not be reset"
        );
    }

    #[test]
    fn custom_identifier_canary_is_absent_from_errors_and_debug() {
        const CANARY: &str = "secret-client-canary-71d8f0";
        let token = RateLimitingMiddleware::new(1.0e-300)
            .burst_capacity(1)
            .client_id_extractor(|_ctx, _request| Some(CANARY.to_string()));
        let sliding = SlidingWindowRateLimitingMiddleware::new(1, 60)
            .client_id_extractor(|_ctx, _request| Some(CANARY.to_string()));
        let ctx = test_context();
        let request = test_request("tools/call");

        assert!(token.on_request(&ctx, &request).is_ok());
        let token_error = token.on_request(&ctx, &request).unwrap_err();
        assert!(!token_error.message.contains(CANARY));
        assert!(!format!("{token:?}").contains(CANARY));

        assert!(sliding.on_request(&ctx, &request).is_ok());
        let sliding_error = sliding.on_request(&ctx, &request).unwrap_err();
        assert!(!sliding_error.message.contains(CANARY));
        assert!(!format!("{sliding:?}").contains(CANARY));
    }

    #[test]
    fn oversized_custom_identifiers_fail_closed_without_partition_growth() {
        let oversized = "x".repeat(MAX_CLIENT_ID_BYTES + 1);
        let token_oversized = oversized.clone();
        let token = RateLimitingMiddleware::new(10.0)
            .client_id_extractor(move |_ctx, _request| Some(token_oversized.clone()));
        let sliding = SlidingWindowRateLimitingMiddleware::new(10, 60)
            .client_id_extractor(move |_ctx, _request| Some(oversized.clone()));
        let ctx = test_context();
        let request = test_request("tools/call");

        let token_error = token.on_request(&ctx, &request).unwrap_err();
        assert_eq!(token_error.message, RATE_LIMIT_EXCEEDED_MESSAGE);
        assert!(
            token
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        let sliding_error = sliding.on_request(&ctx, &request).unwrap_err();
        assert_eq!(sliding_error.message, RATE_LIMIT_EXCEEDED_MESSAGE);
        assert!(
            sliding
                .limiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn zero_and_overflowing_windows_fail_closed() {
        let zero_window = SlidingWindowRateLimiter::new(10, 0);
        assert!(!zero_window.is_allowed());
        assert_eq!(zero_window.current_requests(), 0);

        let zero_window_middleware = SlidingWindowRateLimitingMiddleware::new(10, 0);
        assert!(
            zero_window_middleware
                .on_request(&test_context(), &test_request("tools/call"))
                .is_err()
        );

        let overflowing_minutes = SlidingWindowRateLimitingMiddleware::per_minute(10, u64::MAX);
        assert_eq!(overflowing_minutes.window_seconds, 0);
        assert!(
            overflowing_minutes
                .on_request(&test_context(), &test_request("tools/call"))
                .is_err()
        );

        let maximum_seconds = SlidingWindowRateLimiter::new(1, u64::MAX);
        assert!(maximum_seconds.is_allowed());
        assert!(!maximum_seconds.is_allowed());
    }
}
