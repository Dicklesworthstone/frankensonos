<p align="center">
  <img src="fastmcp_rust_illustration.webp" alt="FastMCP Rust - cancel-aware MCP framework" width="800">
</p>

<h1 align="center">FastMCP Rust</h1>

<p align="center">
  <strong>Cancel-aware Model Context Protocol (MCP) framework for Rust</strong>
</p>

<p align="center">
  <em>A Rust port of <a href="https://github.com/jlowin/fastmcp">jlowin/fastmcp</a> (Python), extended with <a href="https://github.com/Dicklesworthstone/asupersync">asupersync</a> capability contexts and cooperative-cancellation primitives.</em>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/License-MIT%20with%20rider-blue.svg" alt="MIT License with rider">
  <img src="https://img.shields.io/badge/rust-nightly--2026--08--25-orange.svg" alt="Rust Version">
  <img src="https://img.shields.io/badge/edition-2024-purple.svg" alt="Rust Edition">
  <img src="https://img.shields.io/badge/MCP%202026--07--28-under%20implementation-yellow.svg" alt="MCP status">
</p>

> **Protocol status (2026-08-27):** MCP 2026-07-28 support is under
> implementation and remains unverified. The root compatibility
> `PROTOCOL_VERSION` is `2024-11-05`; the modern facade's
> `modern::PROTOCOL_VERSION` is `2026-07-28`. Source presence, examples, and
> historical parity rows are not conformance or release evidence. Versions
> through 0.10.0 have been published, but publication and source edits alone do
> not prove historical workflow identities, queued runs, or credentials inert;
> provider-side release-safety evidence is still required.

### Current qualification boundaries

- **Wire cancellation is only partially qualified:** on Unix, the primary
  stdio path keeps receiving while bounded modern requests run in independent
  request-owned children, so it can route cancellation during handler
  execution. Response and notification commits remain serialized at the
  output writer, while exact MCP 2024-11-05 traffic remains serialized through
  its lifecycle worker. Non-Unix stdio and custom/SSE/WebSocket entry points
  retain sequential or blocking boundaries, except that an unsplit custom
  transport runs `subscriptions/listen` off its receive loop and queues output
  produced while `recv` blocks until `recv` returns. A non-cooperative handler can
  still exceed the bounded process-exit drain, so end-to-end quiescence and
  reliable `awaitCleanup` semantics remain unverified.
- **Bidirectional calls are only partly qualified:** the Unix stdio receive
  pump can route sampling, elicitation, and roots responses while exact-2024
  lifecycle work or modern request children are active. The WebSocket server
  and custom split transports share that pump: live `bind_websocket` tests
  show an exact-2024 handler awaiting `ctx.sample` or `ctx.list_roots`
  (including a synchronous handler that bridges with `block_on`) receives the
  client's reply, and a client without the capability gets a typed refusal
  instead of a hang. Non-Unix stdio, unsplit custom transports, and SSE still
  reject or lack that split routing. Public HTTP has its own dual-era request
  and response routing, but end-to-end bidirectional lifecycle/cancellation
  evidence is incomplete.
- **Response caching is conservatively partitioned:** eligible production
  requests are keyed by committed authentication facts plus opaque session
  identity and revision. Uncommitted authentication, local-only state views,
  allocation failure, or state mutation during a request cause cache admission
  to fail closed rather than sharing an entry.
- **Authentication admission is incomplete:** native HTTP accepts credentials
  only from `Authorization`; recognized body/meta and query credential fields
  are rejected before the provider or application runs. Other adapters retain
  a stripped legacy JSON-RPC credential fallback. Complete authorization,
  lease/revocation, and OAuth challenge qualification remain open.
- **Legacy Tasks RPC stays dead:** `tasks/list` and `tasks/submit` return
  JSON-RPC `MethodNotFound`. Official MCP 2026-07-28 methods `tasks/get`,
  `tasks/update`, and `tasks/cancel` are served by default (process-local
  in-memory store). Call `ServerBuilder::final_tasks` to supply an
  application-owned store. Creating new tasks still requires the
  application to run a caller-owned task supervisor in its own `Cx`
  region. The historical `with_task_manager` path does not install the
  official methods.
- **Authenticated local Tasks retain their caller:** task creation atomically
  stores a private principal binding. Request get/update/cancel and Tasks
  subscriptions enforce that binding; a refreshed credential for the same
  verified owner can continue the task. Custom stores must implement atomic
  authenticated creation and preserve the binding in every snapshot.
  Context-free runtime and subscription APIs are trusted embedding controls.
  Anonymous client isolation, proxy Tasks ownership, durable authorization,
  grant revalidation, and revocation remain unqualified.
- **OAuth/OIDC are unpromoted source surfaces:** their public building blocks
  remain available for development, but production security/profile
  conformance is unverified and no production-support claim is made for them.
- **CLI inspection is bounded diagnostics, not conformance evidence:** with
  default features in a current source checkout, `fastmcp inspect
  --protocol-policy` reports the selected `auto`, `modern-only`, or
  `legacy-only` era. A selected modern session requires valid
  `_meta.io.modelcontextprotocol/serverInfo` metadata and retains the open
  discovery capability shape (including `completions` and `extensions`)
  subject to output bounds and credential/control-text sanitization; a
  selected legacy session renders only the exact legacy capability shape. A
  `--no-default-features` build supports `modern-only` only. This does not
  qualify either protocol era as aggregate conformance or production
  readiness.
- **Subprocess cleanup is explicit and platform-bounded:**
  `Client::close(&mut self)` returns cleanup failures. The opt-in owned-group mode used by
  `fastmcp test` is Unix-only and fails before spawn elsewhere. Its
  `--protocol-policy` option binds both client negotiation and the launched
  FastMCP server profile. It uses a live
  anchor plus an owner-death channel, but cannot contain descendants that
  change group/session, withstand a competing global child reaper, or close a
  control descriptor copied by a host-side fork. Drop is best effort.

