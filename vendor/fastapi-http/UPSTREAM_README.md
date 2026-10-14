# fastapi_rust

<div align="center">
  <img src="fastapi_rust_illustration.webp" alt="fastapi_rust - High-performance Rust web framework with FastAPI-inspired ergonomics">
</div>

<div align="center">

**High-performance Rust web framework with FastAPI-inspired ergonomics**

*A Rust port inspired by [tiangolo/fastapi](https://github.com/tiangolo/fastapi) (Python), extended with [asupersync](https://github.com/Dicklesworthstone/asupersync) for structured concurrency, zero-copy parsing, and deterministic testing.*

[![License: MIT](https://img.shields.io/badge/License-MIT%2BOpenAI%2FAnthropic%20Rider-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.95+-orange.svg)](https://www.rust-lang.org/)
[![Status](https://img.shields.io/badge/status-early%20development-yellow.svg)]()

*Type-safe routing | Zero-copy parsing | Structured concurrency | OpenAPI generation*

</div>

<div align="center">
<h3>Add to your project</h3>

```toml
# Cargo.toml
[dependencies]
fastapi-rust = { git = "https://github.com/Dicklesworthstone/fastapi_rust", branch = "main" }
asupersync = { version = "0.5", default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

(The crates.io package is `fastapi-rust`; the Rust crate name is `fastapi_rust`.)

These examples target current `main`, which uses asupersync 0.5. The published
`fastapi-rust` 0.4.4 release instead uses the asupersync 0.4 line (currently
0.4.11); use that runtime line when choosing the registry release.

<p><em>Requires Rust 1.95+ (2024 edition). Co-developed with <a href="https://github.com/Dicklesworthstone/asupersync">asupersync</a>.</em></p>
</div>

---

## TL;DR

**The Problem**: Rust web frameworks either sacrifice developer ergonomics for performance (raw `hyper`) or hide allocations behind layers of abstraction (Axum + Tower). None leverage structured concurrency for cancel-correct request handling, and most require a massive dependency tree.

**The Solution**: fastapi_rust brings FastAPI's intuitive, type-driven API design to Rust with zero-copy HTTP parsing, compile-time route validation, and first-class integration with [asupersync](https://github.com/Dicklesworthstone/asupersync) for structured concurrency and deterministic testing.

### Why fastapi_rust?

| Feature | What It Does |
|---------|--------------|
| **Zero-copy HTTP parsing** | Requests parsed directly from buffers; no allocations on fast paths |
| **Compile-time handler validation** | Macros check handler signatures and extractors; route conflicts and wildcard placement are checked during app construction |
| **Structured concurrency** | Concurrent connection tasks use regions; handlers receive cooperative cancellation contexts |
| **Type-driven extractors** | Declare parameter types; framework extracts and validates automatically |
| **Dependency discipline** | No Tokio/Hyper/Tower/Axum; direct deps kept small with a bias toward removal |
| **Deterministic testing** | Seeded request contexts and asupersync lab integration |
| **FastAPI-compatible errors** | Validation errors use FastAPI's `detail` array shape |

---

## Quick Example

```rust
use fastapi_rust::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema)]
struct Item {
    id: i64,
    name: String,
    price: f64,
}

#[derive(Deserialize, JsonSchema)]
struct SearchParams {
    q: String,
    limit: Option<usize>,
}

#[get("/items/{id}")]
async fn get_item(ctx: &RequestContext, id: Path<i64>) -> Result<Json<Item>, HttpError> {
    ctx.checkpoint()?;  // Cancellation/budget check (cancelled -> 499)

    Ok(Json(Item {
        id: id.0,
        name: "Widget".into(),
        price: 29.99,
    }))
}

#[post("/items")]
async fn create_item(_cx: &Cx, item: Json<Item>) -> Result<Json<Item>, HttpError> {
    // Automatic JSON deserialization; application validation below
    // Wrong Content-Type -> 415
    // Parse error -> 422 with detailed location
    // Payload too large -> 413
    if item.0.price < 0.0 {
        return Err(HttpError::bad_request().with_detail("price must be non-negative"));
    }
    Ok(item)
}

#[get("/search")]
async fn search(
    _cx: &Cx,
    q: Query<SearchParams>,          // ?q=...&limit=...
    _auth: Option<BearerToken>,       // Optional bearer credentials
) -> Json<Vec<Item>> {
    let items = vec![Item { id: 1, name: q.0.q, price: 29.99 }];
    Json(items.into_iter().take(q.0.limit.unwrap_or(10)).collect())
}

fn main() {
    let app = App::builder()
        .title("My API")
        .version("1.0.0")
        .openapi(fastapi_rust::OpenApiConfig::new())
        .route_entry(get_item_route())
        .route_entry(create_item_route())
        .route_entry(search_route())
        .middleware(RequestIdMiddleware::new())
        .middleware(Cors::new().allow_any_origin())
        .build();

    // Run with asupersync
    let rt = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime must build");
    rt.block_on(async move {
        serve(app, "0.0.0.0:8000")
            .await
            .expect("server must start");
    });
}
```

Each route macro generates `<handler>_route()` for runtime registration.
Register the runtime entry with
`.route_entry(...)`; it runs extractors, calls the handler, and converts its
result into a response. Invalid request values produce an HTTP error; unsupported
extractor types fail to compile.

With `.openapi(OpenApiConfig::new())`, `/openapi.json` includes the registered
JSON model schemas, inferred JSON success responses, and typed query/header
parameters. JSON body, JSON response, and query model types used in route macros
must implement `JsonSchema`; named headers use a schema-capable value type.

---

## Design Philosophy

### 1. Extract Spec, Never Translate

We study FastAPI's behavior and ergonomics, then implement idiomatically in Rust. No line-by-line Python translation - Rust has better tools for these problems:

- Python decorators -> Rust procedural macros
- Pydantic validation -> Compile-time type checking + serde
- ASGI lifecycle -> Structured concurrency regions
- Runtime reflection -> Compile-time code generation

### 2. Zero-Cost Abstractions

| Technique | Implementation |
|-----------|----------------|
| No runtime reflection | Proc macros analyze types at compile time |
| Typed handler signatures | Compile-time extractor and response checks; runtime route entries use boxed futures |
| Pre-allocated buffers | 4KB default, configurable per-route |
| Zero-copy HTTP parsing | Borrowed types reference request buffer |
| Inline critical paths | `#[inline(always)]` on hot code |

### 3. Cancel-Correct by Default

The TCP server uses asupersync contexts and cooperative cancellation. Concurrent
serving methods scope connection work to regions; handlers inherit the caller's
capability context:

```
Connection Accepted
    |
    v
Caller Context -> Request Context
    |
    v
Middleware -> Extractors -> Handler
    |
    v
Response Sent
    |
    v
Background Tasks
```

Use `TcpServer` with the caller's `Cx` for explicit runtime control. The convenience
`serve` function wires application startup, request handling, and shutdown.
Cancellation remains cooperative: handlers should check `ctx.checkpoint()` around
long-running work. Background tasks run after the response is sent.

### 4. Dependency Discipline

| Crate | Purpose | Why |
|-------|---------|-----|
| `asupersync` | Async runtime | Our own - cancel-correct, capability-secure |
| `serde` | Serialization traits | Zero-cost, industry standard |
| `serde_json` | JSON parsing | Fast, well-optimized |

**Explicitly avoided:** Tokio, Hyper, Axum, Tower, and runtime-reflection/schema-building crates.

Note: Some workspace crates currently use additional small utility/test/proc-macro dependencies
where it meaningfully improves safety or developer experience. Shrinking this further is an active
goal; the dependency inventory is recorded in `Cargo.lock` and `UPGRADE_LOG.md`.

---

## How fastapi_rust Compares

| Feature | fastapi_rust | Axum | Actix-web | Rocket |
|---------|--------------|------|-----------|--------|
| Zero-copy HTTP parsing | **Custom** | Hyper | Partial | No |
| Compile-time routes | **Proc macros** | Runtime | Runtime | Macros |
| Structured concurrency | **asupersync** | Tokio spawn | Actix-rt | Tokio |
| Cancel-correct shutdown | **Native** | Manual | Manual | Manual |
| Dependency injection | **Native + cache** | State only | Data only | Managed |
| OpenAPI generation | **Derived schemas; registration-time assembly** | External | External | External |
| Deterministic testing | **Lab runtime** | No | No | No |
| Runtime | **asupersync** | Tokio | Actix-rt | Tokio |
| FastAPI-style errors | **Yes (422 format)** | No | No | No |

### When to Use fastapi_rust

- You need cancel-correct request handling (graceful shutdown, timeouts)
- You want compile-time route validation
- You're building with asupersync for structured concurrency
- You want deterministic tests for concurrent code
- You're familiar with FastAPI and want similar ergonomics in Rust

### When to Consider Alternatives

- You need production-proven stability today (fastapi_rust is v0.4.4)
- You require production-hardened WebSocket support (implementation exists; broader parity remains under `bd-uz2s`)
- You have existing Tokio-based infrastructure
- You need the massive ecosystem of Tower middleware

---

## Installation

### Add to Cargo.toml

```toml
[dependencies]
fastapi-rust = { git = "https://github.com/Dicklesworthstone/fastapi_rust", branch = "main" }
asupersync = { version = "0.5", default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

**Note**: The crates.io package is `fastapi-rust`, and the crate name is `fastapi_rust`.
This dependency set follows current `main`. For published `fastapi-rust = "0.4.4"`,
use `asupersync = { version = "0.4.11", default-features = false }` instead.

### From Source

```bash
git clone https://github.com/Dicklesworthstone/fastapi_rust.git
cd fastapi_rust
cargo build --release
```

### Optional: project bootstrapper (`install.sh`)

fastapi_rust is a library, so there is nothing to "install" beyond the Cargo
dependency above — most users should stop there. For a brand-new machine or a
brand-new project, `install.sh` bundles the first-run chores:

- checks that `rustc` meets the 1.95 MSRV (offers `rustup update` / a rustup
  install if missing; `--easy-mode` does it non-interactively),
- resolves the latest `fastapi-rust` on crates.io,
- `--new NAME` scaffolds a project with a working server, tests, and the
  stable-safe dependency set, and
- installs a `fastapi-rust` skill for Claude Code / Codex / Gemini / Cursor if
  those agents are present (`--no-skill` to skip).

```bash
# inspect flags
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/fastapi_rust/main/install.sh?$(date +%s)" | bash -s -- --help
# scaffold ./my_api and build it
curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/fastapi_rust/main/install.sh?$(date +%s)" | bash -s -- --new my_api --build
```

It never touches your shell rc files or `PATH`; undo by deleting the project
directory and `~/.{claude,codex,gemini,cursor}/skills/fastapi-rust`.

### Requirements

- **Rust 1.95+** (2024 edition)
- **[asupersync](https://github.com/Dicklesworthstone/asupersync)** (co-developed runtime)

---

## Architecture

```
+-------------------------------------------------------------------+
|                       fastapi_rust (facade)                        |
|   Re-exports all public types, prelude module                      |
+-------------------------------------------------------------------+
        |           |           |           |           |
        v           v           v           v           v
+-----------+ +-----------+ +-----------+ +-----------+ +-----------+
|   core    | |   http    | |  router   | |  macros   | |  openapi  |
|           | |           | |           | |           | |           |
| - Request | | - Parser  | | - Trie    | | - #[get]  | | - Schema  |
| - Response| | - Body    | | - Match   | | - #[post] | | - Builder |
| - Context | | - Query   | | - Registry| | - Derive  | | - Spec    |
| - Extract | | - Headers | |           | |           | |           |
| - Depends | | - Writer  | |           | |           | |           |
| - Error   | | - Server  | |           | |           | |           |
| - Middle. | | - Stream  | |           | |           | |           |
| - Logging | |           | |           | |           | |           |
| - Testing | |           | |           | |           | |           |
| - Shutdown| |           | |           | |           | |           |
+-----------+ +-----------+ +-----------+ +-----------+ +-----------+
        |
        v
+-------------------------------------------------------------------+
|                         asupersync                                 |
|   Structured concurrency - Cx - Regions - Budgets - Lab           |
+-------------------------------------------------------------------+
```

### Crate Overview

| Crate | Purpose |
|-------|---------|
| `fastapi-rust` | Facade: re-exports, prelude |
| `fastapi-core` | Request, Response, extractors, DI, middleware, testing, shutdown |
| `fastapi-http` | HTTP/1.1 parser, TCP server, body handling, HTTP/2, WebSockets, streaming |
| `fastapi-router` | Radix trie routing, path matching, conflict detection |
| `fastapi-macros` | Route macros, validation derive, JSON Schema derive |
| `fastapi-openapi` | OpenAPI 3.1 types, schema builder, spec generation |
| `fastapi-types` | Shared HTTP Method enum |
| `fastapi-output` | Optional agent-aware terminal output |

---

## Extractors

Extract typed data from requests declaratively:

```rust
use fastapi_rust::prelude::*;
use fastapi_rust::extractors::{Accept, Authorization, NamedHeader};

#[derive(Deserialize, JsonSchema)]
struct SearchParams {
    q: String,
}

#[get("/users/{id}")]
async fn get_user(
    _cx: &Cx,                          // Capability context
    id: Path<i64>,                     // Path parameter: /users/123
    q: Query<SearchParams>,            // Query string: ?q=...
    _auth: NamedHeader<String, Authorization>, // Required header
    _accept: Option<NamedHeader<String, Accept>>, // Optional header
) -> Json<String> {
    Json(format!("User {}: {}", id.0, q.0.q))
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct CreateItem {
    name: String,
}

#[post("/items")]
async fn create(
    _cx: &Cx,
    item: Json<CreateItem>,            // JSON body
) -> Json<CreateItem> {
    // 415 if wrong Content-Type
    // 413 if payload too large (configurable)
    // 422 if parse error (with location path)
    item
}
```

### Available Extractors

| Extractor | Description | Error Response |
|-----------|-------------|----------------|
| `Path<T>` | URL path parameters | 422 if missing/wrong type |
| `Query<T>` | Query string | 422 if missing/invalid |
| `Json<T>` | JSON request body | 415/413/422 |
| `NamedHeader<T, N>` | Header named by a `HeaderName` marker | 422 if missing/invalid |
| `State<T>` | Application state | 500 if not configured |
| `Depends<T>` | Dependency injection | Depends on factory |
| `Option<T>` | Any extractor, optional | Converts extraction errors to `None`, including malformed values |

---

## Middleware

Composable middleware with onion model execution:

```rust
use fastapi_rust::prelude::*;
use fastapi_rust::core::{
    AddResponseHeader, BoxFuture, ControlFlow, Middleware, RequestResponseLogger,
    RequireHeader,
};

// Built-in middleware
let app = App::builder()
    .middleware(RequestIdMiddleware::new())      // Add X-Request-Id
    .middleware(RequestResponseLogger::default()) // Log all requests
    .middleware(Cors::new().allow_any_origin())   // CORS handling
    .middleware(RequireHeader::new("X-API-Key")) // Require header
    .middleware(AddResponseHeader::new("X-Powered-By", b"fastapi_rust"))
    .build();

// Custom middleware
struct ExampleHeader;

impl Middleware for ExampleHeader {
    fn before<'a>(
        &'a self, _ctx: &'a RequestContext, _req: &'a mut Request,
    ) -> BoxFuture<'a, ControlFlow> {
        Box::pin(async { ControlFlow::Continue })
    }

    fn after<'a>(
        &'a self, _ctx: &'a RequestContext, _req: &'a Request, resp: Response,
    ) -> BoxFuture<'a, Response> {
        Box::pin(async move { resp.header("x-example", b"enabled".to_vec()) })
    }
}
```

### Execution Order

```
Request -> MW1.before -> MW2.before -> MW3.before -> Handler
                                                        |
