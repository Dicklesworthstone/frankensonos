//! Built-in resource providers for common use cases.
//!
//! This module provides pre-built resource providers that can be registered
//! with a server to expose common data sources as MCP resources.
//!
//! # Available Providers
//!
//! - [`FilesystemProvider`]: Exposes a directory as MCP resources on Linux and
//!   macOS. Public `build` fails closed on other targets. Listing and reads
//!   use the caller-owned asupersync blocking pool when one is installed.
//! - With the `apps` feature, `McpAppsUiResource`: one immutable final-only
//!   `ui://` HTML document for a negotiated MCP Apps View.
//! - [`RegisteredSchemaTool`]: a tool using explicitly provisioned schema
//!   resources through the router's normal input/output validation path.
//! - With the `proxy` feature, `managed_oauth::ManagedOAuthProvider`: bounded,
//!   authenticated modern upstream catalogs and request-owned forwarding.
//! - With the `proxy` feature, `ClientCredentialsProvider`: the same exact
//!   tool, resource, prompt, template and completion handlers using an explicitly
//!   provisioned machine identity instead of an interactive OAuth login.
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_rust::prelude::*;
//! use fastmcp_rust::providers::{FilesystemProvider, FilesystemProviderError};
//!
//! let result = FilesystemProvider::new("/data/docs")
//!     .with_prefix("docs")
//!     .with_patterns(&["**/*.md", "**/*.txt"])
//!     .with_recursive(true)
//!     .build();
//! // On Linux/macOS this is `Ok(handler)`. Other targets remain FeatureUnavailable.
//! ```

#![forbid(unsafe_code)]

/// Caller-owned, bounded blocking execution for synchronous MCP handlers.
pub mod blocking;

mod filesystem;
#[cfg(feature = "apps")]
mod mcp_apps;
mod schema_tool;

/// Authenticated, request-owned modern core forwarding using a managed login.
#[cfg(feature = "proxy")]
pub mod managed_oauth;
#[cfg(feature = "proxy")]
pub use managed_oauth::dynamic::ClientCredentialsProvider;

pub use filesystem::{FilesystemProvider, FilesystemProviderError, FilesystemResourceHandler};
pub use blocking::{BlockingCompletion, BlockingHandlerLane, BlockingPrompt, BlockingResource, BlockingTool};
#[cfg(feature = "apps")]
pub use mcp_apps::{McpAppsUiResource, McpAppsUiResourceError};
pub use schema_tool::{RegisteredSchemaTool, RegisteredSchemaToolError};
pub use fastmcp_protocol::{SchemaRegistryError, SchemaRegistryLimits, SchemaResourceRegistry};

/// Enumeration guard over the `host_cancelled` conversion family.
///
/// This lives here, beside the `#[cfg(feature = "proxy")]` that gates
/// `managed_oauth`, rather than inside that module. Placing it inside would
/// gate the guard behind the same feature as the thing it guards, so a default
/// `cargo test -p fastmcp-server` would skip it and exit 0 -- the silent-skip
/// shape this guard exists to refuse. Nothing here reads a `managed_oauth`
/// item: the check is pure text over the source tree, so it runs whether or
/// not `proxy` is enabled and whether or not that module compiles.
#[cfg(test)]
mod host_cancelled_pairing {
    /// Frozen count of `host_cancelled` conversions under `managed_oauth/`:
    /// `interaction` and `dynamic::machine::interaction`. Raising this is a
    /// deliberate act -- see the guard below for what it costs.
    const HOST_CANCELLED_CONVERSION_SITES: usize = 2;

