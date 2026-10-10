//! Docket: Distributed task queue for FastMCP.
//!
//! Provides distributed task queue capabilities with support for multiple backends:
//! - **Memory**: In-process queue for testing and development
//! - **Redis**: Distributed queue for production deployments
//!
//! # Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────┐
//! │                        Docket                              │
//! │  ┌─────────────────┐  ┌─────────────────┐                  │
//! │  │   DocketClient  │  │   Worker Pool   │                  │
//! │  │   (task submit) │  │   (processing)  │                  │
//! │  └────────┬────────┘  └────────┬────────┘                  │
//! │           │                    │                           │
//! │           └──────────┬─────────┘                           │
//! │                      ▼                                     │
//! │           ┌────────────────────┐                           │
//! │           │   DocketBackend    │                           │
//! │           └─────────┬──────────┘                           │
//! │                     │                                      │
//! │       ┌─────────────┼─────────────┐                        │
//! │       ▼             ▼             ▼                        │
//! │  ┌─────────┐  ┌─────────┐  ┌─────────────┐                 │
//! │  │ Memory  │  │  Redis  │  │   Future    │                 │
//! │  │ Backend │  │ Backend │  │  Backends   │                 │
//! │  └─────────┘  └─────────┘  └─────────────┘                 │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use fastmcp_server::docket::{Docket, DocketSettings, Worker};
//!
//! // Create with memory backend (testing/development)
//! let docket = Docket::memory();
//!
//! // Create with Redis backend (production)
//! let settings = DocketSettings::redis("redis://localhost:6379");
//! let docket = Docket::new(settings)?;
//!
//! // Submit a task
//! let task_id = docket.submit("process_data", json!({"input": "data"})).await?;
//!
//! // Create a worker
//! let worker = docket.worker()
//!     .subscribe("process_data", |task| async move {
//!         // Process the task
//!         Ok(json!({"result": "processed"}))
//!     })
//!     .build();
//!
//! // Start processing
//! worker.run().await?;
//! ```

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::McpError;
use fastmcp_core::logging::{debug, info, targets, warn};
use fastmcp_protocol::{TaskId, TaskInfo, TaskResult, TaskStatus};
use serde::{Deserialize, Serialize};

// ============================================================================
// Configuration
// ============================================================================

/// Settings for configuring a Docket instance.
#[derive(Debug, Clone)]
pub struct DocketSettings {
    /// Backend type (memory or redis).
    pub backend: DocketBackendType,
    /// Queue name prefix for namespacing.
    pub queue_prefix: String,
    /// Task visibility timeout (how long before unacked task is requeued).
    pub visibility_timeout: Duration,
    /// Default task timeout.
    pub default_task_timeout: Duration,
    /// Maximum retry count for failed tasks.
    pub max_retries: u32,
    /// Delay between retries (with exponential backoff).
    pub retry_delay: Duration,
    /// Worker poll interval when queue is empty.
    pub poll_interval: Duration,
}

impl Default for DocketSettings {
    fn default() -> Self {
        Self {
            backend: DocketBackendType::Memory,
            queue_prefix: "fastmcp:docket".to_string(),
            visibility_timeout: Duration::from_secs(30),
            default_task_timeout: Duration::from_secs(300),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
            poll_interval: Duration::from_millis(100),
        }
    }
}

impl DocketSettings {
    /// Creates settings for memory backend (testing/development).
    #[must_use]
    pub fn memory() -> Self {
        Self::default()
    }

    /// Creates settings for Redis backend.
    #[must_use]
    pub fn redis(url: impl Into<String>) -> Self {
        Self {
            backend: DocketBackendType::Redis(RedisSettings {
                url: url.into(),
                pool_size: 10,
                connect_timeout: Duration::from_secs(5),
            }),
            ..Self::default()
        }
    }

    /// Sets the queue prefix.
    #[must_use]
    pub fn with_queue_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.queue_prefix = prefix.into();
        self
    }

    /// Sets the visibility timeout.
    #[must_use]
    pub fn with_visibility_timeout(mut self, timeout: Duration) -> Self {
        self.visibility_timeout = timeout;
        self
    }

    /// Sets the maximum retry count.
    #[must_use]
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Sets the poll interval.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}

/// Backend type configuration.
#[derive(Debug, Clone)]
pub enum DocketBackendType {
    /// In-memory backend for testing/development.
    Memory,
    /// Redis backend for production.
    Redis(RedisSettings),
}

/// Redis connection settings.
#[derive(Debug, Clone)]
pub struct RedisSettings {
    /// Redis connection URL.
    pub url: String,
    /// Connection pool size.
    pub pool_size: usize,
    /// Connection timeout.
    pub connect_timeout: Duration,
}

// ============================================================================
// Task Types
// ============================================================================

/// A queued task in the Docket system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocketTask {
    /// Unique task identifier.
    pub id: TaskId,
    /// Task type (determines which handler processes it).
    pub task_type: String,
    /// Task parameters.
    pub params: serde_json::Value,
    /// Task priority (higher = processed first).
    pub priority: i32,
    /// Number of retry attempts so far.
    pub retry_count: u32,
    /// Maximum retries allowed.
    pub max_retries: u32,
    /// When the task was created.
    pub created_at: String,
    /// When the task was claimed by a worker.
    pub claimed_at: Option<String>,
    /// Current task status.
    pub status: TaskStatus,
    /// Error message if failed.
    pub error: Option<String>,
    /// Task result if completed.
    pub result: Option<serde_json::Value>,
}

impl DocketTask {
    /// Creates a new task.
    fn new(
        id: TaskId,
        task_type: String,
        params: serde_json::Value,
        priority: i32,
        max_retries: u32,
    ) -> Self {
        Self {
            id,
            task_type,
            params,
            priority,
            retry_count: 0,
            max_retries,
            created_at: chrono::Utc::now().to_rfc3339(),
            claimed_at: None,
            status: TaskStatus::Pending,
            error: None,
            result: None,
        }
    }

    /// Converts to TaskInfo for protocol responses.
    #[must_use]
    pub fn to_task_info(&self) -> TaskInfo {
        TaskInfo {
            id: self.id.clone(),
            task_type: self.task_type.clone(),
            status: self.status,
            progress: None,
            message: None,
            created_at: self.created_at.clone(),
            started_at: self.claimed_at.clone(),
            completed_at: if self.status.is_terminal() {
                Some(chrono::Utc::now().to_rfc3339())
            } else {
                None
            },
            error: self.error.clone(),
        }
    }

    /// Converts to TaskResult for protocol responses.
    #[must_use]
    pub fn to_task_result(&self) -> Option<TaskResult> {
        if !self.status.is_terminal() {
            return None;
        }
        Some(TaskResult {
            id: self.id.clone(),
            success: self.status == TaskStatus::Completed,
            data: self.result.clone(),
            error: self.error.clone(),
        })
    }
}

/// Options for submitting a task.
#[derive(Debug, Clone, Default)]
pub struct SubmitOptions {
    /// Task priority (higher = processed first).
    pub priority: i32,
    /// Maximum retries (overrides default).
    pub max_retries: Option<u32>,
    /// Delay before task becomes visible.
    pub delay: Option<Duration>,
}

impl SubmitOptions {
    /// Creates default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets task priority.
    #[must_use]
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Sets max retries.
    #[must_use]
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = Some(retries);
        self
    }

    /// Sets initial delay.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

// ============================================================================
// Backend Trait
// ============================================================================

/// Result type for Docket operations.
pub type DocketResult<T> = Result<T, DocketError>;

/// Errors that can occur in Docket operations.
#[derive(Debug)]
pub enum DocketError {
    /// Task not found.
    NotFound(String),
    /// Backend connection error.
    Connection(String),
    /// Serialization error.
    Serialization(String),
    /// Task handler error.
    Handler(String),
    /// Backend-specific error.
    Backend(String),
    /// Cancelled.
    Cancelled,
}

impl std::fmt::Display for DocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocketError::NotFound(msg) => write!(f, "Task not found: {msg}"),
            DocketError::Connection(msg) => write!(f, "Connection error: {msg}"),
            DocketError::Serialization(msg) => write!(f, "Serialization error: {msg}"),
            DocketError::Handler(msg) => write!(f, "Handler error: {msg}"),
            DocketError::Backend(msg) => write!(f, "Backend error: {msg}"),
            DocketError::Cancelled => write!(f, "Operation cancelled"),
        }
    }
}

impl std::error::Error for DocketError {}

impl From<DocketError> for McpError {
    fn from(err: DocketError) -> Self {
        McpError::internal_error(err.to_string())
    }
}

/// Backend trait for Docket storage.
///
/// Implementations provide the actual storage and retrieval of tasks.
/// The trait uses synchronous methods but backends can internally use
/// async operations wrapped in blocking.
pub trait DocketBackend: Send + Sync {
    /// Enqueues a task for processing.
    fn enqueue(&self, task: DocketTask) -> DocketResult<()>;

    /// Dequeues a task for the given task types.
    ///
    /// Returns the highest priority task that matches one of the subscribed types.
    /// The task is marked as claimed but not removed from the queue until acknowledged.
    fn dequeue(&self, task_types: &[String]) -> DocketResult<Option<DocketTask>>;

    /// Acknowledges successful task completion.
    fn ack(&self, task_id: &TaskId, result: serde_json::Value) -> DocketResult<()>;

    /// Negative acknowledgement - task failed, may be retried.
    fn nack(&self, task_id: &TaskId, error: &str) -> DocketResult<()>;

    /// Gets task by ID.
    fn get_task(&self, task_id: &TaskId) -> DocketResult<Option<DocketTask>>;

    /// Lists tasks, optionally filtered by status.
    fn list_tasks(&self, status: Option<TaskStatus>, limit: usize)
    -> DocketResult<Vec<DocketTask>>;

    /// Cancels a task.
    fn cancel(&self, task_id: &TaskId, reason: Option<&str>) -> DocketResult<()>;

    /// Returns queue statistics.
    fn stats(&self) -> DocketResult<QueueStats>;

    /// Requeues tasks that have exceeded visibility timeout.
    fn requeue_stale(&self) -> DocketResult<usize>;
}

/// Queue statistics.
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    /// Number of pending tasks.
    pub pending: usize,
    /// Number of in-progress tasks.
    pub in_progress: usize,
    /// Number of completed tasks.
    pub completed: usize,
    /// Number of failed tasks.
    pub failed: usize,
    /// Number of cancelled tasks.
    pub cancelled: usize,
}

// ============================================================================
// Memory Backend
// ============================================================================

/// In-memory Docket backend for testing and development.
///
/// Tasks are stored in memory and not persisted across restarts.
/// This backend is thread-safe and suitable for single-process deployments.
pub struct MemoryDocketBackend {
    /// All tasks indexed by ID.
    tasks: RwLock<HashMap<TaskId, DocketTask>>,
    /// Pending tasks queue (sorted by priority, then creation time).
    pending: RwLock<VecDeque<TaskId>>,
    /// Settings for visibility timeout, retries, etc.
    settings: DocketSettings,
}