Response <- MW1.after <- MW2.after <- MW3.after <- Response
```

First registered runs first on the way in, last on the way out (onion model).

---

## Dependency Injection

Request-scoped dependencies with caching:

```rust
use fastapi_rust::prelude::*;

// A dependency resolved from application state
#[derive(Clone)]
struct DatabasePool {
    label: String,
}

impl FromDependency for DatabasePool {
    type Error = HttpError;

    async fn from_dependency(ctx: &RequestContext, req: &mut Request) -> Result<Self, HttpError> {
        ctx.checkpoint()?;
        let state = State::<DatabasePool>::from_request(ctx, req).await
            .map_err(|_| HttpError::internal().with_detail("DatabasePool state missing"))?;
        Ok(state.0)
    }
}

// Use in handler
#[get("/users/{id}")]
async fn get_user(
    _cx: &Cx,
    id: Path<i64>,
    db: Depends<DatabasePool>,  // Automatically resolved and cached
) -> Json<String> {
    Json(format!("User {} from {}", id.0, db.label))
}

// Override for testing
let app = App::builder()
    .state(DatabasePool { label: "primary".into() })
    .route_entry(get_user_route())
    .build();
app.override_dependency_value(DatabasePool { label: "test".into() });
```

### Dependency Scopes

| Scope | Behavior |
|-------|----------|
| `Request` (default) | Resolve once, cache for request lifetime |
| `Function` | Resolve on every extraction |
| `Depends<T, NoCache>` | Explicit opt-out of caching |

---

## Testing

In-process testing without network I/O. These tests use the `Item` and route
functions from the quick example above:

```rust
use fastapi_rust::prelude::*;
use fastapi_rust::testing::TestClient;

