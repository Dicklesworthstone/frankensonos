//! FND-07 A: filesystem capability and path-policy primitives.
//!
//! An **external** consumer of the packaged `fastmcp-server` crate: it drives
//! the public filesystem provider boundary — `providers::FilesystemProvider`
//! built into a `FilesystemResourceHandler`, read through the public
//! `ResourceHandler` trait — and never through `use super::` or a
//! `pub(crate)` path. Nothing here is compiled under `cfg(test)` inside the
//! library (PL-3).
//!
//! # What this slice owns
//!
//! FND-07 A is *capability and path policy*. The provider retains one
//! directory capability opened at build time and resolves every request
//! relative to it, so a request can never be converted back into an ambient
//! path. On top of that capability sit four distinct alias defenses, and the
//! interesting property is that they are four, not one:
//!
//! 1. **Component policy** — absolute paths, prefixes, root, `.` and `..` are
//!    rejected outright.
//! 2. **Separator aliasing** — `a//b` and `a/b/` normalize to the same
//!    components as `a/b`, so accepting them would let one file answer to
//!    several policy names while glob exclusions inspect the raw request.
//! 3. **Percent-encoding aliasing** — the decoded path is re-encoded and
//!    compared to the request, so `%2e%2e` and over-encodings like `%72` for
//!    a literal `r` are refused rather than silently canonicalized.
//! 4. **Byte hygiene** — control characters, `?`, `#`, and Unicode bidi
//!    overrides are refused, and the request is length-bounded.
//!
//! Each of those admits a *different* wrong path, so a negative that only
//! plants `..` would leave three defenses unproven.
//!
//! # What this slice does NOT own
//!
//! Bounded I/O, symlink policy and error isolation are FND-07 B. Windows
//! reparse/junction/ADS behaviour, the `SecureAtomicFile` contract, and the
//! blocking-executor routing are elsewhere in the package. Nothing here
//! establishes FND-07 completion or any aggregate MCP claim.
//!
//! # Dependency divergence, recorded rather than resolved
//!
//! The FND-07 package contract names "FND-01's exact `cap-std =4.0.2` and
//! `cap-fs-ext =4.0.2` candidates". The workspace pins **`=4.0.3`** for both
//! and that is what the provider is built against, so this evaluator is
//! written against 4.0.3 — the version that actually ships. The divergence is
//! asserted below so it stays visible, and correcting the contract is a
//! re-attestation decision this bead does not own.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::Cx;
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{Runtime, RuntimeBuilder};

use fastmcp_core::{McpContext, McpError};
use fastmcp_server::ResourceHandler;
use fastmcp_server::providers::{FilesystemProvider, FilesystemResourceHandler};

// ---------------------------------------------------------------------------
// Frozen floors and stable diagnostics
// ---------------------------------------------------------------------------

/// The request length bound, anchored to the literal rather than to the
/// private constant that defines it — a floor compared against its own
/// definition cannot fail.
const MAX_RELATIVE_PATH_BYTES: usize = 4096;

/// The stable public refusal for every path-policy violation.
///
/// It deliberately names no path. A refusal that echoed the request would turn
/// the provider into an oracle for what exists outside the root.
const PATH_REJECTED: &str = "Filesystem resource path was rejected";

/// The pinned `cap-std` version the provider is actually built against.
const PINNED_CAP_STD: &str = "=4.0.3";

/// The version the FND-07 package contract names.
const CONTRACT_CAP_STD: &str = "=4.0.2";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A unique temporary root, so concurrent runs cannot collide.
fn fresh_root(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "fastmcp-fnd07a-{label}-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("nested")).expect("test root is creatable");
    fs::write(root.join("readable.txt"), b"root-file-contents\n").expect("root file is writable");
    fs::write(
        root.join("nested").join("deep.txt"),
        b"nested-file-contents\n",
    )
    .expect("nested file is writable");
    root
}

/// Builds the provider over `root` through its public builder.
fn handler(root: &Path) -> FilesystemResourceHandler {
    FilesystemProvider::new(root)
        .with_recursive(true)
        .build()
        .expect("the filesystem provider builds on this platform")
}

/// One explicit top-level runtime, which a test harness is permitted to own.
fn application_runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("platform reactor is available"))
        .blocking_threads(0, 2)
        .build()
        .expect("application-owned runtime builds")
}