---

```bash
# Current published package; publication is not aggregate conformance evidence
cargo add fastmcp-rust@0.10.0

# Or use the git dependency for bleeding-edge changes
cargo add fastmcp-rust --git https://github.com/Dicklesworthstone/fastmcp_rust
```

---

## TL;DR

### The Problem

MCP server implementations need to solve several recurring problems:

- Handler schemas and JSON-RPC dispatch
- Cooperative cancellation and request budgets
- Ownership of concurrent child work
- Transport framing and session lifecycle

### The Solution

**FastMCP Rust** is an MCP framework with asupersync capability contexts, attribute macros, and explicit cancellation/budget surfaces:

```rust
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fastmcp_rust::{modern::ServerBuilder, prelude::*};

#[tool]
async fn greet(ctx: &McpContext, name: String) -> McpResult<String> {
    ctx.checkpoint()?;  // Cancellation point
    Ok(format!("Hello, {name}!"))
}

fn main() {
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("create I/O reactor"))
        .blocking_threads(0, 16)
        .build()
        .expect("create application runtime");
    runtime.block_on(async {
        let cx = Cx::current().expect("application context");
        ServerBuilder::new("my-server", "1.0.0")
            // Attribute macros generate PascalCase handler values.
            .tool(Greet)
            .build()
            .run_stdio_with_cx(&cx)
            .await
    });
}
```

The application creates the runtime once and supplies its context. Stdio needs
a blocking pool for its receive pump; FastMCP does not create a runtime at this
entry point. See Quick Start below for the dependency declarations.

### Why FastMCP Rust?

| Feature | FastMCP Rust | Manual Implementation |
|---------|--------------|----------------------|
| **Async handler API** | `#[tool] async fn` plus handler trait hooks | Manual Future boxing |
| **Cancellation** | Local request checkpoints; live wire interruption remains unverified | Application-specific checks |
| **Timeouts** | Request and handler budget surfaces | Application-specific timers |
| **Concurrent-future ownership** | Context combinators poll caller-owned futures | Manual ownership |
| **Error handling** | 4-valued Outcome | 2-valued Result |
| **Boilerplate** | Generated handler/schema implementations | Handwritten handler/schema implementations |

---

## AGENTS.md

This project includes an [`AGENTS.md`](AGENTS.md) file with guidelines for AI coding agents. Key points:

- **Porting methodology:** Extract spec from legacy → implement from spec → never translate line-by-line
- **Runtime:** Uses [asupersync](https://github.com/Dicklesworthstone/asupersync) exclusively; Tokio and Tokio-based adapters are unsupported
- **Unsafe code:** Forbidden (`#![forbid(unsafe_code)]`)
- **Toolchain:** Rust 2024 edition; pinned `nightly-2026-08-25` / rustc 1.100.0-nightly (`rust-version = "1.100"`)
- **MCP 2026-07-28 support is under implementation and remains unverified.**
- **Aggregate MCP 2026-07-28 support is not claimed by FND-01.**
- **The root compatibility `PROTOCOL_VERSION` is `2024-11-05`; the modern facade's `modern::PROTOCOL_VERSION` is `2026-07-28`. Neither is proof of negotiated 2026-07-28 support.**

---

## Quick Example

```rust
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fastmcp_rust::{modern::ServerBuilder, prelude::*};

// Define a tool with automatic JSON schema generation
#[tool(description = "Calculate the sum of two numbers")]
async fn add(ctx: &McpContext, a: i64, b: i64) -> McpResult<String> {
    ctx.checkpoint()?;  // Check the local cancellation token and budget
    Ok((a + b).to_string())
}

// Define an in-memory resource. Potentially blocking filesystem work is not
// performed inline on the dispatch worker.
#[resource(uri = "config://settings", description = "Application config")]
fn config(ctx: &McpContext) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(r#"{"theme":"dark"}"#.to_owned())
}

// Define a prompt template
#[prompt(description = "Generate a greeting message")]
async fn greeting(ctx: &McpContext, name: String) -> McpResult<Vec<PromptMessage>> {
    ctx.checkpoint()?;
    Ok(vec![PromptMessage {
        role: Role::User,
        content: Content::text(format!("Please greet {name} warmly.")),
    }])
}

fn main() {
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("create I/O reactor"))
        .blocking_threads(0, 16)
        .build()
        .expect("create application runtime");
    runtime.block_on(async {
        let cx = Cx::current().expect("application context");
        ServerBuilder::new("example-server", "1.0.0")
            .tool(Add)
            .resource(ConfigResource)
            .prompt(GreetingPrompt)
            .request_timeout(30)  // 30-second budget per request
            .build()
            .run_stdio_with_cx(&cx)
            .await
    });
}
```

Run it:

```bash
# echo_server is a shipped binary target of the facade crate, not an example
cargo run -p fastmcp-rust --bin echo_server
```

---

## Design Philosophy

### 1. Explicit Cooperative Cancellation

Handlers should check cancellation at natural suspension or iteration boundaries. FastMCP exposes cooperative checkpoints. These local context semantics do not, by themselves, make cancellation interruptible over a live connection; see the qualification boundaries above.

```rust
#[tool]
async fn process_items(
    ctx: &McpContext,
    items: Vec<String>,
) -> McpResult<Vec<Content>> {
    let mut results = vec![];
    for item in items {
        ctx.checkpoint()?;  // Allow graceful cancellation between items
        results.push(Content::text(process(item).await?));
    }
    Ok(results)
}
```

### 2. Budgets, Not Timeouts

Timeouts are "we gave up." Budgets are "you have X resources." The `Budget` type represents deadline, poll-quota, and cost-quota dimensions:

```rust
// Called from the application's runtime with its context.
async fn serve(cx: &Cx) -> ! {
    ServerBuilder::new("server", "1.0.0")
        .request_timeout(30) // 30-second server-owned request ceiling
        .tool(MyTool)
        .build()
        .run_stdio_with_cx(cx)
        .await
}

// Handler can check remaining budget
#[tool]
async fn my_tool(ctx: &McpContext) -> McpResult<String> {
    ctx.checkpoint()?;
    // ... work ...
    Ok("work completed".to_string())
}
```

### 3. Four-Valued Outcomes

`Result<T, E>` has no distinct cancellation or panic variants. FastMCP's asynchronous handler boundary uses `Outcome<T, E>`:

```rust
enum Outcome<T, E> {
    Ok(T),                    // Success
    Err(E),                   // Expected failure
    Cancelled(CancelReason),  // External interruption
    Panicked(PanicPayload),   // Internal failure
}
```

### 4. Capability-oriented handlers

Request authority flows through `McpContext`; application dependencies should likewise be passed explicitly instead of hidden in globals:

```rust
// BAD: Global state access
async fn bad_tool() {
    let db = GLOBAL_DB.lock().await;  // Hidden dependency
}

// GOOD: Explicit capability
async fn good_tool(ctx: &McpContext, db: &DbHandle) {
    db.query(ctx.cx(), "SELECT ...").await;  // Explicit
}
```

### 5. Owned Concurrent Futures

Concurrent child futures remain owned by the request handler and are polled together by context combinators:

```rust
use std::future::Future;
use std::pin::Pin;

#[tool]
async fn parallel_fetch(
    ctx: &McpContext,
    urls: Vec<String>,
) -> McpResult<Vec<Content>> {
    type FetchFuture = Pin<Box<dyn Future<Output = McpResult<String>> + Send>>;

    let futures: Vec<FetchFuture> = urls
        .into_iter()
        .map(|url| Box::pin(fetch(url)) as FetchFuture)
        .collect();

    let results = ctx.join_all(futures).await?;
    results
        .into_iter()
        .map(|result| result.map(Content::text))
        .collect()
}
```

---

## Design Positioning

These are FastMCP Rust design surfaces, not benchmark results or an MCP 2026-07-28 conformance certificate. Competing projects change independently and should be evaluated from their current documentation rather than a static comparison table.

| Area | FastMCP Rust design |
|------|---------------------|
| **Handler API** | `#[tool]`, `#[resource]`, and `#[prompt]` macros plus explicit handler traits |
| **Cancellation** | `McpContext` checkpoints and masks backed by asupersync |
| **Timeouts** | Request and handler budget surfaces |
| **Runtime** | asupersync only; Tokio adapters are unsupported |
| **Outcomes** | Four-valued `Outcome`: success, expected error, cancellation, or panic |
| **Unsafe code** | Forbidden in workspace crates with `#![forbid(unsafe_code)]` |

---

## Installation

### From crates.io (current 0.10.0 package)

The `0.10.0` package was published on 2026-09-13. Publication does
not establish aggregate MCP 2026-07-28 conformance, production readiness, or
qualification of every in-tree feature.

```toml
[dependencies]
fastmcp-rust = "0.10.0"
```

### As a Git Dependency

```toml
[dependencies]
fastmcp-rust = { git = "https://github.com/Dicklesworthstone/fastmcp_rust" }
asupersync = "=0.5.0"
```

### From Source

```bash
git clone https://github.com/Dicklesworthstone/fastmcp_rust.git
cd fastmcp_rust
cargo build --release
```

### CLI binaries (GitHub Releases)

The latest published release, v0.10.0, provides prebuilt `fastmcp` binaries on
GitHub Releases. The `fastmcp-cli` source package is also available from
crates.io for Cargo-based installation. Archives follow a `fastmcp-<os>-<arch>`
pattern (`.tar.xz` on Unix, `.zip` on Windows).

**This repository does not establish what the published archives are named.**
The checked-in release workflow is a quarantined verification surface and does
not publish new GitHub Releases: it has held `contents: read` since 2026-08-02,
and that permission cannot create a Release. The v0.10.0 tag postdates it
(2026-09-12), so this repository does not establish whether those archives are
downloadable either, and nothing in it can. That workflow's build matrix — the
`SURF-RELEASE-ASSET-CLOSURE` rows in
`evidence/fnd-01/dependency-verification.toml` — describes what the quarantined
workflow *would* produce, which is not evidence of what the release contains;
do not read it as the published filenames.

Take the exact filename from the v0.10.0 release page. The command below pins
the tag rather than `releases/latest` — under a workflow that does not publish,
`latest` is a moving pointer whose meaning is not stated anywhere.
**If the download does not resolve, build from source as shown above; that path
is verified by this repository and the download is not.**

```bash
# Replace <asset> with the exact filename listed on the v0.10.0 release page.
curl -fsSL -O https://github.com/Dicklesworthstone/fastmcp_rust/releases/download/v0.10.0/<asset>
tar -xJf <asset>
./fastmcp --version
```

### CLI via Cargo (optional)

```bash
rustup toolchain install nightly-2026-08-25
cargo +nightly-2026-08-25 install fastmcp-cli --version 0.10.0 --locked
```

### Client request deadlines (current source tree)

Ordinary client requests use separate idle and absolute response-wait
deadlines. Both begin after the request send commits. The idle deadline
defaults to 30 seconds; the non-resettable absolute deadline defaults to 120
seconds. Serialization, sending, and teardown are outside these timers.
Unix subprocess sends have a separate two-second bound covering writer-lock
acquisition and the native pipe commit. These commits remain synchronous;
non-Unix sends retain blocking-I/O limits. Only a valid matching progress
notification on a request that actually supplied a progress token can reset idle.

On Unix, eager `ClientBuilder::connect_stdio_with_cx` initialization also
yields between bounded receive turns, including partial responses and Auto
negotiation. A fallback child starts only after the disposable modern child
has been cleaned up. With `auto_initialize(true)`, async requests and
`Client::ensure_initialized_with_cx` also yield during the first handshake.
Dropping that future preserves the handshake, partial response and original
deadline; the next async or synchronous call resumes it without another send.
Explicit cancellation after the handshake starts fails the connection.
Synchronous constructors and synchronous first use retain their blocking
boundaries; subprocess creation and write commits remain synchronous.

```rust
use std::time::Duration;

use fastmcp_rust::prelude::{Client, ClientBuilder, Cx, McpResult, RequestTimeoutPolicy};

async fn connect(cx: &Cx) -> McpResult<Client> {
    let policy = RequestTimeoutPolicy::new(
        Duration::from_secs(20),
        Duration::from_secs(90),
    )?;
    ClientBuilder::new()
        .request_timeout_policy(policy)
        .connect_stdio_with_cx("my-mcp-server", &[], cx)
        .await
}
```

A live modern `subscriptions/listen` can stay open on the same stdio
`Client` while other requests complete. Call
`Client::open_subscriptions_listener` and then
`Client::next_subscription_event` to drain acknowledgement, catalog, and
resource-update events without collecting the stream to terminal.
On Unix, `open_subscriptions_listener_with_cx` and
`next_subscription_event_with_cx` yield during initialization and between
bounded receive turns. Tasks offer the corresponding
`open_final_task_subscription_listener_with_cx` and
`next_final_task_subscription_event_with_cx` APIs. Dropping an event future
preserves the stream and partial frame for resumption; explicit cancellation
retires only that listener, leaving sibling requests usable. Cancellation
write failures retain the listener and its error. Writes remain synchronous.
`listen_subscriptions_typed` remains the collect-to-terminal adapter.
The same incremental pattern exists on HTTP (`HttpClient::start_subscriptions_listener`),
modern WebSocket (`WebSocketClient::open_subscriptions_listener`), and
`ProxyClient::start_catalog_listener` for stdio and modern HTTP upstreams.
Proxy catalog and Tasks listener startup and event methods are async and
take the caller's `&Cx`. HTTP opening yields while awaiting headers;
cancelling or dropping that opening closes its socket and releases the
route for another attempt. Dropping an event future preserves the installed
listener for resumption. Stdio startup and bounded receive turns retain
their synchronous I/O limits.
`ProxyUpstreamBindingRegistry::connect_http_with_protocol_plan` and
`connect_stdio_with_protocol_plan` are async and take the caller's `&Cx`
first. HTTP negotiation and exact-2024 initialization await that runtime;
Unix stdio initialization uses the client's yielding receive path. Interrupted
openings leave the registry cache unchanged, allowing a fresh attempt. Keep
the caller runtime alive while using a cached legacy HTTP/SSE connection.
Use `ProxyClient::catalog_typed_with_cx` or `catalog_with_cx` to fetch HTTP or stdio
catalogs on that runtime, including every page and its modern cache hints.
These methods release the route lock while awaiting HTTP I/O or between bounded
stdio receive turns and reject cursor cycles without returning a partial
catalog. Stdio writes and individual receive turns retain their synchronous
I/O limits. Backends without an upstream binding retain their synchronous
catalog implementation.

Exact-2024 resource subscribe/unsubscribe hooks also have awaited variants:
`ResourceHandler::on_subscribe_async` and `on_unsubscribe_async`. The live
dispatcher and mounted resources forward these hooks on the caller runtime.
Native proxies release the route lock during HTTP/SSE waits and between bounded
stdio receive turns.
Failed or interrupted proxy requests preserve local URI rewrites, and failed
synchronous hooks roll back the session's subscription membership. Remote
effects already acknowledged upstream cannot be rolled back by these guarantees.

HTTP and WebSocket clients also expose typed `list_tools`/`call_tool`/
`read_resource`/`get_prompt` verbs so callers do not have to decode a raw
core result for ordinary catalog and invocation traffic. HTTP and WebSocket
`list_tools_with_cancellation`/`call_tool_with_cancellation`/
`read_resource_with_cancellation`/`get_prompt_with_cancellation` honor a
caller-owned cancellation domain for those ordinary verbs.
Exact MCP 2024-11-05 HTTP+SSE clients use `Client::sse_with_cx` when the GET
event stream and POST message endpoints are already known.

On Unix, `Client::call_tool_with_cx`, `read_resource_with_cx`, and
`get_prompt_with_cx` follow installed async reverse handlers when a modern
peer requests input. After initialization, all rounds and handler waits share
one absolute deadline; cancellation drops pending handler work and preserves
the connection. Deferred discovery keeps its separate initialization deadline
and connection-ending cancellation rules. Each
retry carries the original arguments and only the latest continuation state.
Without installed handlers, the input-required result is returned to the caller.

Installed modern MRTR callbacks also remain cancellable while pending in HTTP
and WebSocket clients, including callbacks that never wake themselves. Dropping
or interrupting a callback cancels its local token and prevents a continuation;
callback panics become redacted errors. These checks run between callback polls
and cannot preempt synchronous work or undo external effects inside user code.

With the `tasks` feature on Unix, `Client::call_tool_final_outcome_with_cx`
creates tasks without blocking the runtime while waiting for the peer. It
returns the typed complete, task, or input-required outcome. A returned task ID
can be used with `get_task_final_with_cx` and `cancel_task_final_with_cx` on the
same client. An already-admitted valid task result survives caller cancellation;
other results remain cancellation-first. Input-required results are returned
without automatic continuation. Pipe writes remain synchronous.

The published 0.10.0 CLI includes these flags. From a current source checkout,
run the CLI through the workspace to configure the two limits independently:

```bash
cargo run -p fastmcp-cli -- test --protocol-policy auto --idle-timeout 30 --absolute-timeout 120 my-mcp-server
```

The current `fastmcp test` subprocess runner is Unix-only because success
includes verified owned-process-group cleanup. Library callers should likewise
call `client.close()` and handle its `McpResult`; dropping a client is only a
best-effort safety net. The group anchor protects its numeric PGID while it is
live and closes an owner-death channel when the host exits, but this is not
portable process-tree containment or a substitute for Windows Job Objects.

**Requirements:**
- Rust nightly-2026-08-25 (see `rust-toolchain.toml`) for Edition 2024. The last FND-01 evidence snapshot still records `nightly-2026-07-11` until that harness is re-attested.

### Tasks commands (current source tree, optional)

Build the CLI with `--features tasks` to expose `tasks get`, `watch`, `update`,
and `cancel`. These commands require modern MCP and bilateral
`io.modelcontextprotocol/tasks` support. The pinned extension is experimental;
this source implementation remains provisional. The published 0.10.0 binary
does not include these commands.

```bash
cargo run -p fastmcp-cli --features tasks -- tasks get TASK_ID --http-url http://127.0.0.1:8000/mcp --json
cargo run -p fastmcp-cli --features tasks -- tasks watch TASK_ID --http-url http://127.0.0.1:8000/mcp --timeout 60 --max-events 100 --json
cargo run -p fastmcp-cli --features tasks -- tasks update TASK_ID --http-url http://127.0.0.1:8000/mcp --input-file responses.json --json
cargo run -p fastmcp-cli --features tasks -- tasks cancel TASK_ID --http-url http://127.0.0.1:8000/mcp --json
```

The input file contains the `inputResponses` map itself, for example
`{"roots":{"roots":[]}}` when the task's outstanding input key is `roots`
and its method is `roots/list`. Files are limited to 1 MiB. Updates use the
current task's input request types. Update and cancellation success mean
acknowledgement; use `get` or `watch` to observe subsequent task state.

Watch emits an initial snapshot and exits successfully if the task is already
completed, failed, or cancelled. Otherwise it subscribes to live updates and
reads the task again after subscription acknowledgement, so a terminal change
during subscription setup is also reported. It ends successfully on a terminal
update or after `--max-events` updates. Each
successful exit includes a `watch-ended` event; ending a watch does not cancel
the task. It does not reconnect or promise replay across disconnects.
`--timeout` defaults to 30 seconds, and timeout is a failed command. JSON mode
emits one document per event; human output bounds and sanitizes fields and
redacts recognizable credentials.

Use `--server EXECUTABLE` and repeat `--server-arg ARGUMENT` for stdio. Each
command starts a fresh process, so previously issued task IDs require a store
retained outside the prior process. For authenticated HTTPS, Tasks commands and
`inspect` accept `--bearer-token-file PATH`:

```bash
cargo run -p fastmcp-cli --features tasks,native-tls-roots -- tasks get TASK_ID --http-url https://mcp.example.com/mcp --bearer-token-file token.txt --json
```

The file must contain a UTF-8 token of at most 16 KiB, including an optional
trailing LF or CRLF. Do not include a `Bearer ` prefix or other whitespace or
control characters. The CLI reads a regular file once per invocation; callers
own token acquisition and refresh. Credentials require HTTPS and a modern or
auto protocol policy. Core-only builds omit Tasks from help and explain
the required feature when a Tasks command is attempted.

### Authenticated HTTP clients (current source tree)

The SDK accepts a caller-acquired bearer credential bound to one exact HTTPS
endpoint. For example, with a caller-owned `cx` and an existing `token`:

```rust
use fastmcp_rust::{BoundBearerCredential, CanonicalHttpUrl, modern};

let endpoint = CanonicalHttpUrl::parse("https://mcp.example.com/mcp")?;
let credential = BoundBearerCredential::bind(endpoint.clone(), token)?;
let client = modern::ClientBuilder::new()
    .http_bearer_credential(credential)
    .connect_http_with_cx(endpoint, cx)
    .await?;
```

The same credential binding covers discovery, later request/subscription
POSTs, and reverse-response POSTs. It never attaches to cleartext HTTP, a different path or query, or a
redirect target. An authenticated Auto client refuses legacy fallback.
Applications own token acquisition and refresh; create a new client when the
credential changes. The CLI's `--bearer-token-file` uses this same binding.

The bounded JSON/SSE readers withhold JSON-RPC errors that reflect the exact
credential into their message or data, returning a typed diagnostic instead.
Successful application results and the raw native-stream API remain unmodified.

HTTPS uses the transport's bundled WebPKI roots by default. Enable
`native-tls-roots` on `fastmcp-rust`, `fastmcp-client`, or `fastmcp-cli` for the platform trust
store. That profile also honors the native certificate provider's
`SSL_CERT_FILE` and `SSL_CERT_DIR` overrides; these select the trust source,
and do not disable certificate or hostname verification.

---

## Quick Start

### 1. Create a New Project

```bash
cargo new my-mcp-server
cd my-mcp-server
```

### 2. Add FastMCP

```toml
# Cargo.toml
[dependencies]
fastmcp-rust = { git = "https://github.com/Dicklesworthstone/fastmcp_rust" }
asupersync = "=0.5.0"
```

### 3. Write Your Server

```rust
// src/main.rs
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fastmcp_rust::{modern::ServerBuilder, prelude::*};

#[tool(description = "Echo the input message")]
async fn echo(ctx: &McpContext, message: String) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(message)
}

fn main() {
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("create I/O reactor"))
        .blocking_threads(0, 16)
        .build()
        .expect("create application runtime");
    runtime.block_on(async {
        let cx = Cx::current().expect("application context");
        ServerBuilder::new("echo-server", "1.0.0")
            .tool(Echo)
            .instructions("A simple echo server for testing")
            .build()
            .run_stdio_with_cx(&cx)
            .await
    });
}
```

### 4. Run

```bash
cargo run
```

### 5. Test with MCP Inspector

```bash
npx @modelcontextprotocol/inspector cargo run
```

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        MCP Client                               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ JSON-RPC over stdio
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      StdioTransport                             │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐         │
│  │   Codec     │───▶│   recv()    │───▶│   send()    │         │
│  │  (NDJSON)   │    │             │    │             │         │
│  └─────────────┘    └─────────────┘    └─────────────┘         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                         Server                                  │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐         │
│  │   Session   │    │   Router    │    │   Budget    │         │
│  │  (state)    │    │ (dispatch)  │    │ (timeout)   │         │
│  └─────────────┘    └─────────────┘    └─────────────┘         │
│                              │                                  │
│                              ▼                                  │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │                     McpContext                              ││
│  │  ┌─────┐  ┌──────────┐  ┌────────┐  ┌──────┐              ││
│  │  │ Cx  │  │checkpoint│  │ budget │  │masked│              ││
│  │  └─────┘  └──────────┘  └────────┘  └──────┘              ││
│  └─────────────────────────────────────────────────────────────┘│
│                              │                                  │
│                              ▼                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐         │
│  │ ToolHandler  │  │ResourceHandler│ │PromptHandler │         │
│  │  call_async  │  │  read_async  │  │  get_async   │         │
│  └──────────────┘  └──────────────┘  └──────────────┘         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                       asupersync                                │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐           │
│  │ Runtime │  │  Scope  │  │ Budget  │  │ Outcome │           │
│  └─────────┘  └─────────┘  └─────────┘  └─────────┘           │
└─────────────────────────────────────────────────────────────────┘
```

---

## Crate Structure

FastMCP is organized as a workspace with focused crates:

```
fastmcp_rust/
├── crates/
│   ├── fastmcp/           # Facade crate (published as fastmcp-rust)
│   ├── fastmcp-core/      # McpContext, errors, runtime helpers
│   ├── fastmcp-protocol/  # MCP types, JSON-RPC messages
│   ├── fastmcp-transport/ # Transport implementations (stdio, SSE, WebSocket, HTTP, memory)
│   ├── fastmcp-server/    # Server builder, router, handlers
│   ├── fastmcp-client/    # Client implementation
│   ├── fastmcp-macros/    # Proc-macro crate, published as fastmcp-derive
│   ├── fastmcp-console/   # Console rendering and statistics
│   └── fastmcp-cli/       # fastmcp command-line interface
```

| Crate | Purpose |
|-------|---------|
| `fastmcp-rust` | Convenience re-exports for simple `use fastmcp_rust::prelude::*` |
| `fastmcp-core` | `McpContext` wrapper, error types, `block_on` helper |
| `fastmcp-protocol` | MCP message types, capabilities, JSON-RPC framing |
| `fastmcp-transport` | Transport trait and stdio/SSE/WebSocket/HTTP/memory implementations |
| `fastmcp-server` | `Server`, `ServerBuilder`, routing, handler traits |
| `fastmcp-client` | Subprocess-stdio `Client`, plus public `ClientHttpConnection` and `HttpClient` support for modern HTTP and exact legacy SSE. With the experimental `websocket-experimental` facade profile, public async WebSocket clients accept owned native transports; Auto negotiation uses a caller-owned factory that supplies a fresh upgraded transport for its permitted retry |
| `fastmcp-derive` | Procedural macros for handler generation |

---

## Handler Traits

The signatures below are abridged; asynchronous trait methods return four-valued `McpOutcome` values, not ordinary `McpResult` values.

### ToolHandler

```rust
pub trait ToolHandler: Send + Sync {
    fn definition(&self) -> Tool;
    fn call(
        &self,
        ctx: &McpContext,
        arguments: serde_json::Value,
    ) -> McpResult<Vec<Content>>;

    // Override for true async (default delegates to call())
    fn call_async<'a>(&'a self, ctx: &'a McpContext, arguments: serde_json::Value)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = McpOutcome<Vec<Content>>> + Send + 'a>>;
}
```

### ResourceHandler

```rust
pub trait ResourceHandler: Send + Sync {
    fn definition(&self) -> Resource;
    fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>>;

    // Override for true async
    fn read_async<'a>(&'a self, ctx: &'a McpContext)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = McpOutcome<Vec<ResourceContent>>> + Send + 'a>>;
}
```

### PromptHandler

```rust
pub trait PromptHandler: Send + Sync {
    fn definition(&self) -> Prompt;
    fn get(&self, ctx: &McpContext, arguments: std::collections::HashMap<String, String>)
        -> McpResult<Vec<PromptMessage>>;

    // Override for true async
    fn get_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: std::collections::HashMap<String, String>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = McpOutcome<Vec<PromptMessage>>> + Send + 'a>>;
}
```

---

## Troubleshooting

| Problem | Cause | Fix |
|---------|-------|-----|
| JSON-RPC `InvalidParams` (`-32602`) `Unknown tool: <name>` for `tools/call` | Tool not registered | Register the generated handler, for example `.tool(MyTool)` |
| Request cancelled mid-operation | Local request cancellation or budget exhaustion | Add checkpoints and mask only the smallest atomic section that must finish; Unix stdio keeps receiving while bounded modern request children run, but output commits are serialized, non-Unix/custom/SSE/WebSocket loops retain blocking boundaries, and a non-cooperative handler can exceed the process-exit quiescence drain |
| JSON-RPC `RequestCancelled` (`-32004`) error `Request timeout exceeded` | The request outlived the server-owned deadline (`.request_timeout(...)`, 30 seconds by default), whether the server or the handler's own checkpoint observed it first | Add checkpoints so work stops promptly; increase `.request_timeout(...)` only for a deadline that is intentionally too short |
| `#[tool]` macro compilation error | Unsupported return conversion or argument schema | Prefer `String`, `Vec<Content>`, `McpResult<String>`, or `McpResult<Vec<Content>>` and ensure custom argument types implement `JsonSchema` |
| `TransportError::Io` on startup | stdin unavailable | Ensure nothing else reads stdin |

### Critical Section Example

```rust
use std::sync::atomic::{AtomicU64, Ordering};

// A handler that owns `committed` can call this helper after validation.
fn commit_revision(
    ctx: &McpContext,
    revision: u64,
    committed: &AtomicU64,
) -> McpResult<()> {
    // Mask only a small, non-blocking atomic commit. Masking does not make
    // synchronous filesystem or device I/O bounded or cancel-safe.
    ctx.masked(|| committed.store(revision, Ordering::Release))
        .map_err(|error| McpError::internal_error(error.to_string()))?;
    Ok(())
}
```

---

## Limitations

| Limitation | Details |
|------------|---------|
| **Pinned Nightly Required** | The project contract pins `nightly-2026-08-25`; do not substitute a different toolchain merely because it supports Edition 2024 |
| **Protocol Modernization** | The root compatibility `PROTOCOL_VERSION` remains `2024-11-05`; the modern facade's `modern::PROTOCOL_VERSION` is `2026-07-28`. MCP 2026-07-28 implementation and verification are incomplete |
| **Runtime-context migration** | Library client constructors and returning/custom transport runners require a caller-owned `Cx`. The process-owning CLI and `Server::run_stdio` install the ambient context at their top-level runtime boundary; `test-internals` is confined to test-only dependencies and the facade's opt-in `testing-lab` feature |
| **Network Transports** | The turnkey `run_http*` entry points provide a caller-owned dual-era HTTP listener and dispatch lifecycle. The experimental `websocket-experimental` facade profile also provides native async `bind_websocket` and `serve_websocket` listener lifecycles, plus caller-driven client connection. These surfaces do not establish aggregate conformance or complete lifecycle qualification |
| **Client Transport Coverage** | `fastmcp-client::Client` is subprocess-stdio only; public `ClientHttpConnection` and `HttpClient` provide modern HTTP and exact legacy SSE integration with typed `list_tools`/`call_tool`/`read_resource`/`get_prompt` verbs. Modern HTTP answers typed reverse `sampling/createMessage`, `roots/list`, and `elicitation/create` requests that arrive on a request-owned SSE body by POSTing the JSON-RPC response. Public `HttpClient::call_tool` and `WebSocketClient::call_tool` also follow modern server `input_required` by invoking those same installed handlers locally and retrying with `inputResponses`. Live `bind_http` JSON `tools/call` returns `ctx.final_sampling` and `ctx.final_roots` as `input_required`; a write-half EOF after the request is ordinary H1 completion and does not cancel that result. Public `modern::Client` stdio `call_tool_result` / `read_resource_result` / `get_prompt_result` keep the same live `input_required` branch. A Modern2026 stdio session stamps the same `_meta` protocol version and client capabilities on `start_multiplexed_request` that the typed verbs already send. Public stdio `read_resource` / `get_prompt` follow installed modern reverse handlers the same way `call_tool` does. Eligible stateless HTTP MRTR retries use framework-issued opaque, single-use `requestState` and may resume on a later POST only when the method, target, arguments, and admitted principal still match; forged or replayed state is rejected, and elicitation cannot resume over stateless HTTP because it requires a durable MCP transport connection. With `websocket-experimental`, the facade exposes `WebSocketClient` with incremental catalog listen, the same typed verbs, and the same modern reverse handlers: ModernOnly and LegacyOnly builders accept an owned async WebSocket transport, while Auto accepts a caller-owned factory that yields a fresh upgraded transport for initial modern discovery and its sole permitted exact-2024 retry |
| **Experimental WebSocket TLS** | The experimental async transport supports `ws://` and `wss://`. `wss://` can use the built-in WebPKI-rooted connector or a caller-supplied TLS connector for private roots, pinning, or client certificates; this connection support does not imply complete TLS, lifecycle, or MCP conformance qualification |
| **HTTP Dispatch Qualification** | Public `run_http*` binds and serves the caller-owned dual-era HTTP lifecycle. `ModernOnly` selects the exact MCP 2026-07-28 era and `LegacyOnly` selects the exact MCP 2024-11-05 era; MCP 2025-11-25 is not an adapter or supported policy. This executable surface does not establish aggregate MCP conformance or complete lifecycle qualification |
| **Wire Cancellation** | The modern-only split receive pump, including Unix stdio, admits bounded concurrent requests on the caller's dispatch runtime, including `subscriptions/listen`. It authenticates cancellation against the existing connection owner and serializes cancellation, response, and notification commits at the writer. Synchronous split runners use the bounded asupersync blocking bridge. Unsplit custom transports dispatch ordinary requests serially because receive and send share one handle; a `subscriptions/listen` runs off the receive loop, and output produced while `recv` blocks is queued and written as soon as `recv` returns. Transport reads and writes can still block. The dual-era Unix stdio pump retains its concurrent modern children and serialized exact MCP 2024-11-05 worker. A non-cooperative handler can exceed the bounded process-exit drain, and reliable `awaitCleanup` semantics remain unverified |
| **Silent stdio peers** | On Unix, the public subprocess `Client` enforces configured idle/absolute deadlines at child-pipe readiness and decode boundaries, including silent and partial-frame peers. Generic blocking `StdioTransport::recv` and non-Unix child-pipe I/O retain their documented frame/I/O-boundary limitation. Unix client writes use a separate two-second queue-and-commit deadline, so response-wait deadlines are not an end-to-end request or process wall-clock guarantee. Those residuals remain FND-04 work |
| **Stdio output backpressure** | On Unix, primary server responses and notifications use serialized nonblocking writes with a two-second commit deadline for ordinary pipes/sockets; a timeout, lock poison, partial write, notification encoding failure, or descriptor-flag restoration failure is connection-fatal. The writer attempts to restore descriptor flags before releasing the local lock; on restoration failure the descriptor may remain nonblocking, and inherited duplicate descriptors can observe the temporary `O_NONBLOCK` setting. Regular files/devices and non-Unix stdout retain blocking-I/O limits. A handler that ignores cancellation may force unsuccessful process exit after the bounded drain; shutdown hooks are skipped unless all worker and modern-child quiescence is proven |
| **Subprocess cleanup** | `Client::close(&mut self) -> McpResult<()>` is the proof-bearing path; Drop is best effort. `fastmcp test` uses Unix-only anchored process-group ownership; successful connections report explicit final cleanup separately, and initialization-cleanup failures remain visible. Descendants can escape via a new group/session, host forks can copy the control descriptor, and `SIGCHLD=SIG_IGN`, `SA_NOCLDWAIT`, or competing global reapers can invalidate reap evidence. Windows Job Object support is not implemented |
| **Development subprocess cleanup** | On Unix, each `fastmcp dev` build/server group contains a signal-immune watchdog tied to a private owner-held control pipe, so ordinary shutdown, child-handle drop, and CLI owner death trigger bounded TERM-then-KILL cleanup. A host-side fork that copies the owner descriptor or a descendant that changes group/session remains outside this boundary; non-Unix `dev` remains fail-closed |
| **Synchronous HTTP readers** | Low-level HTTP parsing checkpoints before/after reads and retries `EINTR`, but a generic synchronous `Read` already blocked in the kernel cannot be preempted. A bounded host must supply readiness-aware/asynchronous I/O. Public turnkey `run_http*` uses its caller-owned asynchronous listener lifecycle, whose broader qualification boundaries remain documented here |
| **Returning transport runners** | `run_transport_returning_with_cx` and the split returning variants return fatal receive/send/close errors and preserve simultaneous run-plus-close failures. Clean EOF/cancellation is `Ok(())`. Each exact-2024 handler dispatch now opens and closes a budgeted child region of the caller's runtime, requiring a runtime-backed `Cx`. Blocking transport I/O and complete cancellation/cleanup guarantees remain unresolved |
| **Request Cancellation Ownership** | Unix modern stdio request work runs in independently owned bounded child contexts, but process-exiting shutdown does not wait unboundedly for a non-cooperative child; cancellation therefore is not yet a complete quiescence or `awaitCleanup` guarantee |
| **Bidirectional Response Routing** | On Unix, stdio continuously routes inbound responses while exact-2024 lifecycle work or modern request children are active. The WebSocket server and custom split transports use the same routing, proven live for exact-2024 reverse sampling and roots over `bind_websocket`. Non-Unix stdio, unsplit custom transports, and SSE do not provide the same split routing. Public HTTP has separate dual-era routing, while end-to-end bidirectional lifecycle qualification remains open |
| **Response Cache Partitioning** | Eligible entries are partitioned by committed authentication facts and opaque session identity/revision; ambiguous admission and state mutation fail closed. This does not promote OAuth/OIDC or establish protocol conformance |
| **Authentication Admission** | Native HTTP requires `Authorization` for protected requests and rejects recognized body/meta and query credential fields before provider invocation. Other adapters retain a stripped legacy fallback. Complete authorization, lease/revocation, and OAuth challenge qualification remain open |
| **Tasks RPC** | `tasks/list` and `tasks/submit` stay `MethodNotFound`. Official `tasks/get`, `tasks/update`, and `tasks/cancel` run by default on a process-local in-memory store; `ServerBuilder::final_tasks` replaces that store |
| **HTTP as_proxy auto-follow** | A gateway HTTP `as_proxy` does not auto-follow an upstream server `input_required` task across POSTs; per-request dispatch is stateless and upstream request state cannot resume. Callers resume such upstream tasks through an explicit matching `tasks/update` |
| **OAuth/OIDC Promotion** | Public source APIs exist, but production security and profile conformance remain unverified; they are quarantined from production-support claims |
| **Early Development** | API may change before 1.0 |

---

## FAQ

**Q: Why is Tokio unsupported?**

A: FastMCP Rust is built around asupersync capability contexts, budgets, and cooperative-cancellation surfaces. Tokio and Tokio-based adapters are outside the supported runtime model.

**Q: Can I use this with Claude Desktop?**

A: Stdio integration exists, but compatibility must be checked against the client: the root compatibility `PROTOCOL_VERSION` is `2024-11-05`, the modern facade's `modern::PROTOCOL_VERSION` is `2026-07-28`, and MCP 2026-07-28 support is not yet verified.

**Q: How do I add authentication?**

A: Install an `AuthProvider` through the server builder. Native HTTP uses the
`Authorization` header and rejects recognized credentials in JSON-RPC params,
their `_meta`/`headers` containers, and query parameters with a fixed migration
diagnostic. Static tokens remain a non-OAuth deployment mode; applications own
TLS termination and token provisioning. Other transport adapters retain a
stripped legacy JSON-RPC credential fallback. OAuth/OIDC production security,
continuous authorization and profile conformance remain unverified.

**Q: What's the performance overhead of checkpoints?**

A: Checkpoints perform cancellation and budget checks. No project benchmark currently supports a universal per-call latency claim; measure them in the target workload if the cost matters.

**Q: Can I use other async runtimes?**

A: No. The current API and implementation require asupersync; other async runtimes are not supported.

**Q: How do I test my handlers?**

A: `#[tool]` keeps your function, so call it directly with an `McpContext`
built from a runtime's context and a request ID. (`Cx::for_testing()` needs
asupersync's `test-internals` feature; this form needs no extra features.)
```rust
use asupersync::runtime::RuntimeBuilder;
use fastmcp_rust::{Cx, McpContext, McpResult, tool};

#[tool]
fn my_tool(ctx: &McpContext, input: String) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(input)
}

#[test]
fn test_my_tool() {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("create test runtime");
    runtime.block_on(async {
        let ctx = McpContext::new(Cx::current().expect("test context"), 1);
        let result = my_tool(&ctx, "input".to_string());
        assert_eq!(result.unwrap(), "input");
    });
}
```

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

FastMCP Rust is distributed under the terms in [LICENSE](LICENSE): the MIT
License with the included OpenAI/Anthropic rider. Every workspace crate uses
that file as its Cargo `license-file`. [LICENSE-MIT](LICENSE-MIT) is retained
as a reference copy of the underlying MIT text; it is not an alternative
license for this project.

---

<p align="center">
  <sub>Built with <a href="https://github.com/Dicklesworthstone/asupersync">asupersync</a> for context-aware async</sub>
</p>