#[test]
fn test_get_item() {
    let app = App::builder().route_entry(get_item_route()).build();
    let client = TestClient::new(app);

    let resp = client.get("/items/42")
        .header("Authorization", "Bearer token")
        .send();

    assert_eq!(resp.status().as_u16(), 200);

    let item: Item = resp.json().expect("valid item JSON");
    assert_eq!(item.id, 42);
}

#[test]
fn test_deterministic() {
    let app = App::builder().route_entry(create_item_route()).build();
    // Seeded in-process request context
    let client = TestClient::with_seed(app, 12345);

    let resp = client.post("/items")
        .json(&Item { id: 1, name: "Widget".into(), price: 29.99 })
        .send();

    assert_eq!(resp.status().as_u16(), 200);
}
```

### Assertion Helpers

```rust
use fastapi_rust::core::{assert_status, assert_header, assert_json};

assert_status!(resp, 200);
assert_header!(resp, "Content-Type", "application/json");
assert_json!(resp, {"id": 42, "name": "Widget", "price": 29.99});
```

---

## Error Handling

FastAPI-compatible validation errors:

```rust
use fastapi_rust::prelude::*;
use fastapi_rust::core::error::loc;

#[get("/items/{id}")]
async fn get_item(_cx: &Cx, id: Path<i64>) -> Result<Json<i64>, ValidationErrors> {
    if id.0 < 0 {
        return Err(ValidationErrors::single(
            ValidationError::value_error(loc::path("id"), "ID must be non-negative")
                .with_input(serde_json::json!(id.0)),
        ));
    }
    Ok(Json(id.0))
}
```

**Error response format (FastAPI-compatible):**

```json
{
  "detail": [
    {
      "type": "value_error",
      "loc": ["path", "id"],
      "msg": "ID must be non-negative",
      "input": -1
    }
  ]
}
```

### Built-in Error Types

| Status | Constructor | Use Case |
|--------|-------------|----------|
| 400 | `HttpError::bad_request()` | Malformed request |
| 401 | `HttpError::unauthorized()` | Missing/invalid auth |
| 403 | `HttpError::forbidden()` | Permission denied |
| 404 | `HttpError::not_found()` | Resource not found |
| 413 | `HttpError::payload_too_large()` | Body exceeds limit |
| 415 | `HttpError::unsupported_media_type()` | Wrong Content-Type |
| 422 | `ValidationErrors::single(error)` | Validation failed |
| 500 | `HttpError::internal()` | Server error |

---

## Graceful Shutdown

The TCP server exposes a shutdown controller and a configurable drain timeout:

```rust
use fastapi_rust::{ServerConfig, TcpServer};