/// Drives one read through the public `ResourceHandler` boundary.
///
/// `read_file` takes a checkpoint on the caller's context, so this needs a
/// live `Cx` rather than a detached one.
fn read_uri(handler: &FilesystemResourceHandler, uri: &str) -> Result<String, McpError> {
    application_runtime().block_on(async move {
        let cx = Cx::current().expect("block_on installs a current Cx");
        let ctx = McpContext::new(cx, 1);
        let params: HashMap<String, String> = HashMap::new();
        handler
            .read_with_uri(&ctx, uri, &params)
            .map(|contents| format!("{contents:?}"))
    })
}

/// Asserts that a refusal is the stable path rejection and leaks nothing.
///
/// `#[track_caller]` so a failure reports the CASE that failed rather than
/// this helper's own line. Without it every one of the planted cases panics at
/// the same location, and a panic line read as a defect location sends the
/// reader to the wrong code.
#[track_caller]
fn assert_path_rejected(label: &str, error: &McpError, requested: &str) {
    let rendered = error.to_string();
    assert!(
        rendered.contains(PATH_REJECTED),
        "{label}: expected the stable path refusal, got {rendered}"
    );
    // The refusal must not echo the request. A provider that repeated the
    // rejected path back would confirm or deny what lies outside the root.
    let leaked = requested
        .trim_start_matches("file:///")
        .trim_end_matches('/');
    if leaked.len() >= 4 && !leaked.contains('%') {
        assert!(
            !rendered.contains(leaked),
            "{label}: the refusal must not echo the requested path: {rendered}"
        );
    }
}

// ---------------------------------------------------------------------------
// fnd_07_a_positive
// ---------------------------------------------------------------------------

#[test]
fn fnd_07_a_positive() {
    let root = fresh_root("positive");
    let provider = handler(&root);

    // --- The capability resolves ordinary relative requests -----------------
    let single = read_uri(&provider, "file:///readable.txt")
        .expect("a canonical single-component request is served");
    assert!(
        single.contains("root-file-contents"),
        "the read must return the file's bytes: {single}"
    );

    let nested = read_uri(&provider, "file:///nested/deep.txt")
        .expect("a canonical multi-component request is served");
    assert!(
        nested.contains("nested-file-contents"),
        "handle-relative traversal must reach a nested component: {nested}"
    );
    assert_ne!(
        single, nested,
        "two distinct paths must resolve to distinct contents"
    );

    // --- The capability is retained, not reopened per request ---------------
    //
    // Reading twice through the same handler must be stable. This is the
    // observable side of "one retained directory handle": a provider that
    // reopened an ambient path per request would still pass, so this is a
    // necessary condition rather than a proof of the mechanism, and is
    // labelled as such.
    let repeat = read_uri(&provider, "file:///readable.txt")
        .expect("the retained capability serves repeat requests");
    assert_eq!(
        single, repeat,
        "the retained directory capability must serve identical bytes across requests"
    );

    // --- Containment: nothing outside the root is reachable ------------------
    //
    // A sibling file next to the root, reachable only by escaping it.
    let outside = root.with_file_name(format!(
        "{}-outside.txt",
        root.file_name().expect("root has a name").to_string_lossy()
    ));
    fs::write(&outside, b"outside-contents\n").expect("sibling file is writable");
    let escape = read_uri(&provider, "file:///../outside.txt")
        .expect_err("a parent-traversal request must be refused");
    assert_path_rejected("parent traversal", &escape, "file:///../outside.txt");

    // --- The frozen request bound --------------------------------------------
    let overlong = format!("file:///{}", "a".repeat(MAX_RELATIVE_PATH_BYTES + 1));
    let bounded = read_uri(&provider, &overlong).expect_err("an overlong request must be refused");
    assert_path_rejected("overlong request", &bounded, "");
    // No `assert_eq!(MAX_RELATIVE_PATH_BYTES, 4096)` here: that compares this
    // file's own constant with its own value and cannot fail. The production
    // bound is private, so there is nothing to compare it against. The refusal
    // above is what actually binds it — if the shipped limit were raised, the
    // request built from this constant would stop being overlong and this case
    // would fail.

    // --- Dependency divergence, recorded --------------------------------------
    //
    // Asserted against the tree so this fails if the pin moves, rather than
    // being a comment that silently rots.
    let manifest = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root exists above crates/fastmcp-server")
            .join("Cargo.toml"),
    )
    .expect("workspace manifest is readable");
    assert!(
        manifest.contains(&format!("cap-std = {{ version = \"{PINNED_CAP_STD}\"")),
        "the provider is built against cap-std {PINNED_CAP_STD}; if this pin moved, every \
         capability claim in this evaluator needs re-deriving against the new archive"
    );
    // The divergence is checked against the TREE, not between two constants I
    // wrote. Comparing `PINNED_CAP_STD` to `CONTRACT_CAP_STD` would be two
    // hard-coded literals disagreeing by construction and could never fail.
    assert!(
        !manifest.contains(&format!("cap-std = {{ version = \"{CONTRACT_CAP_STD}\"")),
        "the tree no longer diverges from the contract's cap-std {CONTRACT_CAP_STD}; the \
         divergence recorded by this bead has been resolved and this evaluator's premise \
         needs revisiting rather than the assertion being deleted"
    );
    println!(
        "fnd-07-a divergence: package contract names cap-std {CONTRACT_CAP_STD}; \
         tree pins and this evaluator is built against {PINNED_CAP_STD}"
    );

    let _ = fs::remove_file(&outside);
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// fnd_07_a_planted_negative
// ---------------------------------------------------------------------------

