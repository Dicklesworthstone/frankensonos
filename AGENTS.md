# AGENTS.md — FrankenSonos

Guidelines for AI coding agents working in this repository. Claude-compatible
tools also read `CLAUDE.md`, which points here.

## Rule 0 — Direct Instructions

Follow Jeffrey's direct instructions. These rules encode standing preferences;
they do not overrule the user. Finish authorized work and report concrete
results, with the checks that support them. Do not claim runtime or integration
success from documentation or a passing `cargo check` alone.

## Rule 1 — Scope boundary is law

Read [`docs/SCOPE.md`](docs/SCOPE.md) before any task. FrankenSonos is a **local
interoperability controller for the owner's own Sonos hardware on the owner's
own LAN** — the same category as SoCo and Home Assistant. Agents build:
discovery, UPnP/SOAP control, GENA events, DIDL/URI construction, Spotify **Web
API library reads** (owner's own account), the daemon, API, MCP server, CLI, DJ,
and the Tailscale deployment.

Agents do **not** (these are out of the swarm's backlog): reflash or modify
device firmware; reverse-engineer or patch Sonos binaries; defeat or probe
authentication/encryption/DRM/access controls; extract or re-use any secrets
beyond the owner's own OAuth tokens in the local cache; target anything the
owner does not control; or do network capture as an engineering task. If a task
reads as "attack something" or "handle someone's secrets", it is out of scope —
stop and ask. None of the project's goals require crossing these lines.

## No Deletion Or Destructive Git

Do not delete files or directories without explicit written permission,
including files you created. Do not run `git reset --hard`, `git clean -fd`,
`rm -rf`, force pushes, forced branch moves/deletes (`git branch -M/-D`,
`git push --force`), or equivalent, without explicit authorization for the exact
operation. Inspect before changing. Preserve work you did not create; never
stash, revert, or blanket-stage another agent's changes — stage explicit owned
paths. Work on `main`; create a branch only when asked.

## Project Mission And Reading Order

FrankenSonos is a memory-safe Rust end-run around Sonos's own software: control
the owner's Sonos gear reliably from a CLI, an HTTP API, and an MCP server, and
act as a tasteful classical-music DJ from the owner's own Spotify library,
including off-LAN over Tailscale.

```text
SSDP discover + UPnP/SOAP control + GENA events  (fsonos-proto)
  -> live inventory/topology + coordinator-addressed control + durable store (fsonos-core)
  -> Spotify library reads + DJ selection (fsonos-spotify)
  -> HTTP API (fsonos-api) + MCP tools (fsonos-mcp) + `fsonos` CLI/daemon (fsonos-cli)
```

Read before substantive work:

1. [`COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md`](COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md)
   — the self-contained source of truth: goals, architecture, lanes, protocols,
   the dependency recipe, data model, testing, milestones.
2. [`docs/SCOPE.md`](docs/SCOPE.md) — the in-scope / out-of-scope boundary.
3. [`README.md`](README.md) — the product and command contract.
4. Actual source, the committed `Cargo.lock`, fixtures, and live `br` task state.

When code and prose disagree, current source + executed checks establish
behavior; reconcile at the boundary and update the docs.

## Architecture Doctrine

- One Cargo workspace; the lanes own disjoint crates (`fsonos-{types,proto,
  core,spotify,api,mcp,cli}`). Do not reach into another lane's crate files;
  coordinate at the public API boundary.
- **Pure core, I/O at the edges.** Protocol encode/decode (SSDP, SOAP, GENA,
  DIDL) and DJ selection are pure, unit-tested without a network. I/O sits
  behind the `Transport` and `Store` traits; the franken async stack is wired in
  one place (bead `FND-DEPS`).
- **Coordinator-addressed control.** Group-wide commands target the group's
  coordinator; `fsonos-core` resolves it. **Event-driven state** via GENA, not
  polling storms. **Two households** (S1/S2) handled explicitly where protocols
  differ (music-service linkage, Queue, SonosNet vs WiFi).
- `#![forbid(unsafe_code)]` at every crate root (the workspace lints enforce it).
  This workload has no reason for unsafe, SIMD islands, or unsafe parsers.
- Rust 2024, toolchain pinned in `rust-toolchain.toml` (`nightly-2026-08-31`),
  committed `Cargo.lock` (this is an application — the lock IS tracked).

## Dependency Recipe (the franken stack) — highest integration risk

None of the owner's patched libraries are on crates.io for these versions. The
**known-good** wiring (proven by the `am_baseline` project's committed lockfile)
is recorded, commented, in the workspace `Cargo.toml`. Bead `FND-DEPS` activates
the needed subset per crate and proves `cargo check --workspace` still passes.

- `asupersync = "0.5"` (crates.io registry, feature `tls-webpki-roots`). It
  **must unify** on one `0.5.x` across our crates + fastmcp + fastapi. Do not
  path/git-dep asupersync (our local checkout is `0.6` and would fork `Cx`).
- `fsqlite` / `fsqlite-types`: git `frankensqlite@2633b38…`. Use the **non-`Cx`
  `Connection` API**; it rides its own internal asupersync line. **Prove a real
  open→create→insert→query round-trip** before any lane depends on it. No
  `bundled` feature exists (from-scratch engine).
- `fastmcp` (`package = "fastmcp-rust"`) + `fastmcp-core/-server/-transport/
  -protocol`: git `fastmcp_rust@03b5274…` (v0.10).
- `fastapi` (`package = "fastapi-rust"`) + `fastapi-core/-router`: git
  `fastapi_rust@cb9d729…` (v0.4.4).
- No Tokio, no reqwest, no `bundled` SQLite. All networking is asupersync.

Real API shapes to use (verified against the library sources) are in the plan,
§4. When an API is uncertain, read the pinned dependency source — a plan sketch
is not proof that something compiles or that a blocking call is cancellable.

## Editing Discipline

Prefer narrow edits to existing modules. Do not create `*_v2.rs`,
`*_improved.rs`, speculative frameworks, or duplicate implementations. Use `rg`
for search. Keep comments at the density of the surrounding code; write code
that reads like its neighbors. Keep all site data (IPs, MACs, serials, tokens,
room lists, captures) out of git — it is learned at runtime and stored locally.
Scrub site identifiers from any committed test fixture.

## Verification And Gates

Builds offload to the `rch` fleet; `cargo` is the gate, `rch` is where it runs.
After substantive Rust changes run the relevant tests plus:

```bash
cargo fmt --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Infrastructure failure is not a test pass. Prefer real localhost sockets and a
real fsqlite `:memory:`/tempfile over mocks. Live-LAN tests are opt-in behind a
feature/env flag so CI never needs the network. Keep the pure unit tests green
at all times.

## Beads And The Swarm (code-first / batch-verify + honest credit)

Use `br` for implementation tasks.

```bash
br ready --json            # claimable work
br show ISSUE_ID
br update ISSUE_ID --status in_progress
br close ISSUE_ID --reason "Implemented + verified with <named checks / evidence>"
br dep cycles
```

Swarm doctrine (one lane per agent, disjoint crates):

1. **Claim** a ready bead; reserve your crate paths if coordinating via Agent
   Mail. Do not take out-of-scope work (`docs/SCOPE.md`).
2. **Code + real tests in the same bead.** Pass the format/`check` gate. Commit
   immediately with a conventional message. **Do not `git push`** — commit
   locally only; the orchestrator pushes. Concurrent pushers diverge `main` and
   cause rejected pushes. If you must sync first, `git pull --rebase`, never a
   force-push.
3. **The orchestrator runs the verifying pass** (`cargo clippy -D warnings`,
   `cargo test --workspace`) and is the only one that **closes** beads and
   **pushes**, citing evidence (the commands that passed, the revision).

Honest-credit floor (enforced): process artifacts are not progress; a refusal
or guard path does not close a positive-capability bead; commits are not a KPI;
a close without cited evidence is a debt to be reopened. Do not split in-scope
acceptance conditions into new beads to close the original. Report denominators
honestly (timeouts and failures stay in the count).

## Product Voice (README and user-facing docs)

FrankenSonos is positioned as **the best way to run Sonos** — confident and
truthful. Never write deferential comparisons: do not call SoCo / node-sonos /
Home Assistant "mature", "battle-tested", or "worth using instead", and never
describe FrankenSonos as "young", "limited", or "not as capable". State what
FrankenSonos does that nothing else does. Being pre-release may be stated as
honest status (a roadmap, a limitations list), never as deference to a competitor.
**Tailscale is central:** keep "command your speakers from anywhere in the world,
as long as you're on your Tailscale tailnet" a headline capability, and make the
tailnet path first-class in the daemon and the deploy docs.

## GitHub And Contributions

Repository: <https://github.com/Dicklesworthstone/frankensonos>. Use `gh` for
repo operations. Outside submissions are reports to investigate and
independently reproduce/verify, not patches to merge directly.

Commit trailer:

```
Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
```

PR descriptions end with:

```
🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

## Session Completion

1. Finish the authorized change and inspect the final diff.
2. Run the gates above and state exactly what ran and what passed/failed.
3. Update owned beads with implementation and evidence; leave incomplete gates
   and exact blockers visible.
4. Stage explicit owned paths and commit with a descriptive message. Push only
   authorized work; never force-push to resolve a surprise.
5. Report result, validation, and concrete remaining work. No runtime or
   integration claims from docs or `cargo check` alone.