let server = TcpServer::new(
    ServerConfig::new("0.0.0.0:8000").with_drain_timeout_secs(30),
);
let shutdown = server.shutdown_controller().clone();
// Keep this handle in application state or a signal-handling task.
// Calling shutdown initiates the server's shutdown sequence.
shutdown.shutdown();
```

Use `serve_with_shutdown` or a concurrent serving method to stop accepting
connections and drain active work. Application shutdown hooks use
`.on_shutdown(|| { ... })` or `.on_shutdown_async(|| async { ... })`.

---

## Configuration

```rust
use fastapi_rust::prelude::*;

let app = App::builder()
    // Metadata
    .title("My API")
    .version("1.0.0")
    .description("A sample API built with fastapi_rust")

    // Routes
    .route_entry(get_item_route())
    .route_entry(create_item_route())

    // Middleware (order matters)
    .middleware(RequestIdMiddleware::new())
    .middleware(Cors::new()
        .allow_origin("https://example.com")
        .allow_methods([Method::Get, Method::Post])
        .allow_headers(["Authorization"])
        .max_age(3600))

    // Shared state
    .state(DatabasePool { label: "primary".into() })

    // Exception handlers
    .exception_handler(|_ctx, err: std::io::Error| {
        HttpError::internal().with_detail(err.to_string()).into_response()
    })

    // Lifecycle hooks
    .on_startup(|| {
        println!("Starting up...");
        Ok(())
    })
    .on_shutdown(|| {
        println!("Shutting down...");
    })

    // Build
    .build();
