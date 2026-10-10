# Pinned input, transport and caller-address patches

These four crates carry narrowly scoped input and transport fixes required by the
announcement upload path. They are copied from the same Git revisions used by
the rest of the workspace; this does not upgrade the Franken stack.

| Crate | Upstream source |
| --- | --- |
| `fastapi-http` | [`fastapi_rust@cb9d72926434e2ec3e08cced5eae7f7c13168c7d`](https://github.com/Dicklesworthstone/fastapi_rust/tree/cb9d72926434e2ec3e08cced5eae7f7c13168c7d/crates/fastapi-http) |
| `fastmcp-server` | [`fastmcp_rust@03b5274544048babf6b358874310038a9ee4f76a`](https://github.com/Dicklesworthstone/fastmcp_rust/tree/03b5274544048babf6b358874310038a9ee4f76a/crates/fastmcp-server) |
| `fastmcp-transport` | [`fastmcp_rust@03b5274544048babf6b358874310038a9ee4f76a`](https://github.com/Dicklesworthstone/fastmcp_rust/tree/03b5274544048babf6b358874310038a9ee4f76a/crates/fastmcp-transport) |
| `fastmcp-protocol` | [`fastmcp_rust@03b5274544048babf6b358874310038a9ee4f76a`](https://github.com/Dicklesworthstone/fastmcp_rust/tree/03b5274544048babf6b358874310038a9ee4f76a/crates/fastmcp-protocol) |

## Why the copies are needed

The FastAPI HTTP/1 application listener constructs a `StatefulParser` with the
configured request-size limit, but leaves its separate `BodyConfig` at the
default one MiB. Its private connection handler exposes no way for FrankenSonos
to override that body configuration. The local patch connects it to
`app.config().max_body_size`. The request and header limits still apply.

The FastMCP native HTTP listeners configure their codec's body limit but leave
the surrounding `Framed` reader at its default of about eight MiB. The local
patch gives the reader the configured body budget plus the codec's existing
bounded header allowance. It does not change protocol admission, authentication,
session handling, or the codec's own validation.

The modern HTTP session then re-encodes each admitted request into a bounded
queue. Its separate codec previously retained a ten-MiB default, and its queue
retained a sixteen-MiB byte budget even when the HTTP body limit was larger.
The local transport constructor passes the configured message limit to the
codec before creating either handle. Each direction keeps a finite byte budget
of at least sixteen MiB, or one configured maximum message plus its framing
byte. Zero limits and framing overflow are rejected. Existing default
constructors, queue counts, cancellation, and byte accounting stay unchanged.
Both the dual-era endpoint and the modern-only server shim use this constructor.

After transport admission, final tool dispatch had additional fixed limits:
256 KiB for raw parameters, 64 KiB for the argument digest, one MiB for its
exact JSON parser, and 64 KiB per schema instance string. The local server
configuration gives only the registered `announce` tool an explicit input
budget, no larger than the already configured HTTP body limit. Unknown tool
names, zero limits, and limits beyond that body budget fail server construction.
Unlisted tools retain every original default.

That one budget reaches raw parameter admission, the argument digest, exact
request parsing, and all four legacy/modern and direct/nested schema validation
paths. Local and nested calls also check the encoded argument size. The exact
parser enlarges value strings only beneath the decoded top-level `arguments`
member; result decoding and sibling strings retain their original limits.
Object keys, duplicate-key rejection, exact numbers, nesting, value/container
counts, schema rules, strict additional-property checks, metadata, and request
routing remain validated. The combined non-argument parameters retain their
256 KiB ceiling, and MRTR input responses retain their separate 192 KiB limit.
Continuation bindings remain active: only the argument digest receives the
configured budget; target, principal, grant, retry ownership, and replay checks
are unchanged.

The root Cargo manifest applies all four copies through `[patch]`. The other
FastAPI/FastMCP crates retain their exact Git revisions, and the runtime remains
the single registry `asupersync 0.5.0` required by the workspace.

## Provenance and maintenance

Each directory retains the upstream source, tests, license material, and
original Cargo manifest. Its active manifest expands inherited package,
dependency, and lint settings so the crate works outside the upstream workspace;
sibling dependencies still point to the exact original Git revision. The
`FRANKENSONOS_PATCH.diff` file records only the Rust changes against that revision.

Keep these patches narrow. Once the upstream revisions used by FrankenSonos
include equivalent fixes, remove the corresponding local override in a reviewed
dependency update and rerun the same upload regression tests.

## Caller addresses for tailnet identity

`fastapi-http`'s connection handlers receive each connection's peer address but
never set the `RemoteAddr` request extension, although
`fastapi_core::middleware::RemoteAddr` documents setting it as the server's job.
So no handler could tell who called. The local patch (`note_peer`) sets it on
every request of the HTTP/1 and h2c application listeners and of the HTTP/1
handler listener. The handler h2c path is given no address by its caller and
is unchanged. FrankenSonos uses the address to name tailnet callers with
Tailscale's WhoIs (`fsonos_api::Identity::with_tailnet`). Routing, limits and
the Host checks are unchanged.

## Regression coverage

`crates/fsonos-cli/tests/e2e_announce.rs` sends a WAV with two MiB of metadata
through HTTP and one with twelve MiB of metadata through MCP (more than sixteen
MiB after base64 encoding). The scenario verifies valid media fetches from the
daemon's shared listener, volume caps, state restoration, the daemon CLI upload,
and rejection of malformed or ambiguous input without additional playback.
These sizes intentionally exceed the old transport and semantic ceilings.
The same scenario confirms that a large `echo` request still hits the default
limit, and malformed or over-limit WAV bytes are rejected after decoding without
an extra playback. Focused protocol regressions exercise configured boundaries,
unchanged defaults, exact raw/source equality, and retained structural checks.

`crates/fsonos-api/tests/http_api.rs`
(`a_tailnet_caller_is_named_by_the_tailnet`) serves the API over a real socket
and passes only if each request carries its caller's address: the tailnet namer
gets that address, names the caller, and the caller's own policy applies.

The Linux speech workflow watches `vendor/**` and runs formatting, the complete
workspace check and Clippy gates, and the full regression/simulator suite. It
also runs `cargo test --locked -p fastmcp-protocol --lib tool_input_limit`
explicitly, because the vendored protocol crate is excluded from the workspace
and its focused regression tests would otherwise be omitted.