#[test]
fn fnd_07_a_planted_negative() {
    let root = fresh_root("negative");
    let provider = handler(&root);

    // --- Control: the accepted request works before any mutation ------------
    //
    // A refusal proves something only if acceptance was reachable through the
    // same handler in the same configuration.
    let accepted = read_uri(&provider, "file:///nested/deep.txt")
        .expect("the control request must be accepted before any mutation");
    assert!(accepted.contains("nested-file-contents"));

    // --- One alias dimension changed per case --------------------------------
    //
    // Every case is a single-dimension mutation of the accepted request above,
    // and each targets a *different* defense. A negative that only planted
    // `..` would leave the separator, encoding and byte-hygiene defenses
    // entirely unproven.
    let cases: [(&str, &str); 10] = [
        ("component: parent traversal", "file:///../readable.txt"),
        ("component: current directory", "file:///./readable.txt"),
        (
            "component: embedded parent",
            "file:///nested/../readable.txt",
        ),
        ("component: absolute request", "file:////readable.txt"),
        ("separator alias: doubled", "file:///nested//deep.txt"),
        ("separator alias: trailing", "file:///nested/deep.txt/"),
        (
            "encoding alias: percent-encoded traversal",
            "file:///%2e%2e/readable.txt",
        ),
        (
            "encoding alias: over-encoded literal",
            "file:///%72eadable.txt",
        ),
        ("byte hygiene: query delimiter", "file:///readable.txt?x=1"),
        (
            "byte hygiene: fragment delimiter",
            "file:///readable.txt#frag",
        ),
    ];

    for (label, uri) in cases {
        let error = read_uri(&provider, uri)
            .err()
            .unwrap_or_else(|| panic!("{label} must be refused: {uri}"));
        assert_path_rejected(label, &error, uri);

        // Named mutable state unchanged: the accepted request still resolves
        // to identical bytes after every refusal.
        let still = read_uri(&provider, "file:///nested/deep.txt")
            .unwrap_or_else(|error| panic!("{label} must leave the control readable: {error}"));
        assert_eq!(
            still, accepted,
            "{label} must leave the accepted request byte-for-byte unchanged"
        );
    }

    // --- Bidi override, kept separate because it is a spoofing defense -------
    //
    // A right-to-left override can make a rejected path render as an accepted
    // one in a log or a UI. It is refused on its bytes, not its appearance.
    let bidi = format!("file:///nested/{}deep.txt", '\u{202e}');
    let bidi_error = read_uri(&provider, &bidi).expect_err("a bidi override must be refused");
    assert_path_rejected("byte hygiene: bidi override", &bidi_error, "");

    // --- The near-identical positive -----------------------------------------
    //
    // The same request without the one forbidden character is still accepted,
    // which is what makes each refusal above one-variable rather than a path
    // that was never going to work.
    let unchanged = read_uri(&provider, "file:///nested/deep.txt")
        .expect("the near-identical accepted form differs only in the forbidden dimension");
    assert_eq!(unchanged, accepted);

    // --- A missing file is refused WITHOUT confirming its absence ------------
    //
    // The redaction boundary matters most here: a distinguishable "not found"
    // for a policy-clean path that does not exist would let a caller map the
    // directory. The provider answers with a redacted identity.
    let missing = read_uri(&provider, "file:///nested/absent.txt")
        .expect_err("a policy-clean but absent path is still refused");
    let rendered = missing.to_string();
    assert!(
        !rendered.contains("absent.txt"),
        "a not-found refusal must not echo the probed name: {rendered}"
    );

    let _ = fs::remove_dir_all(&root);
}