impl MemoryDocketBackend {
    /// Creates a new memory backend.
    #[must_use]
    pub fn new(settings: DocketSettings) -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            pending: RwLock::new(VecDeque::new()),
            settings,
        }
    }
}

impl DocketBackend for MemoryDocketBackend {
    fn enqueue(&self, task: DocketTask) -> DocketResult<()> {
        let task_id = task.id.clone();
        let priority = task.priority;

        {
            let mut tasks = self
                .tasks
                .write()
                .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;
            tasks.insert(task_id.clone(), task);
        }

        {
            let mut pending = self
                .pending
                .write()
                .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

            // Insert maintaining priority order (higher priority first)
            let tasks = self
                .tasks
                .read()
                .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

            let pos = pending
                .iter()
                .position(|id| tasks.get(id).is_none_or(|t| t.priority < priority))
                .unwrap_or(pending.len());

            pending.insert(pos, task_id);
        }

        Ok(())
    }

    fn dequeue(&self, task_types: &[String]) -> DocketResult<Option<DocketTask>> {
        let mut pending = self
            .pending
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

        // Find first pending task matching subscribed types
        let pos = pending.iter().position(|id| {
            tasks.get(id).is_some_and(|t| {
                t.status == TaskStatus::Pending && task_types.contains(&t.task_type)
            })
        });

        if let Some(pos) = pos {
            let task_id = pending.remove(pos).expect("position valid");
            if let Some(task) = tasks.get_mut(&task_id) {
                task.status = TaskStatus::Running;
                task.claimed_at = Some(chrono::Utc::now().to_rfc3339());
                return Ok(Some(task.clone()));
            }
        }

        Ok(None)
    }

    fn ack(&self, task_id: &TaskId, result: serde_json::Value) -> DocketResult<()> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| DocketError::NotFound(task_id.to_string()))?;

        task.status = TaskStatus::Completed;
        task.result = Some(result);

        Ok(())
    }

    fn nack(&self, task_id: &TaskId, error: &str) -> DocketResult<()> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;
        let mut pending = self
            .pending
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| DocketError::NotFound(task_id.to_string()))?;

        task.retry_count += 1;
        task.error = Some(error.to_string());

        if task.retry_count >= task.max_retries {
            // Max retries exceeded - mark as failed
            task.status = TaskStatus::Failed;
        } else {
            // Requeue for retry
            task.status = TaskStatus::Pending;
            task.claimed_at = None;
            pending.push_back(task_id.clone());
        }

        Ok(())
    }

    fn get_task(&self, task_id: &TaskId) -> DocketResult<Option<DocketTask>> {
        let tasks = self
            .tasks
            .read()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;
        Ok(tasks.get(task_id).cloned())
    }

    fn list_tasks(
        &self,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> DocketResult<Vec<DocketTask>> {
        let tasks = self
            .tasks
            .read()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

        let iter = tasks
            .values()
            .filter(|t| status.is_none_or(|s| t.status == s));

        Ok(iter.take(limit).cloned().collect())
    }

    fn cancel(&self, task_id: &TaskId, reason: Option<&str>) -> DocketResult<()> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;
        let mut pending = self
            .pending
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| DocketError::NotFound(task_id.to_string()))?;

        if task.status.is_terminal() {
            return Err(DocketError::Backend(format!(
                "Cannot cancel task in terminal state: {:?}",
                task.status
            )));
        }

        task.status = TaskStatus::Cancelled;
        task.error = reason.map(String::from);

        // Remove from pending queue
        pending.retain(|id| id != task_id);

        Ok(())
    }

    fn stats(&self) -> DocketResult<QueueStats> {
        let tasks = self
            .tasks
            .read()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

        let mut stats = QueueStats::default();
        for task in tasks.values() {
            match task.status {
                TaskStatus::Pending => stats.pending += 1,
                TaskStatus::Running => stats.in_progress += 1,
                TaskStatus::Completed => stats.completed += 1,
                TaskStatus::Failed => stats.failed += 1,
                TaskStatus::Cancelled => stats.cancelled += 1,
            }
        }

        Ok(stats)
    }

    fn requeue_stale(&self) -> DocketResult<usize> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;
        let mut pending = self
            .pending
            .write()
            .map_err(|e| DocketError::Backend(format!("Lock poisoned: {e}")))?;

        let now = chrono::Utc::now();
        let timeout = chrono::Duration::from_std(self.settings.visibility_timeout)
            .unwrap_or_else(|_| chrono::Duration::seconds(30));

        let mut requeued = 0;

        for task in tasks.values_mut() {
            if task.status != TaskStatus::Running {
                continue;
            }

            if let Some(ref claimed_at) = task.claimed_at {
                if let Ok(claimed) = chrono::DateTime::parse_from_rfc3339(claimed_at) {
                    if now - claimed.with_timezone(&chrono::Utc) > timeout {
                        // Task has exceeded visibility timeout - requeue
                        task.status = TaskStatus::Pending;
                        task.claimed_at = None;
                        task.retry_count += 1;

                        if task.retry_count >= task.max_retries {
                            task.status = TaskStatus::Failed;
                            task.error = Some("Exceeded visibility timeout".to_string());
                        } else {
                            pending.push_back(task.id.clone());
                            requeued += 1;
                        }
                    }
                }
            }
        }

        Ok(requeued)
    }
}

// ============================================================================
// Redis Backend
// ============================================================================

/// Redis Docket backend for production distributed deployments.
///
/// Uses Redis lists and sorted sets for reliable task queuing with
/// visibility timeout and atomic operations.
#[cfg(feature = "redis")]
pub struct RedisDocketBackend {
    client: redis::Client,
    pool: Vec<std::sync::Mutex<redis::Connection>>,
    next_conn: std::sync::atomic::AtomicUsize,
    settings: RedisSettings,
    docket_settings: DocketSettings,
}

#[cfg(feature = "redis")]
impl RedisDocketBackend {
    /// Creates a new Redis backend.
    pub fn new(
        redis_settings: RedisSettings,
        docket_settings: DocketSettings,
    ) -> DocketResult<Self> {
        let client = redis::Client::open(redis_settings.url.as_str())
            .map_err(|e| DocketError::Backend(format!("Redis client init failed: {e}")))?;

        let mut pool = Vec::new();
        let pool_size = redis_settings.pool_size.max(1);
        for _ in 0..pool_size {
            let conn = client
                .get_connection()
                .map_err(|e| DocketError::Backend(format!("Redis connect failed: {e}")))?;
            pool.push(std::sync::Mutex::new(conn));
        }

        Ok(Self {
            client,
            pool,
            next_conn: std::sync::atomic::AtomicUsize::new(0),
            settings: redis_settings,
            docket_settings,
        })
    }

    fn key_tasks(&self) -> String {
        format!("{}:tasks", self.docket_settings.queue_prefix)
    }

    fn key_running(&self) -> String {
        format!("{}:running", self.docket_settings.queue_prefix)
    }

    fn key_types(&self) -> String {
        format!("{}:types", self.docket_settings.queue_prefix)
    }

    fn key_queue_member(&self) -> String {
        // task_id -> queue member encoding (used for pending/delayed removal)
        format!("{}:queue_member", self.docket_settings.queue_prefix)
    }

    fn key_queue_type(&self) -> String {
        // task_id -> task_type (used for pending/delayed key selection)
        format!("{}:queue_type", self.docket_settings.queue_prefix)
    }

    fn key_pending(&self, task_type: &str) -> String {
        format!("{}:pending:{task_type}", self.docket_settings.queue_prefix)
    }

    fn key_delayed(&self, task_type: &str) -> String {
        format!("{}:delayed:{task_type}", self.docket_settings.queue_prefix)
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn encode_member(task: &DocketTask) -> String {
        let created_ms = chrono::DateTime::parse_from_rfc3339(&task.created_at)
            .map(|dt| dt.timestamp_millis())
            .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());
        let prio_key: i64 = (i32::MAX as i64) - (task.priority as i64);
        format!("{prio_key:010}:{created_ms:013}:{}", task.id.0)
    }

    fn retry_delay_ms(&self, retry_count: u32) -> i64 {
        let base = self
            .docket_settings
            .retry_delay
            .as_millis()
            .min(i64::MAX as u128) as i64;
        if base <= 0 {
            return 0;
        }
        let exp = retry_count.saturating_sub(1).min(30);
        let factor: i64 = 1i64.checked_shl(exp).unwrap_or(i64::MAX);
        base.saturating_mul(factor)
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&mut redis::Connection) -> redis::RedisResult<T>,
    ) -> DocketResult<T> {
        let idx = self
            .next_conn
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.pool.len();
        let mut guard = self.pool[idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard).map_err(|e| DocketError::Backend(format!("Redis error: {e}")))
    }
}

#[cfg(feature = "redis")]
impl DocketBackend for RedisDocketBackend {
    fn enqueue(&self, task: DocketTask) -> DocketResult<()> {
        let task_id = task.id.0.clone();
        let task_type = task.task_type.clone();
        let member = Self::encode_member(&task);
        let json = serde_json::to_string(&task)
            .map_err(|e| DocketError::Backend(format!("Task serialize failed: {e}")))?;

        let tasks_key = self.key_tasks();
        let types_key = self.key_types();
        let member_key = self.key_queue_member();
        let type_key = self.key_queue_type();
        let pending_key = self.key_pending(&task_type);

        self.with_conn(|conn| {
            redis::pipe()
                .atomic()
                .cmd("HSET")
                .arg(&tasks_key)
                .arg(&task_id)
                .arg(&json)
                .ignore()
                .cmd("SADD")
                .arg(&types_key)
                .arg(&task_type)
                .ignore()
                .cmd("HSET")
                .arg(&member_key)
                .arg(&task_id)
                .arg(&member)
                .ignore()
                .cmd("HSET")
                .arg(&type_key)
                .arg(&task_id)
                .arg(&task_type)
                .ignore()
                .cmd("ZADD")
                .arg(&pending_key)
                .arg(0)
                .arg(&member)
                .ignore()
                .query::<()>(conn)
        })?;

        Ok(())
    }

