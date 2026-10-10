# Pinned transport patches for WAV announcements

These two crates carry narrowly scoped transport fixes required by the
announcement upload path. They are copied from the same Git revisions used by
the rest of the workspace; this does not upgrade the Franken stack.

| Crate | Upstream source |
| --- | --- |
| `fastapi-http` | [`fastapi_rust@cb9d72926434e2ec3e08cced5eae7f7c13168c7d`](https://github.com/Dicklesworthstone/fastapi_rust/tree/cb9d72926434e2ec3e08cced5eae7f7c13168c7d/crates/fastapi-http) |
| `fastmcp-server` | [`fastmcp_rust@03b5274544048babf6b358874310038a9ee4f76a`](https://github.com/Dicklesworthstone/fastmcp_rust/tree/03b5274544048babf6b358874310038a9ee4f76a/crates/fastmcp-server) |

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

The root Cargo manifest applies both copies through `[patch]`. The other
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

## Regression coverage

`crates/fsonos-cli/tests/e2e_announce.rs` sends a WAV with two MiB of metadata
through HTTP and one with eight MiB of metadata through MCP (more than ten MiB
after base64 encoding). The scenario verifies valid media fetches from the
daemon's shared listener, volume caps, state restoration, the daemon CLI upload,
and rejection of malformed or ambiguous input without additional playback.
These sizes intentionally exceed the old transport ceilings.

The Linux speech workflow watches `vendor/**` and runs formatting, the complete
workspace check and Clippy gates, and the full regression/simulator suite.