    /// Every `.rs` file under `dir`, recursively, as `(display path, contents)`.
    fn collect_rust_sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect_rust_sources(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let contents = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
                out.push((path.display().to_string(), contents));
            }
        }
    }

    /// Names declared by a `fn` item in `contents`, in source order.
    ///
    /// A line whose trimmed form starts with `//` is skipped whole. It is
    /// deliberately not truncated at its first `//`: this module tree carries
    /// `report://` and `note://` inside dozens of string literals, and cutting
    /// those mid-string is how a scanner starts lying about what a file
    /// declares. Line-level skipping is sufficient because the tree contains
    /// no block comments; if one is added, this needs revisiting.
    fn declared_fn_names(contents: &str) -> Vec<&str> {
        let mut names = Vec::new();
        for line in contents.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut offset = 0;
            while let Some(at) = line[offset..].find("fn ") {
                let start = offset + at;
                offset = start + 3;
                let word_boundary = start == 0
                    || !matches!(line.as_bytes()[start - 1],
                        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_');
                if !word_boundary {
                    continue;
                }
                let name = line[offset..]
                    .split(|character: char| !character.is_alphanumeric() && character != '_')
                    .next()
                    .unwrap_or_default();
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
        names
    }

    /// Paths declaring a `host_cancelled` conversion, in walk order.
    ///
    /// Exact name equality, so the pair tests -- `host_cancelled_*_by_variant`
    /// and `host_cancelled_*_planted_negative` -- are not miscounted as
    /// conversions themselves.
    fn host_cancelled_conversion_sites(sources: &[(String, String)]) -> Vec<String> {
        sources
            .iter()
            .filter(|(_, contents)| declared_fn_names(contents).contains(&"host_cancelled"))
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Conversion sites with no `host_cancelled_*_by_variant` +
    /// `host_cancelled_*_planted_negative` pair declared in the same file.
    fn host_cancelled_conversions_missing_their_pair(sources: &[(String, String)]) -> Vec<String> {
        let mut missing = Vec::new();
        for (path, contents) in sources {
            let names = declared_fn_names(contents);
            if !names.contains(&"host_cancelled") {
                continue;
            }
            let has = |suffix: &str| {
                names
                    .iter()
                    .any(|name| name.starts_with("host_cancelled_") && name.ends_with(suffix))
            };
            if !(has("_by_variant") && has("_planted_negative")) {
                missing.push(path.clone());
            }
        }
        missing
    }

    /// A third `host_cancelled`-style conversion cannot arrive under
    /// `managed_oauth/` without its test pair.
    ///
    /// Both interaction error types implement
    /// `managed_oauth::interaction::HostDisposition`, which owns the mapping
    /// from a host failure or a failed checkpoint. Each implementation still
    /// names its own `host_cancelled` variant, because lifting one type into
    /// the other through `From` would change a value (`Interaction(..)` versus
    /// `Core(Protocol(..))`). A new implementation is a new variant that no
    /// test exercises until its pair exists, which is the gap this guards.
    ///
    /// It is a source-level check because the property is source-level: an
    /// untested conversion is invisible to every runtime assertion. It walks
    /// the directory rather than reading fixed files through `include_str!`,
    /// because `include_str!` cannot see a conversion added in a *new* file --
    /// the likeliest way a third one arrives.
    ///
    /// The frozen count is the anti-rename control. A scanner that cannot find
    /// its subject reports success; if `host_cancelled` is renamed away, the
    /// count falls rather than this passing vacuously.
    #[test]
    fn no_host_cancelled_conversion_lacks_its_test_pair() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/providers/managed_oauth");
        let mut sources = Vec::new();
        collect_rust_sources(&root, &mut sources);
        assert!(
            !sources.is_empty(),
            "the walk found no sources under {}",
            root.display()
        );

        let sites = host_cancelled_conversion_sites(&sources);
        let found = sites.len();
        let frozen = HOST_CANCELLED_CONVERSION_SITES;
        assert_eq!(
            found, frozen,
            "this tree froze at {frozen} `host_cancelled` conversions; \
             the walk found {found}: {sites:?}. If one was added, give it a \
             `host_cancelled_*_by_variant` + `host_cancelled_*_planted_negative` pair in its own \
             file and raise the constant. If one was renamed or removed, this guard is now blind \
             to it -- repair the scan, do not lower the constant to match it.",
        );

        let unpaired = host_cancelled_conversions_missing_their_pair(&sources);
        assert!(
            unpaired.is_empty(),
            "these files declare a `host_cancelled` conversion with no \
             `host_cancelled_*_by_variant` + `host_cancelled_*_planted_negative` pair \
             beside it: {unpaired:?}",
        );
    }

    /// Planted negative for the guard above: it names a conversion that
    /// arrives without its pair, and does not name the two that have one.
    ///
    /// The guard is required to be *seen refusing*. Doing that by editing the
    /// tree into a broken state proves it once, in a scratch no receipt can
    /// cite. This fixture makes the refusal permanent and re-runnable.
    ///
    /// Two controls are folded in. The paired rows are the over-fire control:
    /// a predicate that simply named every conversion would fail this
    /// assertion too. And the third row's missing pair is named only in
    /// comments, so if the comment skip in `declared_fn_names` were dropped,
    /// that file would read as paired and this assertion would fail.
    #[test]
    fn a_host_cancelled_conversion_arriving_without_its_pair_is_named() {
        let paired = |kind: &str| {
            format!(
                "fn host_cancelled() -> {kind} {{ todo!() }}\n\
                 #[test]\n\
                 fn host_cancelled_{kind}_by_variant() {{}}\n\
                 #[test]\n\
                 fn host_cancelled_{kind}_planted_negative() {{}}\n",
            )
        };
        let planted = vec![
            ("managed_oauth/interaction.rs".to_owned(), paired("managed")),
            (
                "managed_oauth/dynamic/machine/interaction.rs".to_owned(),
                paired("machine"),
            ),
            (
                "managed_oauth/dynamic/device/interaction.rs".to_owned(),
                "fn host_cancelled() -> Device { todo!() }\n\
                 // TODO: fn host_cancelled_device_by_variant() and\n\
                 // fn host_cancelled_device_planted_negative() are still unwritten.\n"
                    .to_owned(),
            ),
        ];

        assert_eq!(host_cancelled_conversion_sites(&planted).len(), 3);
        assert_eq!(
            host_cancelled_conversions_missing_their_pair(&planted),
            vec!["managed_oauth/dynamic/device/interaction.rs".to_owned()],
        );
    }
}