    fn dequeue(&self, task_types: &[String]) -> DocketResult<Option<DocketTask>> {
        if task_types.is_empty() {
            return Ok(None);
        }

        // Move any delayed tasks that are now due into the pending queues.
        let _ = self.requeue_stale();

        let mut pending_keys: Vec<String> =
            task_types.iter().map(|t| self.key_pending(t)).collect();
        let tasks_key = self.key_tasks();
        let running_key = self.key_running();
        let member_key = self.key_queue_member();

        // KEYS: pending..., tasks_key, running_key, member_key
        pending_keys.push(tasks_key.clone());
        pending_keys.push(running_key.clone());
        pending_keys.push(member_key.clone());

        let now_rfc = Self::now_rfc3339();
        let now_ms = Self::now_ms();

        // Atomically pick the best pending task across the subscribed queues.
        const LUA: &str = r#"
local tasks_key = KEYS[#KEYS-2]
local running_key = KEYS[#KEYS-1]
local member_key = KEYS[#KEYS]

local now_rfc = ARGV[1]
local now_ms = tonumber(ARGV[2])

local best = nil
local best_key = nil
for i=1,(#KEYS-3) do
  local k = KEYS[i]
  local r = redis.call('ZRANGE', k, 0, 0)
  if r and r[1] then
    local m = r[1]
    if (not best) or (m < best) then
      best = m
      best_key = k
    end
  end
end

if not best then
  return nil
end

redis.call('ZREM', best_key, best)

-- Extract task_id from member "{prio}:{created}:{task_id}"
local _, _, task_id = string.find(best, "^[^:]+:[^:]+:(.+)$")
if not task_id then
  return nil
end

redis.call('HDEL', member_key, task_id)
redis.call('ZADD', running_key, now_ms, task_id)

local tjson = redis.call('HGET', tasks_key, task_id)
if not tjson then
  return nil
end

local t = cjson.decode(tjson)
t["status"] = "running"
t["claimed_at"] = now_rfc
local out = cjson.encode(t)
redis.call('HSET', tasks_key, task_id, out)
return out
"#;

        let task_json: Option<String> = self.with_conn(|conn| {
            let script = redis::Script::new(LUA);
            let mut inv = script.prepare_invoke();
            for k in &pending_keys {
                inv.key(k);
            }
            inv.arg(now_rfc).arg(now_ms).invoke(conn)
        })?;

        let Some(task_json) = task_json else {
            return Ok(None);
        };

        let task: DocketTask = serde_json::from_str(&task_json)
            .map_err(|e| DocketError::Backend(format!("Task deserialize failed: {e}")))?;
        Ok(Some(task))
    }

    fn ack(&self, task_id: &TaskId, result: serde_json::Value) -> DocketResult<()> {
        let task_id_str = task_id.0.clone();
        let tasks_key = self.key_tasks();
        let running_key = self.key_running();
        let member_key = self.key_queue_member();
        let type_key = self.key_queue_type();

        let mut task = self
            .get_task(task_id)?
            .ok_or_else(|| DocketError::NotFound(task_id_str.clone()))?;
        task.status = TaskStatus::Completed;
        task.result = Some(result);

        let json = serde_json::to_string(&task)
            .map_err(|e| DocketError::Backend(format!("Task serialize failed: {e}")))?;

        self.with_conn(|conn| {
            redis::pipe()
                .atomic()
                .cmd("HSET")
                .arg(&tasks_key)
                .arg(&task_id_str)
                .arg(&json)
                .ignore()
                .cmd("ZREM")
                .arg(&running_key)
                .arg(&task_id_str)
                .ignore()
                .cmd("HDEL")
                .arg(&member_key)
                .arg(&task_id_str)
                .ignore()
                .cmd("HDEL")
                .arg(&type_key)
                .arg(&task_id_str)
                .ignore()
                .query::<()>(conn)
        })?;

        Ok(())
    }

    fn nack(&self, task_id: &TaskId, error: &str) -> DocketResult<()> {
        let task_id_str = task_id.0.clone();
        let tasks_key = self.key_tasks();
        let running_key = self.key_running();
        let member_key = self.key_queue_member();
        let type_key = self.key_queue_type();

        let mut task = self
            .get_task(task_id)?
            .ok_or_else(|| DocketError::NotFound(task_id_str.clone()))?;

        task.retry_count += 1;
        task.error = Some(error.to_string());

        // Remove from running
        self.with_conn(|conn| {
            redis::cmd("ZREM")
                .arg(&running_key)
                .arg(&task_id_str)
                .query::<()>(conn)
        })?;

        if task.retry_count >= task.max_retries {
            task.status = TaskStatus::Failed;
            let json = serde_json::to_string(&task)
                .map_err(|e| DocketError::Backend(format!("Task serialize failed: {e}")))?;
            self.with_conn(|conn| {
                redis::pipe()
                    .atomic()
                    .cmd("HSET")
                    .arg(&tasks_key)
                    .arg(&task_id_str)
                    .arg(&json)
                    .ignore()
                    .cmd("HDEL")
                    .arg(&member_key)
                    .arg(&task_id_str)
                    .ignore()
                    .cmd("HDEL")
                    .arg(&type_key)
                    .arg(&task_id_str)
                    .ignore()
                    .query::<()>(conn)
            })?;
            return Ok(());
        }

        // Schedule retry with exponential backoff.
        task.status = TaskStatus::Pending;
        task.claimed_at = None;

        let member = Self::encode_member(&task);
        let task_type = task.task_type.clone();
        let delayed_key = self.key_delayed(&task_type);
        let available_ms = Self::now_ms().saturating_add(self.retry_delay_ms(task.retry_count));

        let json = serde_json::to_string(&task)
            .map_err(|e| DocketError::Backend(format!("Task serialize failed: {e}")))?;

        self.with_conn(|conn| {
            redis::pipe()
                .atomic()
                .cmd("HSET")
                .arg(&tasks_key)
                .arg(&task_id_str)
                .arg(&json)
                .ignore()
                .cmd("HSET")
                .arg(&member_key)
                .arg(&task_id_str)
                .arg(&member)
                .ignore()
                .cmd("HSET")
                .arg(&type_key)
                .arg(&task_id_str)
                .arg(&task_type)
                .ignore()
                .cmd("ZADD")
                .arg(&delayed_key)
                .arg(available_ms)
                .arg(&member)
                .ignore()
                .query::<()>(conn)
        })?;

        Ok(())
    }

    fn get_task(&self, task_id: &TaskId) -> DocketResult<Option<DocketTask>> {
        let tasks_key = self.key_tasks();
        let task_id_str = task_id.0.clone();
        let json: Option<String> = self.with_conn(|conn| {
            redis::cmd("HGET")
                .arg(&tasks_key)
                .arg(&task_id_str)
                .query(conn)
        })?;
        let Some(json) = json else {
            return Ok(None);
        };
        let task: DocketTask = serde_json::from_str(&json)
            .map_err(|e| DocketError::Backend(format!("Task deserialize failed: {e}")))?;
        Ok(Some(task))
    }

    fn list_tasks(
        &self,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> DocketResult<Vec<DocketTask>> {
        let tasks_key = self.key_tasks();
        let values: Vec<String> =
            self.with_conn(|conn| redis::cmd("HVALS").arg(&tasks_key).query(conn))?;

        let mut tasks = Vec::new();
        for json in values {
            if let Ok(task) = serde_json::from_str::<DocketTask>(&json) {
                if status.is_none_or(|s| task.status == s) {
                    tasks.push(task);
                }
            }
        }

        tasks.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        tasks.truncate(limit);
        Ok(tasks)
    }

    fn cancel(&self, task_id: &TaskId, reason: Option<&str>) -> DocketResult<()> {
        let task_id_str = task_id.0.clone();
        let tasks_key = self.key_tasks();
        let running_key = self.key_running();
        let member_key = self.key_queue_member();
        let type_key = self.key_queue_type();

        let Some(mut task) = self.get_task(task_id)? else {
            return Err(DocketError::NotFound(task_id_str));
        };

        task.status = TaskStatus::Cancelled;
        task.error = Some(reason.unwrap_or("Cancelled").to_string());
        task.result = Some(serde_json::json!({"cancelled": true}));

        let json = serde_json::to_string(&task)
            .map_err(|e| DocketError::Backend(format!("Task serialize failed: {e}")))?;

        let member: Option<String> = self.with_conn(|conn| {
            redis::cmd("HGET")
                .arg(&member_key)
                .arg(&task_id_str)
                .query(conn)
        })?;
        let task_type: Option<String> = self.with_conn(|conn| {
            redis::cmd("HGET")
                .arg(&type_key)
                .arg(&task_id_str)
                .query(conn)
        })?;

        self.with_conn(|conn| {
            redis::pipe()
                .atomic()
                .cmd("HSET")
                .arg(&tasks_key)
                .arg(&task_id_str)
                .arg(&json)
                .ignore()
                .cmd("ZREM")
                .arg(&running_key)
                .arg(&task_id_str)
                .ignore()
                .cmd("HDEL")
                .arg(&member_key)
                .arg(&task_id_str)
                .ignore()
                .cmd("HDEL")
                .arg(&type_key)
                .arg(&task_id_str)
                .ignore()
                .query::<()>(conn)
        })?;

        if let (Some(member), Some(task_type)) = (member, task_type) {
            let pending_key = self.key_pending(&task_type);
            let delayed_key = self.key_delayed(&task_type);
            let _ = self.with_conn(|conn| {
                redis::pipe()
                    .atomic()
                    .cmd("ZREM")
                    .arg(&pending_key)
                    .arg(&member)
                    .ignore()
                    .cmd("ZREM")
                    .arg(&delayed_key)
                    .arg(&member)
                    .ignore()
                    .query::<()>(conn)
            });
        }

        Ok(())
    }

    fn stats(&self) -> DocketResult<QueueStats> {
        let tasks = self.list_tasks(None, usize::MAX)?;
        let mut stats = QueueStats::default();
        for t in tasks {
            match t.status {
                TaskStatus::Pending => stats.pending += 1,
                TaskStatus::Running => stats.in_progress += 1,
                TaskStatus::Completed => stats.completed += 1,
                TaskStatus::Failed => stats.failed += 1,
                TaskStatus::Cancelled => stats.cancelled += 1,
            }
        }
        Ok(stats)
    }

    fn requeue_stale(&self) -> DocketResult<usize> {
        let now_ms = Self::now_ms();
        let visibility_ms: i64 = self
            .docket_settings
            .visibility_timeout
            .as_millis()
            .min(i64::MAX as u128) as i64;
        let cutoff = now_ms.saturating_sub(visibility_ms);

        let tasks_key = self.key_tasks();
        let running_key = self.key_running();
        let types_key = self.key_types();
        let member_key = self.key_queue_member();
        let type_key = self.key_queue_type();

        // 1) Promote due delayed tasks into pending queues.
        let types: Vec<String> =
            self.with_conn(|conn| redis::cmd("SMEMBERS").arg(&types_key).query(conn))?;
        for t in &types {
            let delayed_key = self.key_delayed(t);
            let pending_key = self.key_pending(t);
            let due_members: Vec<String> = self.with_conn(|conn| {
                redis::cmd("ZRANGEBYSCORE")
                    .arg(&delayed_key)
                    .arg("-inf")
                    .arg(now_ms)
                    .query(conn)
            })?;
            if due_members.is_empty() {
                continue;
            }
            self.with_conn(|conn| {
                let mut pipe = redis::pipe();
                pipe.atomic();
                for m in &due_members {
                    pipe.cmd("ZREM").arg(&delayed_key).arg(m).ignore();
                    pipe.cmd("ZADD").arg(&pending_key).arg(0).arg(m).ignore();
                }
                pipe.query::<()>(conn)
            })?;
        }

        // 2) Requeue running tasks past visibility timeout.
        let stale: Vec<String> = self.with_conn(|conn| {
            redis::cmd("ZRANGEBYSCORE")
                .arg(&running_key)
                .arg("-inf")
                .arg(cutoff)
                .query(conn)
        })?;

        let mut requeued = 0usize;
        for task_id in stale {
            // Claim the stale record: if another worker already handled it, skip.
            let removed: i64 = self.with_conn(|conn| {
                redis::cmd("ZREM")
                    .arg(&running_key)
                    .arg(&task_id)
                    .query(conn)
            })?;
            if removed == 0 {
                continue;
            }

            let id = TaskId::from_string(task_id.clone());
            let Some(mut task) = self.get_task(&id)? else {
                continue;
            };

            task.retry_count += 1;
            task.claimed_at = None;
            task.error = Some("Exceeded visibility timeout".to_string());

            if task.retry_count >= task.max_retries {
                task.status = TaskStatus::Failed;
                let json = serde_json::to_string(&task)
                    .map_err(|e| DocketError::Backend(format!("Task serialize failed: {e}")))?;
                self.with_conn(|conn| {
                    redis::pipe()
                        .atomic()
                        .cmd("HSET")
                        .arg(&tasks_key)
                        .arg(&task_id)
                        .arg(&json)
                        .ignore()
                        .cmd("HDEL")
                        .arg(&member_key)
                        .arg(&task_id)
                        .ignore()
                        .cmd("HDEL")
                        .arg(&type_key)
                        .arg(&task_id)
                        .ignore()
                        .query::<()>(conn)
                })?;
                continue;
            }

            task.status = TaskStatus::Pending;
            let member = Self::encode_member(&task);
            let task_type = task.task_type.clone();
            let delayed_key = self.key_delayed(&task_type);
            let available_ms = now_ms.saturating_add(self.retry_delay_ms(task.retry_count));

            let json = serde_json::to_string(&task)
                .map_err(|e| DocketError::Backend(format!("Task serialize failed: {e}")))?;
            self.with_conn(|conn| {
                redis::pipe()
                    .atomic()
                    .cmd("HSET")
                    .arg(&tasks_key)
                    .arg(&task_id)
                    .arg(&json)
                    .ignore()
                    .cmd("HSET")
                    .arg(&member_key)
                    .arg(&task_id)
                    .arg(&member)
                    .ignore()
                    .cmd("HSET")
                    .arg(&type_key)
                    .arg(&task_id)
                    .arg(&task_type)
                    .ignore()
                    .cmd("ZADD")
                    .arg(&delayed_key)
                    .arg(available_ms)
                    .arg(&member)
                    .ignore()
                    .query::<()>(conn)
            })?;
            requeued += 1;
        }

        Ok(requeued)
    }
}

// ============================================================================
// Docket Client
// ============================================================================

/// Docket distributed task queue.
///
/// The main entry point for submitting and managing distributed tasks.
pub struct Docket {
    backend: Arc<dyn DocketBackend>,
    settings: DocketSettings,
    task_counter: AtomicU64,
}

impl Docket {
    /// Creates a new Docket with the given settings.
    pub fn new(settings: DocketSettings) -> DocketResult<Self> {
        let backend: Arc<dyn DocketBackend> = match &settings.backend {
            DocketBackendType::Memory => Arc::new(MemoryDocketBackend::new(settings.clone())),
            #[cfg(feature = "redis")]
            DocketBackendType::Redis(redis_settings) => Arc::new(RedisDocketBackend::new(
                redis_settings.clone(),
                settings.clone(),
            )?),
            #[cfg(not(feature = "redis"))]
            DocketBackendType::Redis(_) => {
                return Err(DocketError::Backend(
                    "Redis backend requires 'redis' feature".to_string(),
                ));
            }
        };

        Ok(Self {
            backend,
            settings,
            task_counter: AtomicU64::new(0),
        })
    }

    /// Creates a Docket with memory backend (for testing).
    #[must_use]
    pub fn memory() -> Self {
        Self::new(DocketSettings::memory()).expect("memory backend always succeeds")
    }

    /// Submits a task to the queue.
    pub fn submit(
        &self,
        task_type: impl Into<String>,
        params: serde_json::Value,
    ) -> DocketResult<TaskId> {
        self.submit_with_options(task_type, params, SubmitOptions::default())
    }

    /// Submits a task with custom options.
    pub fn submit_with_options(
        &self,
        task_type: impl Into<String>,
        params: serde_json::Value,
        options: SubmitOptions,
    ) -> DocketResult<TaskId> {
        let counter = self.task_counter.fetch_add(1, Ordering::SeqCst);
        let task_id = TaskId::from_string(format!("docket-{counter:08x}"));

        let max_retries = options.max_retries.unwrap_or(self.settings.max_retries);
        let task = DocketTask::new(
            task_id.clone(),
            task_type.into(),
            params,
            options.priority,
            max_retries,
        );

        self.backend.enqueue(task)?;

        info!(
            target: targets::SERVER,
            "Docket: submitted task {} (type: {})",
            task_id,
            task_id
        );

        Ok(task_id)
    }

    /// Gets a task by ID.
    pub fn get_task(&self, task_id: &TaskId) -> DocketResult<Option<DocketTask>> {
        self.backend.get_task(task_id)
    }

    /// Lists tasks with optional status filter.
    pub fn list_tasks(
        &self,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> DocketResult<Vec<DocketTask>> {
        self.backend.list_tasks(status, limit)
    }

    /// Cancels a task.
    pub fn cancel(&self, task_id: &TaskId, reason: Option<&str>) -> DocketResult<()> {
        self.backend.cancel(task_id, reason)
    }

    /// Returns queue statistics.
    pub fn stats(&self) -> DocketResult<QueueStats> {
        self.backend.stats()
    }

    /// Creates a worker builder.
    #[must_use]
    pub fn worker(&self) -> WorkerBuilder {
        WorkerBuilder::new(Arc::clone(&self.backend), self.settings.clone())
    }

    /// Returns the settings.
    #[must_use]
    pub fn settings(&self) -> &DocketSettings {
        &self.settings
    }

    /// Converts to a shared handle.
    #[must_use]
    pub fn into_shared(self) -> SharedDocket {
        Arc::new(self)
    }
}

impl std::fmt::Debug for Docket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Docket")
            .field("settings", &self.settings)
            .field("task_counter", &self.task_counter.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

/// Thread-safe handle to a Docket.
pub type SharedDocket = Arc<Docket>;

// ============================================================================
// Worker
// ============================================================================

/// Task handler function type.
pub type TaskHandlerFn = Box<
    dyn Fn(DocketTask) -> Pin<Box<dyn Future<Output = DocketResult<serde_json::Value>> + Send>>
        + Send
        + Sync,
>;

/// Builder for creating workers.
pub struct WorkerBuilder {
    backend: Arc<dyn DocketBackend>,
    settings: DocketSettings,
    handlers: HashMap<String, TaskHandlerFn>,
}

impl WorkerBuilder {
    fn new(backend: Arc<dyn DocketBackend>, settings: DocketSettings) -> Self {
        Self {
            backend,
            settings,
            handlers: HashMap::new(),
        }
    }

    /// Subscribes to a task type with a handler.
    pub fn subscribe<F, Fut>(mut self, task_type: impl Into<String>, handler: F) -> Self
    where
        F: Fn(DocketTask) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DocketResult<serde_json::Value>> + Send + 'static,
    {
        let task_type = task_type.into();
        let boxed: TaskHandlerFn = Box::new(move |task| Box::pin(handler(task)));
        self.handlers.insert(task_type, boxed);
        self
    }

    /// Builds the worker.
    #[must_use]
    pub fn build(self) -> Worker {
        Worker {
            backend: self.backend,
            settings: self.settings,
            handlers: Arc::new(self.handlers),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// A worker that processes tasks from the queue.
pub struct Worker {
    backend: Arc<dyn DocketBackend>,
    settings: DocketSettings,
    handlers: Arc<HashMap<String, TaskHandlerFn>>,
    running: Arc<AtomicBool>,
}

impl Worker {
    /// Returns the task types this worker is subscribed to.
    #[must_use]
    pub fn subscribed_types(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }

    /// Returns whether the worker is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Stops the worker.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Processes a single task if available.
    ///
    /// Returns true if a task was processed, false if no task was available.
    pub async fn process_one(&self, cx: &Cx) -> DocketResult<bool> {
        let task_types = self.subscribed_types();

        // Check for cancellation
        if cx.is_cancel_requested() {
            return Err(DocketError::Cancelled);
        }

        // Try to dequeue a task
        let Some(task) = self.backend.dequeue(&task_types)? else {
            return Ok(false);
        };

        let task_id = task.id.clone();
        let task_type = task.task_type.clone();

        debug!(
            target: targets::SERVER,
            "Docket worker: processing task {} (type: {})",
            task_id,
            task_type
        );

        // Get handler
        let Some(handler) = self.handlers.get(&task_type) else {
            // This shouldn't happen since we only dequeue subscribed types
            self.backend.nack(&task_id, "No handler for task type")?;
            return Ok(true);
        };

        // Execute handler
        let result = handler(task).await;

        match result {
            Ok(data) => {
                self.backend.ack(&task_id, data)?;
                info!(
                    target: targets::SERVER,
                    "Docket worker: completed task {}",
                    task_id
                );
            }
            Err(e) => {
                let error_msg = e.to_string();
                self.backend.nack(&task_id, &error_msg)?;
                warn!(
                    target: targets::SERVER,
                    "Docket worker: task {} failed: {}",
                    task_id,
                    error_msg
                );
            }
        }

        Ok(true)
    }

    /// Runs the worker loop until stopped.
    pub async fn run(&self, cx: &Cx) -> DocketResult<()> {
        self.running.store(true, Ordering::SeqCst);

        info!(
            target: targets::SERVER,
            "Docket worker starting with subscriptions: {:?}",
            self.subscribed_types()
        );

        while self.running.load(Ordering::SeqCst) {
            // Check for cancellation
            if cx.is_cancel_requested() {
                break;
            }

            // Requeue stale tasks periodically
            let _ = self.backend.requeue_stale();

            // Process tasks
            match self.process_one(cx).await {
                Ok(true) => {
                    // Processed a task, immediately try for another
                    continue;
                }
                Ok(false) => {
                    // No task available, wait before polling again
                    std::thread::sleep(self.settings.poll_interval);
                }
                Err(DocketError::Cancelled) => {
                    break;
                }
                Err(e) => {
                    warn!(
                        target: targets::SERVER,
                        "Docket worker error: {}",
                        e
                    );
                    // Brief pause on error before retrying
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        self.running.store(false, Ordering::SeqCst);
        info!(target: targets::SERVER, "Docket worker stopped");

        Ok(())
    }
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("subscribed_types", &self.subscribed_types())
            .field("running", &self.is_running())
            .finish_non_exhaustive()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "redis")]
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    #[cfg(feature = "redis")]
    use std::net::TcpListener;
    #[cfg(feature = "redis")]
    use std::process::{Child, Command, Stdio};
    #[cfg(feature = "redis")]
    use std::time::Instant;

    #[cfg(feature = "redis")]
    static REDIS_TEST_SEQ: AtomicU64 = AtomicU64::new(1);

    #[cfg(feature = "redis")]
    fn next_test_token(label: &str) -> String {
        let seq = REDIS_TEST_SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        format!("{label}-{}-{seq}", std::process::id())
    }

    #[cfg(feature = "redis")]
    struct TestRedisServer {
        child: Child,
        url: String,
    }

    #[cfg(feature = "redis")]
    impl TestRedisServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral redis port");
            let port = listener.local_addr().expect("redis test local addr").port();
            drop(listener);

            let child = Command::new("redis-server")
                .arg("--port")
                .arg(port.to_string())
                .arg("--save")
                .arg("")
                .arg("--appendonly")
                .arg("no")
                .arg("--bind")
                .arg("127.0.0.1")
                .arg("--protected-mode")
                .arg("no")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn redis-server");

            let url = format!("redis://127.0.0.1:{port}/");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let ready = redis::Client::open(url.as_str())
                    .ok()
                    .and_then(|client| client.get_connection().ok())
                    .and_then(|mut conn| redis::cmd("PING").query::<String>(&mut conn).ok())
                    .is_some_and(|pong| pong == "PONG");

                if ready {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "redis-server did not become ready"
                );
                std::thread::sleep(Duration::from_millis(20));
            }

            Self { child, url }
        }
    }

    #[cfg(feature = "redis")]
    impl Drop for TestRedisServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(feature = "redis")]
    fn redis_settings_for_test(url: &str) -> DocketSettings {
        let mut settings = DocketSettings::redis(url);
        settings.queue_prefix = format!("fastmcp:docket:test:{}", next_test_token("queue"));
        settings.poll_interval = Duration::from_millis(1);
        settings.retry_delay = Duration::from_millis(0);
        settings
    }

    #[test]
    fn test_docket_settings_default() {
        let settings = DocketSettings::default();
        assert!(matches!(settings.backend, DocketBackendType::Memory));
        assert_eq!(settings.max_retries, 3);
    }

    #[test]
    fn test_docket_settings_redis() {
        let settings = DocketSettings::redis("redis://localhost:6379");
        assert!(matches!(settings.backend, DocketBackendType::Redis(_)));
    }

    #[test]
    fn test_docket_settings_builder() {
        let settings = DocketSettings::memory()
            .with_queue_prefix("test:queue")
            .with_max_retries(5)
            .with_poll_interval(Duration::from_millis(50));

        assert_eq!(settings.queue_prefix, "test:queue");
        assert_eq!(settings.max_retries, 5);
        assert_eq!(settings.poll_interval, Duration::from_millis(50));
    }

    #[test]
    fn test_docket_memory_creation() {
        let docket = Docket::memory();
        assert!(matches!(docket.settings.backend, DocketBackendType::Memory));
    }

    #[test]
    fn test_docket_submit_task() {
        let docket = Docket::memory();

        let task_id = docket
            .submit("test_task", serde_json::json!({"key": "value"}))
            .unwrap();

        assert!(task_id.to_string().starts_with("docket-"));

        // Verify task exists
        let task = docket.get_task(&task_id).unwrap().unwrap();
        assert_eq!(task.task_type, "test_task");
        assert_eq!(task.status, TaskStatus::Pending);
    }

    #[test]
    fn test_docket_submit_with_priority() {
        let docket = Docket::memory();

        let low_id = docket
            .submit_with_options(
                "task",
                serde_json::json!({"priority": "low"}),
                SubmitOptions::new().with_priority(1),
            )
            .unwrap();

        let high_id = docket
            .submit_with_options(
                "task",
                serde_json::json!({"priority": "high"}),
                SubmitOptions::new().with_priority(10),
            )
            .unwrap();

        // High priority should be dequeued first
        let worker = docket
            .worker()
            .subscribe("task", |t| async move { Ok(t.params) })
            .build();

        let types = worker.subscribed_types();
        let dequeued = docket.backend.dequeue(&types).unwrap().unwrap();
        assert_eq!(dequeued.id, high_id);

        // Ack it
        docket.backend.ack(&high_id, serde_json::json!({})).unwrap();

        // Now low priority should be available
        let dequeued = docket.backend.dequeue(&types).unwrap().unwrap();
        assert_eq!(dequeued.id, low_id);
    }

    #[test]
    fn test_docket_cancel_task() {
        let docket = Docket::memory();

        let task_id = docket.submit("task", serde_json::json!({})).unwrap();

        docket.cancel(&task_id, Some("User cancelled")).unwrap();

        let task = docket.get_task(&task_id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.error, Some("User cancelled".to_string()));
    }

    #[test]
    fn test_docket_stats() {
        let docket = Docket::memory();

        docket.submit("task1", serde_json::json!({})).unwrap();
        docket.submit("task2", serde_json::json!({})).unwrap();
        let task3 = docket.submit("task3", serde_json::json!({})).unwrap();
        docket.cancel(&task3, None).unwrap();

        let stats = docket.stats().unwrap();
        assert_eq!(stats.pending, 2);
        assert_eq!(stats.cancelled, 1);
    }

    #[test]
    fn test_docket_list_tasks() {
        let docket = Docket::memory();

        docket.submit("type_a", serde_json::json!({})).unwrap();
        docket.submit("type_b", serde_json::json!({})).unwrap();
        let cancelled_id = docket.submit("type_a", serde_json::json!({})).unwrap();
        docket.cancel(&cancelled_id, None).unwrap();

        // All tasks
        let all = docket.list_tasks(None, 100).unwrap();
        assert_eq!(all.len(), 3);

        // Pending only
        let pending = docket.list_tasks(Some(TaskStatus::Pending), 100).unwrap();
        assert_eq!(pending.len(), 2);

        // Cancelled only
        let cancelled = docket.list_tasks(Some(TaskStatus::Cancelled), 100).unwrap();
        assert_eq!(cancelled.len(), 1);
    }

    #[test]
    fn test_worker_builder() {
        let docket = Docket::memory();

        let worker = docket
            .worker()
            .subscribe("type_a", |_| async { Ok(serde_json::json!({})) })
            .subscribe("type_b", |_| async { Ok(serde_json::json!({})) })
            .build();

        let types = worker.subscribed_types();
        assert!(types.contains(&"type_a".to_string()));
        assert!(types.contains(&"type_b".to_string()));
    }

    #[test]
    fn test_memory_backend_retry() {
        let settings = DocketSettings::memory().with_max_retries(2);
        let backend = MemoryDocketBackend::new(settings);

        let task = DocketTask::new(
            TaskId::from_string("test-1"),
            "retry_test".to_string(),
            serde_json::json!({}),
            0,
            2,
        );

        backend.enqueue(task).unwrap();

        // Dequeue and nack (first failure)
        let task = backend
            .dequeue(&["retry_test".to_string()])
            .unwrap()
            .unwrap();
        backend.nack(&task.id, "error 1").unwrap();

        // Should be requeued
        let task = backend
            .dequeue(&["retry_test".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(task.retry_count, 1);
        backend.nack(&task.id, "error 2").unwrap();

        // Max retries exceeded - should be failed, not requeued
        let task = backend.dequeue(&["retry_test".to_string()]).unwrap();
        assert!(task.is_none());

        // Verify it's marked as failed
        let task = backend
            .get_task(&TaskId::from_string("test-1"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
    }

    #[test]
    fn test_docket_task_to_info() {
        let task = DocketTask::new(
            TaskId::from_string("test-info"),
            "test_type".to_string(),
            serde_json::json!({"data": 42}),
            5,
            3,
        );

        let info = task.to_task_info();
        assert_eq!(info.id.to_string(), "test-info");
        assert_eq!(info.task_type, "test_type");
        assert_eq!(info.status, TaskStatus::Pending);
        assert!(info.started_at.is_none());
    }

    #[test]
    fn test_worker_process_one() {
        use fastmcp_core::block_on;

        let docket = Docket::memory();

        // Submit a task
        let task_id = docket
            .submit("process_test", serde_json::json!({"x": 1}))
            .unwrap();

        // Create worker
        let worker = docket
            .worker()
            .subscribe("process_test", |task| async move {
                let x = task.params.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                Ok(serde_json::json!({"result": x * 2}))
            })
            .build();

        // Process
        let cx = Cx::for_testing();
        let processed = block_on(worker.process_one(&cx)).unwrap();
        assert!(processed);

        // Verify completion
        let task = docket.get_task(&task_id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.result, Some(serde_json::json!({"result": 2})));
    }

    #[test]
    fn test_worker_no_task_available() {
        use fastmcp_core::block_on;

        let docket = Docket::memory();

        let worker = docket
            .worker()
            .subscribe("empty_test", |_| async { Ok(serde_json::json!({})) })
            .build();

        let cx = Cx::for_testing();
        let processed = block_on(worker.process_one(&cx)).unwrap();
        assert!(!processed);
    }

    #[test]
    fn test_submit_options() {
        let opts = SubmitOptions::new()
            .with_priority(10)
            .with_max_retries(5)
            .with_delay(Duration::from_secs(60));

        assert_eq!(opts.priority, 10);
        assert_eq!(opts.max_retries, Some(5));
        assert_eq!(opts.delay, Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_docket_error_display() {
        let errors = vec![
            (
                DocketError::NotFound("task-1".into()),
                "Task not found: task-1",
            ),
            (
                DocketError::Connection("refused".into()),
                "Connection error: refused",
            ),
            (DocketError::Handler("panic".into()), "Handler error: panic"),
            (DocketError::Cancelled, "Operation cancelled"),
        ];

        for (error, expected) in errors {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[cfg(feature = "redis")]
    #[test]
    fn test_redis_worker_process_round_trip() {
        use fastmcp_core::block_on;

        let redis_server = TestRedisServer::start();
        let settings = redis_settings_for_test(&redis_server.url);
        let docket = Docket::new(settings).expect("redis docket");

        let task_id = docket
            .submit("redis_round_trip", serde_json::json!({"value": 21}))
            .expect("submit");

        let worker = docket
            .worker()
            .subscribe("redis_round_trip", |task| async move {
                let value = task
                    .params
                    .get("value")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                Ok(serde_json::json!({"doubled": value * 2}))
            })
            .build();

        let cx = Cx::for_testing();
        let processed = block_on(worker.process_one(&cx)).expect("process one");
        assert!(processed);

        let task = docket
            .get_task(&task_id)
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.result, Some(serde_json::json!({"doubled": 42})));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn test_redis_worker_retries_then_marks_failed() {
        use fastmcp_core::block_on;

        let redis_server = TestRedisServer::start();
        let mut settings = redis_settings_for_test(&redis_server.url);
        settings.max_retries = 2;
        let docket = Docket::new(settings).expect("redis docket");

        let task_id = docket
            .submit("redis_retry", serde_json::json!({"attempt": 0}))
            .expect("submit");

        let worker = docket
            .worker()
            .subscribe("redis_retry", |_task| async move {
                Err(DocketError::Handler("boom".to_string()))
            })
            .build();

        let cx = Cx::for_testing();
        assert!(block_on(worker.process_one(&cx)).expect("process first"));
        assert!(block_on(worker.process_one(&cx)).expect("process second"));

        let task = docket
            .get_task(&task_id)
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.retry_count, 2);
        assert!(task.error.unwrap_or_default().contains("boom"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn test_redis_cancel_pending_task() {
        let redis_server = TestRedisServer::start();
        let settings = redis_settings_for_test(&redis_server.url);
        let docket = Docket::new(settings).expect("redis docket");

        let task_id = docket
            .submit("redis_cancel", serde_json::json!({"x": 1}))
            .expect("submit");
        docket
            .cancel(&task_id, Some("stopped by test"))
            .expect("cancel task");

        let task = docket
            .get_task(&task_id)
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.error, Some("stopped by test".to_string()));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn test_redis_requeue_stale_running_task() {
        let redis_server = TestRedisServer::start();
        let mut settings = redis_settings_for_test(&redis_server.url);
        settings.visibility_timeout = Duration::from_millis(0);
        let docket = Docket::new(settings).expect("redis docket");

        let task_id = docket
            .submit("redis_stale", serde_json::json!({"x": 1}))
            .expect("submit");

        let task_types = vec!["redis_stale".to_string()];
        let claimed = docket
            .backend
            .dequeue(&task_types)
            .expect("dequeue")
            .expect("claimed task");
        assert_eq!(claimed.id, task_id);
        assert_eq!(claimed.status, TaskStatus::Running);

        let requeued = docket.backend.requeue_stale().expect("requeue stale");
        assert_eq!(requeued, 1);

        let reclaimed = docket
            .backend
            .dequeue(&task_types)
            .expect("dequeue after requeue")
            .expect("task available again");
        assert_eq!(reclaimed.id, task_id);
        assert_eq!(reclaimed.retry_count, 1);
    }

    // ========================================
    // DocketSettings — additional
    // ========================================

    #[test]
    fn docket_settings_memory_equals_default() {
        let mem = DocketSettings::memory();
        let def = DocketSettings::default();
        assert_eq!(mem.queue_prefix, def.queue_prefix);
        assert_eq!(mem.max_retries, def.max_retries);
        assert_eq!(mem.visibility_timeout, def.visibility_timeout);
        assert_eq!(mem.default_task_timeout, def.default_task_timeout);
        assert_eq!(mem.retry_delay, def.retry_delay);
        assert_eq!(mem.poll_interval, def.poll_interval);
    }

    #[test]
    fn docket_settings_with_visibility_timeout() {
        let s = DocketSettings::memory().with_visibility_timeout(Duration::from_secs(120));
        assert_eq!(s.visibility_timeout, Duration::from_secs(120));
    }

    #[test]
    fn docket_settings_debug() {
        let s = DocketSettings::memory();
        let debug = format!("{:?}", s);
        assert!(debug.contains("DocketSettings"));
        assert!(debug.contains("fastmcp:docket"));
    }

    #[test]
    fn docket_settings_clone() {
        let s = DocketSettings::memory().with_max_retries(7);
        let c = s.clone();
        assert_eq!(c.max_retries, 7);
    }

    #[test]
    fn docket_settings_redis_pool_size() {
        let s = DocketSettings::redis("redis://localhost:6379");
        match s.backend {
            DocketBackendType::Redis(ref r) => {
                assert_eq!(r.pool_size, 10);
                assert_eq!(r.connect_timeout, Duration::from_secs(5));
                assert_eq!(r.url, "redis://localhost:6379");
            }
            _ => panic!("expected Redis backend"),
        }
    }

    // ========================================
    // DocketBackendType / RedisSettings
    // ========================================

    #[test]
    fn docket_backend_type_debug() {
        let mem = DocketBackendType::Memory;
        let debug = format!("{:?}", mem);
        assert!(debug.contains("Memory"));

        let redis = DocketBackendType::Redis(RedisSettings {
            url: "redis://test".to_string(),
            pool_size: 5,
            connect_timeout: Duration::from_secs(3),
        });
        let debug = format!("{:?}", redis);
        assert!(debug.contains("Redis"));
    }

    #[test]
    fn redis_settings_clone() {
        let r = RedisSettings {
            url: "redis://host".to_string(),
            pool_size: 3,
            connect_timeout: Duration::from_secs(2),
        };
        let c = r.clone();
        assert_eq!(c.url, "redis://host");
        assert_eq!(c.pool_size, 3);
    }

    // ========================================
    // DocketTask
    // ========================================

    #[test]
    fn docket_task_new_fields() {
        let task = DocketTask::new(
            TaskId::from_string("t-1"),
            "my_type".to_string(),
            serde_json::json!({"key": "val"}),
            5,
            3,
        );
        assert_eq!(task.task_type, "my_type");
        assert_eq!(task.priority, 5);
        assert_eq!(task.max_retries, 3);
        assert_eq!(task.retry_count, 0);
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.claimed_at.is_none());
        assert!(task.error.is_none());
        assert!(task.result.is_none());
        assert!(!task.created_at.is_empty());
    }

    #[test]
    fn docket_task_debug_and_clone() {
        let task = DocketTask::new(
            TaskId::from_string("t-dbg"),
            "dbg_type".to_string(),
            serde_json::json!(null),
            0,
            1,
        );
        let debug = format!("{:?}", task);
        assert!(debug.contains("DocketTask"));
        assert!(debug.contains("dbg_type"));

        let cloned = task.clone();
        assert_eq!(cloned.task_type, "dbg_type");
    }

    #[test]
    fn docket_task_serialize_deserialize_roundtrip() {
        let task = DocketTask::new(
            TaskId::from_string("t-ser"),
            "ser_type".to_string(),
            serde_json::json!({"a": 1}),
            2,
            4,
        );
        let json = serde_json::to_string(&task).unwrap();
        let deserialized: DocketTask = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.task_type, "ser_type");
        assert_eq!(deserialized.priority, 2);
        assert_eq!(deserialized.max_retries, 4);
    }

    #[test]
    fn docket_task_to_task_info_pending() {
        let task = DocketTask::new(
            TaskId::from_string("t-info"),
            "info_type".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        let info = task.to_task_info();
        assert_eq!(info.status, TaskStatus::Pending);
        assert!(info.started_at.is_none());
        assert!(info.completed_at.is_none()); // Pending is not terminal
    }

    #[test]
    fn docket_task_to_task_info_completed() {
        let mut task = DocketTask::new(
            TaskId::from_string("t-comp"),
            "comp_type".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Completed;
        task.claimed_at = Some("2025-01-01T00:00:00Z".to_string());
        let info = task.to_task_info();
        assert_eq!(info.status, TaskStatus::Completed);
        assert!(info.started_at.is_some());
        assert!(info.completed_at.is_some()); // Completed is terminal
    }

    #[test]
    fn docket_task_to_task_result_non_terminal() {
        let task = DocketTask::new(
            TaskId::from_string("t-res"),
            "res_type".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        assert!(task.to_task_result().is_none());
    }

    #[test]
    fn docket_task_to_task_result_completed() {
        let mut task = DocketTask::new(
            TaskId::from_string("t-res2"),
            "res_type".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Completed;
        task.result = Some(serde_json::json!({"data": 42}));
        let result = task.to_task_result().unwrap();
        assert!(result.success);
        assert_eq!(result.data, Some(serde_json::json!({"data": 42})));
        assert!(result.error.is_none());
    }

    #[test]
    fn docket_task_to_task_result_failed() {
        let mut task = DocketTask::new(
            TaskId::from_string("t-fail"),
            "fail_type".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Failed;
        task.error = Some("something broke".to_string());
        let result = task.to_task_result().unwrap();
        assert!(!result.success);
        assert_eq!(result.error, Some("something broke".to_string()));
    }

    // ========================================
    // SubmitOptions — additional
    // ========================================

    #[test]
    fn submit_options_default() {
        let opts = SubmitOptions::default();
        assert_eq!(opts.priority, 0);
        assert!(opts.max_retries.is_none());
        assert!(opts.delay.is_none());
    }

    #[test]
    fn submit_options_debug() {
        let opts = SubmitOptions::new().with_priority(3);
        let debug = format!("{:?}", opts);
        assert!(debug.contains("SubmitOptions"));
        assert!(debug.contains('3'));
    }

    // ========================================
    // DocketError — additional
    // ========================================

    #[test]
    fn docket_error_serialization_display() {
        let err = DocketError::Serialization("bad json".to_string());
        assert_eq!(err.to_string(), "Serialization error: bad json");
    }

    #[test]
    fn docket_error_backend_display() {
        let err = DocketError::Backend("lock poisoned".to_string());
        assert_eq!(err.to_string(), "Backend error: lock poisoned");
    }

    #[test]
    fn docket_error_is_std_error() {
        let err = DocketError::NotFound("x".to_string());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn docket_error_into_mcp_error() {
        let err = DocketError::Handler("timeout".to_string());
        let mcp: McpError = err.into();
        assert!(mcp.message.contains("Handler error: timeout"));
    }

    #[test]
    fn docket_error_debug() {
        let err = DocketError::Cancelled;
        let debug = format!("{:?}", err);
        assert!(debug.contains("Cancelled"));
    }

    // ========================================
    // QueueStats
    // ========================================

    #[test]
    fn queue_stats_default() {
        let stats = QueueStats::default();
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.in_progress, 0);
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.cancelled, 0);
    }

    #[test]
    fn queue_stats_debug_and_clone() {
        let stats = QueueStats {
            pending: 1,
            in_progress: 2,
            completed: 3,
            failed: 4,
            cancelled: 5,
        };
        let debug = format!("{:?}", stats);
        assert!(debug.contains("QueueStats"));

        let c = stats.clone();
        assert_eq!(c.pending, 1);
        assert_eq!(c.completed, 3);
    }

    // ========================================
    // MemoryDocketBackend — additional
    // ========================================

    #[test]
    fn memory_backend_dequeue_empty() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let result = backend.dequeue(&["any".to_string()]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn memory_backend_dequeue_wrong_type() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let task = DocketTask::new(
            TaskId::from_string("t-1"),
            "type_a".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        // Request wrong type — should return None
        let result = backend.dequeue(&["type_b".to_string()]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn memory_backend_dequeue_sets_running() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let task = DocketTask::new(
            TaskId::from_string("t-run"),
            "work".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        let dequeued = backend.dequeue(&["work".to_string()]).unwrap().unwrap();
        assert_eq!(dequeued.status, TaskStatus::Running);
        assert!(dequeued.claimed_at.is_some());
    }

    #[test]
    fn memory_backend_ack_nonexistent() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let result = backend.ack(&TaskId::from_string("nonexistent"), serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn memory_backend_nack_nonexistent() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let result = backend.nack(&TaskId::from_string("nonexistent"), "err");
        assert!(result.is_err());
    }

    #[test]
    fn memory_backend_get_task_nonexistent() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let result = backend.get_task(&TaskId::from_string("missing")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn memory_backend_cancel_terminal_task_fails() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let task = DocketTask::new(
            TaskId::from_string("t-can"),
            "cancel_test".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        let id = TaskId::from_string("t-can");
        backend.cancel(&id, None).unwrap();

        // Cancelling again should fail (already in terminal state)
        let result = backend.cancel(&id, Some("again"));
        assert!(result.is_err());
    }

    #[test]
    fn memory_backend_cancel_removes_from_pending() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let task = DocketTask::new(
            TaskId::from_string("t-crp"),
            "cancel_pend".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        let id = TaskId::from_string("t-crp");
        backend.cancel(&id, Some("bye")).unwrap();

        // Should not be dequeued
        let result = backend.dequeue(&["cancel_pend".to_string()]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn memory_backend_list_tasks_with_limit() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        for i in 0..5 {
            let task = DocketTask::new(
                TaskId::from_string(format!("t-{i}")),
                "limit_test".to_string(),
                serde_json::json!({}),
                0,
                3,
            );
            backend.enqueue(task).unwrap();
        }

        let tasks = backend.list_tasks(None, 3).unwrap();
        assert_eq!(tasks.len(), 3);
    }

    #[test]
    fn memory_backend_list_tasks_filter_by_status() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        for i in 0..3 {
            let task = DocketTask::new(
                TaskId::from_string(format!("t-{i}")),
                "filter_test".to_string(),
                serde_json::json!({}),
                0,
                3,
            );
            backend.enqueue(task).unwrap();
        }
        // Cancel one
        backend.cancel(&TaskId::from_string("t-1"), None).unwrap();

        let pending = backend.list_tasks(Some(TaskStatus::Pending), 100).unwrap();
        assert_eq!(pending.len(), 2);

        let cancelled = backend
            .list_tasks(Some(TaskStatus::Cancelled), 100)
            .unwrap();
        assert_eq!(cancelled.len(), 1);
    }

    #[test]
    fn memory_backend_stats_all_statuses() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());

        // Create pending
        for i in 0..2 {
            let task = DocketTask::new(
                TaskId::from_string(format!("p-{i}")),
                "stat_test".to_string(),
                serde_json::json!({}),
                0,
                3,
            );
            backend.enqueue(task).unwrap();
        }

        // Dequeue one → Running
        let _running = backend.dequeue(&["stat_test".to_string()]).unwrap();

        // Ack → Completed (re-create and dequeue another)
        let task = DocketTask::new(
            TaskId::from_string("c-0"),
            "stat_test".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();
        let deq = backend
            .dequeue(&["stat_test".to_string()])
            .unwrap()
            .unwrap();
        backend.ack(&deq.id, serde_json::json!({})).unwrap();

        let stats = backend.stats().unwrap();
        assert!(stats.pending >= 1);
        assert!(stats.in_progress >= 1 || stats.completed >= 1);
    }

    #[test]
    fn memory_backend_requeue_stale_no_stale() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let task = DocketTask::new(
            TaskId::from_string("t-fresh"),
            "stale_test".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        // Dequeue but with a long visibility timeout — should not be stale
        let _deq = backend
            .dequeue(&["stale_test".to_string()])
            .unwrap()
            .unwrap();
        let requeued = backend.requeue_stale().unwrap();
        assert_eq!(requeued, 0);
    }

    #[test]
    fn memory_backend_ack_marks_completed() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let task = DocketTask::new(
            TaskId::from_string("t-ack"),
            "ack_test".to_string(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        let deq = backend.dequeue(&["ack_test".to_string()]).unwrap().unwrap();
        backend
            .ack(&deq.id, serde_json::json!({"done": true}))
            .unwrap();

        let task = backend
            .get_task(&TaskId::from_string("t-ack"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.result, Some(serde_json::json!({"done": true})));
    }

    #[test]
    fn memory_backend_priority_ordering() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());

        // Enqueue low priority first, then high
        let low = DocketTask::new(
            TaskId::from_string("low"),
            "prio".to_string(),
            serde_json::json!({}),
            1,
            3,
        );
        let high = DocketTask::new(
            TaskId::from_string("high"),
            "prio".to_string(),
            serde_json::json!({}),
            10,
            3,
        );
        backend.enqueue(low).unwrap();
        backend.enqueue(high).unwrap();

        // High priority dequeued first
        let first = backend.dequeue(&["prio".to_string()]).unwrap().unwrap();
        assert_eq!(first.id.to_string(), "high");
    }

    // ========================================
    // Docket — additional
    // ========================================

    #[test]
    fn docket_debug() {
        let docket = Docket::memory();
        let debug = format!("{:?}", docket);
        assert!(debug.contains("Docket"));
        assert!(debug.contains("settings"));
    }

    #[test]
    fn docket_settings_accessor() {
        let docket = Docket::memory();
        let settings = docket.settings();
        assert!(matches!(settings.backend, DocketBackendType::Memory));
    }

    #[test]
    fn docket_into_shared() {
        let docket = Docket::memory();
        let shared: SharedDocket = docket.into_shared();
        // Can be cloned (Arc)
        let _clone = Arc::clone(&shared);
        assert!(matches!(
            shared.settings().backend,
            DocketBackendType::Memory
        ));
    }

    #[test]
    fn docket_get_task_nonexistent() {
        let docket = Docket::memory();
        let result = docket
            .get_task(&TaskId::from_string("no-such-task"))
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn docket_cancel_nonexistent_is_error() {
        let docket = Docket::memory();
        let result = docket.cancel(&TaskId::from_string("no-such"), None);
        assert!(result.is_err());
    }

    #[test]
    fn docket_task_ids_are_sequential() {
        let docket = Docket::memory();
        let id1 = docket.submit("seq", serde_json::json!({})).unwrap();
        let id2 = docket.submit("seq", serde_json::json!({})).unwrap();
        // IDs should be different (sequential counter)
        assert_ne!(id1.to_string(), id2.to_string());
        assert!(id1.to_string().starts_with("docket-"));
        assert!(id2.to_string().starts_with("docket-"));
    }

    #[test]
    fn docket_submit_with_max_retries_override() {
        let docket = Docket::memory();
        let id = docket
            .submit_with_options(
                "retry_over",
                serde_json::json!({}),
                SubmitOptions::new().with_max_retries(10),
            )
            .unwrap();

        let task = docket.get_task(&id).unwrap().unwrap();
        assert_eq!(task.max_retries, 10);
    }

    #[cfg(not(feature = "redis"))]
    #[test]
    fn docket_new_redis_without_feature_fails() {
        let settings = DocketSettings::redis("redis://localhost:6379");
        let result = Docket::new(settings);
        assert!(result.is_err());
    }

    // ========================================
    // Worker — additional
    // ========================================

    #[test]
    fn worker_is_running_initially_false() {
        let docket = Docket::memory();
        let worker = docket
            .worker()
            .subscribe("test", |_| async { Ok(serde_json::json!({})) })
            .build();
        assert!(!worker.is_running());
    }

    #[test]
    fn worker_stop() {
        let docket = Docket::memory();
        let worker = docket
            .worker()
            .subscribe("test", |_| async { Ok(serde_json::json!({})) })
            .build();

        // Manually set running
        worker.running.store(true, Ordering::SeqCst);
        assert!(worker.is_running());

        worker.stop();
        assert!(!worker.is_running());
    }

    #[test]
    fn worker_debug() {
        let docket = Docket::memory();
        let worker = docket
            .worker()
            .subscribe("type_x", |_| async { Ok(serde_json::json!({})) })
            .build();
        let debug = format!("{:?}", worker);
        assert!(debug.contains("Worker"));
        assert!(debug.contains("type_x"));
    }

    #[test]
    fn worker_process_one_handler_error_nacks() {
        use fastmcp_core::block_on;

        let settings = DocketSettings::memory().with_max_retries(2);
        let docket = Docket::new(settings).unwrap();

        let id = docket.submit("fail_type", serde_json::json!({})).unwrap();

        let worker = docket
            .worker()
            .subscribe("fail_type", |_| async {
                Err(DocketError::Handler("boom".to_string()))
            })
            .build();

        let cx = Cx::for_testing();
        // First failure — nack, requeue
        let processed = block_on(worker.process_one(&cx)).unwrap();
        assert!(processed);

        // Task should be requeued (retry_count=1, still < max_retries=2)
        let task = docket.get_task(&id).unwrap().unwrap();
        assert_eq!(task.retry_count, 1);
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.error.as_deref().unwrap().contains("boom"));

        // Second failure — should exceed max retries
        let processed = block_on(worker.process_one(&cx)).unwrap();
        assert!(processed);

        let task = docket.get_task(&id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.retry_count, 2);
    }

    // ========================================
    // DocketSettings — additional
    // ========================================

    #[test]
    fn docket_settings_with_poll_interval() {
        let s = DocketSettings::memory().with_poll_interval(Duration::from_millis(500));
        assert_eq!(s.poll_interval, Duration::from_millis(500));
    }

    #[test]
    fn docket_settings_with_max_retries() {
        let s = DocketSettings::memory().with_max_retries(10);
        assert_eq!(s.max_retries, 10);
    }

    // ========================================
    // SubmitOptions — clone
    // ========================================

    #[test]
    fn submit_options_clone() {
        let opts = SubmitOptions::new().with_priority(5).with_max_retries(3);
        let cloned = opts.clone();
        assert_eq!(cloned.priority, 5);
        assert_eq!(cloned.max_retries, Some(3));
    }

    // ========================================
    // DocketTask::to_task_info — additional statuses
    // ========================================

    #[test]
    fn docket_task_to_task_info_running_has_started_at() {
        let mut task = DocketTask::new(
            TaskId::from_string("t"),
            "type".into(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Running;
        task.claimed_at = Some("2026-01-01T00:00:00Z".to_string());

        let info = task.to_task_info();
        assert_eq!(info.status, TaskStatus::Running);
        assert_eq!(info.started_at, Some("2026-01-01T00:00:00Z".to_string()));
        // Not terminal, so completed_at should be None
        assert!(info.completed_at.is_none());
    }

    #[test]
    fn docket_task_to_task_info_cancelled_has_completed_at() {
        let mut task = DocketTask::new(
            TaskId::from_string("t"),
            "type".into(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Cancelled;
        task.error = Some("user cancelled".to_string());

        let info = task.to_task_info();
        assert_eq!(info.status, TaskStatus::Cancelled);
        assert!(info.completed_at.is_some()); // terminal
        assert_eq!(info.error, Some("user cancelled".to_string()));
    }

    #[test]
    fn docket_task_to_task_result_cancelled() {
        let mut task = DocketTask::new(
            TaskId::from_string("t"),
            "type".into(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Cancelled;
        task.error = Some("cancelled by user".to_string());

        let result = task.to_task_result().expect("terminal");
        assert!(!result.success);
        assert_eq!(result.error, Some("cancelled by user".to_string()));
    }

    // ========================================
    // Memory backend — cancel running
    // ========================================

    #[test]
    fn memory_backend_cancel_running_task() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let task = DocketTask::new(
            TaskId::from_string("t1"),
            "work".into(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        // Dequeue to set running
        let _claimed = backend.dequeue(&["work".to_string()]).unwrap().unwrap();

        // Cancel while running
        backend
            .cancel(&TaskId::from_string("t1"), Some("force cancel"))
            .unwrap();

        let task = backend
            .get_task(&TaskId::from_string("t1"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.error, Some("force cancel".to_string()));
    }

    // ========================================
    // Memory backend — requeue stale
    // ========================================

    #[test]
    fn memory_backend_requeue_stale_with_stale_task() {
        let settings = DocketSettings::memory().with_visibility_timeout(Duration::from_millis(0));
        let backend = MemoryDocketBackend::new(settings);

        let mut task = DocketTask::new(
            TaskId::from_string("t1"),
            "work".into(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Running;
        task.claimed_at = Some("2000-01-01T00:00:00Z".to_string()); // long ago
        backend.enqueue(task).unwrap();

        // Manually set status to Running (enqueue sets Pending, we need to update)
        {
            let mut tasks = backend.tasks.write().unwrap();
            let t = tasks.get_mut(&TaskId::from_string("t1")).unwrap();
            t.status = TaskStatus::Running;
            t.claimed_at = Some("2000-01-01T00:00:00Z".to_string());
        }

        let requeued = backend.requeue_stale().unwrap();
        assert_eq!(requeued, 1);

        let task = backend
            .get_task(&TaskId::from_string("t1"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.claimed_at.is_none());
        assert_eq!(task.retry_count, 1);
    }

    #[test]
    fn memory_backend_requeue_stale_at_retry_limit_marks_failed() {
        let settings = DocketSettings::memory()
            .with_visibility_timeout(Duration::from_millis(0))
            .with_max_retries(1);
        let backend = MemoryDocketBackend::new(settings);

        let task = DocketTask::new(
            TaskId::from_string("t1"),
            "work".into(),
            serde_json::json!({}),
            0,
            1, // max_retries = 1
        );
        backend.enqueue(task).unwrap();

        // Manually set as running and stale
        {
            let mut tasks = backend.tasks.write().unwrap();
            let t = tasks.get_mut(&TaskId::from_string("t1")).unwrap();
            t.status = TaskStatus::Running;
            t.claimed_at = Some("2000-01-01T00:00:00Z".to_string());
        }

        let requeued = backend.requeue_stale().unwrap();
        assert_eq!(requeued, 0); // Failed, not requeued

        let task = backend
            .get_task(&TaskId::from_string("t1"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error.as_deref(), Some("Exceeded visibility timeout"));
    }

    // ========================================
    // Memory backend — nack retry vs fail
    // ========================================

    #[test]
    fn memory_backend_nack_under_limit_requeues() {
        let settings = DocketSettings::memory().with_max_retries(3);
        let backend = MemoryDocketBackend::new(settings);

        let task = DocketTask::new(
            TaskId::from_string("t1"),
            "work".into(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        // Dequeue to set running
        let _claimed = backend.dequeue(&["work".to_string()]).unwrap().unwrap();

        // First nack — requeued
        backend.nack(&TaskId::from_string("t1"), "fail1").unwrap();

        let task = backend
            .get_task(&TaskId::from_string("t1"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.retry_count, 1);
    }

    #[test]
    fn memory_backend_nack_at_limit_marks_failed() {
        let settings = DocketSettings::memory().with_max_retries(1);
        let backend = MemoryDocketBackend::new(settings);

        let task = DocketTask::new(
            TaskId::from_string("t1"),
            "work".into(),
            serde_json::json!({}),
            0,
            1,
        );
        backend.enqueue(task).unwrap();

        // Dequeue to set running
        let _claimed = backend.dequeue(&["work".to_string()]).unwrap().unwrap();

        // First nack at max_retries=1 — fails
        backend.nack(&TaskId::from_string("t1"), "fatal").unwrap();

        let task = backend
            .get_task(&TaskId::from_string("t1"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.retry_count, 1);
    }

    // ========================================
    // Memory backend — dequeue multi-type
    // ========================================

    #[test]
    fn memory_backend_dequeue_multiple_types() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());

        let task_a = DocketTask::new(
            TaskId::from_string("a"),
            "type_a".into(),
            serde_json::json!({}),
            0,
            3,
        );
        let task_b = DocketTask::new(
            TaskId::from_string("b"),
            "type_b".into(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task_a).unwrap();
        backend.enqueue(task_b).unwrap();

        // Dequeue accepting both types — should get first enqueued
        let claimed = backend
            .dequeue(&["type_a".to_string(), "type_b".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(claimed.task_type, "type_a");
    }

    // ========================================
    // Docket::memory constructor
    // ========================================

    #[test]
    fn docket_memory_constructor() {
        let d = Docket::memory();
        assert!(matches!(d.settings().backend, DocketBackendType::Memory));
    }

    // ========================================
    // Worker — subscribed_types order
    // ========================================

    #[test]
    fn worker_subscribed_types_contains_all() {
        let docket = Docket::memory();
        let worker = docket
            .worker()
            .subscribe("x", |_| async { Ok(serde_json::json!(1)) })
            .subscribe("y", |_| async { Ok(serde_json::json!(2)) })
            .subscribe("z", |_| async { Ok(serde_json::json!(3)) })
            .build();

        let types = worker.subscribed_types();
        assert_eq!(types.len(), 3);
        assert!(types.contains(&"x".to_string()));
        assert!(types.contains(&"y".to_string()));
        assert!(types.contains(&"z".to_string()));
    }

    // ========================================
    // Worker — process_one with cancelled cx
    // ========================================

    #[test]
    fn worker_process_one_cancelled_cx_returns_error() {
        use fastmcp_core::block_on;

        let docket = Docket::memory();
        docket.submit("cancel_test", serde_json::json!({})).unwrap();

        let worker = docket
            .worker()
            .subscribe("cancel_test", |_| async { Ok(serde_json::json!({})) })
            .build();

        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let result = block_on(worker.process_one(&cx));
        assert!(result.is_err());
    }

    // ========================================
    // Cancel with None reason sets error to None
    // ========================================

    #[test]
    fn cancel_with_none_reason_sets_no_error() {
        let docket = Docket::memory();
        let id = docket.submit("t", serde_json::json!({})).unwrap();
        docket.cancel(&id, None).unwrap();

        let task = docket.get_task(&id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert!(task.error.is_none());
    }

    // ========================================
    // to_task_info for Failed status
    // ========================================

    #[test]
    fn docket_task_to_task_info_failed_has_completed_at() {
        let mut task = DocketTask::new(
            TaskId::from_string("t"),
            "type".into(),
            serde_json::json!({}),
            0,
            3,
        );
        task.status = TaskStatus::Failed;
        task.error = Some("crash".to_string());

        let info = task.to_task_info();
        assert_eq!(info.status, TaskStatus::Failed);
        assert!(info.completed_at.is_some()); // Failed is terminal
        assert_eq!(info.error, Some("crash".to_string()));
    }

    // ========================================
    // Memory backend cancel nonexistent
    // ========================================

    #[test]
    fn memory_backend_cancel_nonexistent_returns_error() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());
        let result = backend.cancel(&TaskId::from_string("no-such"), None);
        assert!(result.is_err());
    }

    // ========================================
    // submit_with_options uses default max_retries
    // ========================================

    #[test]
    fn submit_with_options_uses_default_max_retries() {
        let settings = DocketSettings::memory().with_max_retries(7);
        let docket = Docket::new(settings).unwrap();

        // Submit without overriding max_retries
        let id = docket
            .submit_with_options("t", serde_json::json!({}), SubmitOptions::default())
            .unwrap();

        let task = docket.get_task(&id).unwrap().unwrap();
        assert_eq!(task.max_retries, 7); // inherited from settings
    }

    // ========================================
    // Same-priority FIFO ordering
    // ========================================

    #[test]
    fn memory_backend_same_priority_fifo() {
        let backend = MemoryDocketBackend::new(DocketSettings::memory());

        for i in 0..3 {
            let task = DocketTask::new(
                TaskId::from_string(format!("t-{i}")),
                "fifo".into(),
                serde_json::json!({}),
                0, // same priority
                3,
            );
            backend.enqueue(task).unwrap();
        }

        let first = backend.dequeue(&["fifo".to_string()]).unwrap().unwrap();
        assert_eq!(first.id.to_string(), "t-0");
        let second = backend.dequeue(&["fifo".to_string()]).unwrap().unwrap();
        assert_eq!(second.id.to_string(), "t-1");
        let third = backend.dequeue(&["fifo".to_string()]).unwrap().unwrap();
        assert_eq!(third.id.to_string(), "t-2");
    }

    // ========================================
    // requeue_stale skips invalid claimed_at
    // ========================================

    #[test]
    fn memory_backend_requeue_stale_skips_invalid_claimed_at() {
        let settings = DocketSettings::memory().with_visibility_timeout(Duration::from_millis(0));
        let backend = MemoryDocketBackend::new(settings);

        let task = DocketTask::new(
            TaskId::from_string("t1"),
            "work".into(),
            serde_json::json!({}),
            0,
            3,
        );
        backend.enqueue(task).unwrap();

        // Set as running with an invalid claimed_at
        {
            let mut tasks = backend.tasks.write().unwrap();
            let t = tasks.get_mut(&TaskId::from_string("t1")).unwrap();
            t.status = TaskStatus::Running;
            t.claimed_at = Some("not-a-valid-timestamp".to_string());
        }

        // Should not requeue (can't parse claimed_at)
        let requeued = backend.requeue_stale().unwrap();
        assert_eq!(requeued, 0);

        // Task should still be Running
        let task = backend
            .get_task(&TaskId::from_string("t1"))
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Running);
    }

    // ========================================
    // DocketError::Cancelled into McpError
    // ========================================

    #[test]
    fn docket_error_cancelled_into_mcp_error() {
        let err = DocketError::Cancelled;
        let mcp: McpError = err.into();
        assert!(mcp.message.contains("cancelled"));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn test_redis_requeue_stale_marks_failed_at_retry_limit() {
        let redis_server = TestRedisServer::start();
        let mut settings = redis_settings_for_test(&redis_server.url);
        settings.visibility_timeout = Duration::from_millis(0);
        settings.max_retries = 1;
        let docket = Docket::new(settings).expect("redis docket");

        let task_id = docket
            .submit("redis_stale_fail", serde_json::json!({}))
            .expect("submit");

        let task_types = vec!["redis_stale_fail".to_string()];
        let _claimed = docket
            .backend
            .dequeue(&task_types)
            .expect("dequeue")
            .expect("claimed");

        let requeued = docket.backend.requeue_stale().expect("requeue stale");
        assert_eq!(requeued, 0);

        let task = docket
            .get_task(&task_id)
            .expect("get task")
            .expect("task exists");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error.as_deref(), Some("Exceeded visibility timeout"));
    }
}