```

---

## Troubleshooting

### Common Issues

| Problem | Cause | Solution |
|---------|-------|----------|
| `asupersync not found` | Missing dependency | Add `asupersync` to Cargo.toml |
| Handler needs a request context | Context must come from the caller | Accept `&Cx` or `&RequestContext`; pass the caller's `Cx` to `TcpServer::serve_app` |
| Route conflicts | Overlapping path patterns | Check for `{param}` vs literal conflicts |
| JSON handler does not compile | Model lacks `Deserialize`, `Serialize`, or `JsonSchema` | Derive the traits needed by the JSON extractor, response, and route documentation |
| Middleware not running | Wrong registration order | Check middleware ordering |

### Debugging Tips

```rust
use fastapi_rust::core::RequestResponseLogger;
use fastapi_rust::testing::TestClient;

// Enable request logging
let app = App::builder()
    .route_entry(get_item_route())
    .middleware(RequestResponseLogger::new()
        .log_body(true)
        .log_request_headers(true))
    .build();

// Check route registration
app.routes().for_each(|(method, path)| println!("{} {}", method.as_str(), path));

// Seeded in-process request context
let client = TestClient::with_seed(app, 12345);
```

---

## Parity Status (Goal: 100% FastAPI Coverage)

This project is intended to reach **100% feature-for-feature, behavior-for-behavior parity** with
the legacy Python FastAPI library.

Current parity status and the concrete gap list are tracked in:
- `PROPOSED_RUST_ARCHITECTURE.md` (Section 0: Parity Matrix)
- Beads (`br ready`, `br show <id>`) with coverage/gap audit epic `bd-uz2s`

### Current Coverage and Limits

- **Route macros**: runtime entries execute extractors and handlers; consumer tests cover JSON,
  path parameters, cancellation checkpoints, and error responses.
- **OpenAPI generation**: macro runtime entries register named JSON models, inline primitive,
  list/map/nullable schemas, infer `Json<T>` and `Result<Json<T>, E>` success responses,
  and retain declared response statuses/descriptions. Named query fields and `NamedHeader`
  parameters include their types and requiredness; symmetric serde renames and defaults
  feed query metadata. Converter paths become valid OpenAPI templates for both macro
  and manually registered routes.
  Unconverted `Path<T>` parameters use scalar handler types, inline tuple order,
  or the serialized field names of named `JsonSchema` models. Path parameters remain
  required, including optional extractors. One grouped path extractor is required;
  separate scalar path arguments are rejected instead of reading the first value twice.
  Explicit numeric/UUID converter schemas remain authoritative, so narrower handler
  constraints on those converters are not fully described. Tuple aliases,
  recursive/custom schema references, directional serde attributes, arbitrary extractor
  metadata remain outside this coverage.
  `BearerToken`, `BasicAuth`, and `OAuth2PasswordBearer` register security schemes
  and operation requirements through their extractor traits, including type aliases.
  Default OAuth2 documentation uses `/token` and empty scopes. Required extractors
  form a conjunction; optional-only authentication includes an anonymous alternative.
  Manual route requirements retain their alternatives and scopes, with explicit
  definitions registered through `RouteEntry::security_scheme`. Conflicting or
  missing definitions are rejected during document construction. These declarations
  do not validate tokens/passwords, enforce OAuth scopes, or create token endpoints;
  custom configuration is explicit metadata.
- **HTTPS redirects**: use the server-admitted effective authority, preserve encoded
  origin targets and queries, and support bracketed IPv6 and configured HTTPS ports.
  Malformed authority/target inputs return 400 without a redirect. Proxy scheme-header
  trust, other request-target forms, and TLS remain separate configuration/protocol limits.
- **TCP server**: HTTP/1.1 keep-alive, request deadlines, body streaming, and protocol upgrades
  have integration coverage. Production hardening remains an ongoing goal.
- **WebSockets**: handshake, frames, ping/pong, and close handling have integration tests.
  This does not establish every FastAPI/Starlette behavior.
- **Multipart/form-data + file uploads**: parser + `MultipartForm` extractor + incremental streamed-body parsing +
  streamed-part incremental flushing + spool-backed file parts + `UploadFile` async API (`read`/`write`/`seek`/`close`) are implemented.
  Fully streamed consumption and broader edge-case equivalence still require comparison to the spec.
- **HTTP/2**: H2C prior knowledge, HPACK, SETTINGS, flow control, and error frames exist with
  integration coverage. Concurrent stream multiplexing and a full stream-state machine remain gaps.

The parity matrix records implementation coverage, not proof of complete Python parity.
Closed historical beads are implementation history; their closure alone does not establish
full subsystem equivalence.

### Non-Negotiables / Constraints

- **Requires asupersync**: no Tokio support (by design).
- **Rust 1.95+**: edition 2024; repository checks use the pinned toolchain in `rust-toolchain.toml`.
- **Early development**: API will change before v1.0.

---

## FAQ

### Why "fastapi_rust"?

It's a Rust web framework inspired by Python's [FastAPI](https://fastapi.tiangolo.com/), preserving the type-driven API design while achieving native performance and cancel-correctness.

### Why not use Tokio/Axum?

Tokio's spawn model makes cancel-correctness difficult - tasks can outlive their scope, leading to resource leaks and subtle bugs. asupersync's structured concurrency ensures all request-related work completes or cancels together.

### Can I use this in production?

Not production-ready yet. This is v0.4.4 in active development; the TCP server exists (built on `asupersync::net`),
but parity and production hardening are tracked under `bd-uz2s`.

### How fast is it?

We haven't published end-to-end benchmarks yet, but the architecture is designed for:
- Zero allocations on the fast path
- Zero-copy request parsing
- No runtime reflection
- Pre-allocated buffers (4KB default)

### Does it support async/await?

Yes, fully. All handlers, middleware, and extractors are async-native, built on asupersync's structured concurrency model.

### Why the minimal dependency approach?

Each dependency is a maintenance burden, security surface, and compile-time cost. By keeping dependencies small and intentional, we:
- Reduce build times significantly
- Have full control over behavior
- Avoid dependency conflicts
- Make auditing practical

### How do validation errors compare to FastAPI?

Validation errors use FastAPI's `detail` array with `type`, `loc`, and `msg` fields.
Exact messages and rule coverage still need comparison against the specification:

```json
{
  "detail": [
    {"type": "missing", "loc": ["body", "email"], "msg": "Field required"}
  ]
}
```

---

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

---

## License

MIT License (with OpenAI/Anthropic Rider). See [LICENSE](LICENSE).

---

## Related Projects

| Project | Description |
|---------|-------------|
| [asupersync](https://github.com/Dicklesworthstone/asupersync) | Structured concurrency async runtime (co-developed) |
| [FastAPI](https://fastapi.tiangolo.com/) | The Python framework that inspired this project |
