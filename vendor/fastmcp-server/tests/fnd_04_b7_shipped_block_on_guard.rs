//! bd-fnd04-b7-shipped-block-on-proxy-4rkp9, R5 and R6.
//!
//! R5 asks for one `#[test]` that asserts the shipped `block_on` call-site count
//! is zero across four files, and that the guard FAILS CLOSED when a named
//! source file is absent, renamed or unreadable, with that behaviour itself
//! asserted. R6 asks for a near-identical negative proving the guard can refuse,
//! run against an in-memory copy of a shipped file with exactly one call site
//! reintroduced, naming the file and line it objected to.
//!
//! The zero assertion (`fnd_04_b7_shipped_block_on_guard_four_files_are_zero`)
//! landed once R1/R2's removals did. Before then it was deliberately withheld:
//! a knowingly-red test would have broken the target for every lane.
//!
//! WHY A NEW FILE rather than an addition to an existing one. The only test in
//! this package that reads source text is `fnd_07_a.rs`, which belongs to
//! another bead and is cited by its receipts; adding a FND-04 guard there would
//! disturb a frozen surface and could void evidence this bead has no business
//! touching.
//!
//! WIDENED TO THE LIBRARY GRAPH. `guard_library_crate` applies the same scan to
//! every file each of `GUARDED_CRATES` ships, found by walking `mod` declarations
//! from `src/lib.rs`. It also refuses library-side runtime construction. The one
//! exemption is `fastmcp_core::block_on`'s own body. The workspace zero is
//! `fnd_04_b7_library_graph_has_no_block_on_or_runtime_construction`.
//!
//! The counting rules mirror `tools/shipped_block_on_census.py`, which is the
//! instrument R3/R4 were established with, except that cfg predicates are
//! evaluated for satisfiability: `not(feature = "x")` ships when the feature is
//! off. The census read 21 / 7 / 3 / 0 shipped sites for the four files before
//! the removals.

use std::path::{Path, PathBuf};

/// The four files R1 and R2 name, relative to the repository root.
const GUARDED_FILES: [&str; 4] = [
    "crates/fastmcp-server/src/proxy.rs",
    "crates/fastmcp-server/src/router.rs",
    "crates/fastmcp-server/src/lib.rs",
    "crates/fastmcp-server/src/legacy_2024.rs",
];

/// Why the guard refused. Every variant is a REFUSAL — there is deliberately no
/// variant meaning "could not tell", because a guard that cannot find its
/// subject and reports success is the defect R5 exists to prevent.
#[derive(Debug, PartialEq, Eq)]
enum Objection {
    /// A named file is absent, renamed, or unreadable.
    Unreadable { path: String },
    /// Shipped `block_on` call sites remain, with the 1-based line of each.
    ShippedCallSites { path: String, lines: Vec<usize> },
    /// Shipped library code builds a runtime, with the 1-based line of each.
    RuntimeConstruction { path: String, lines: Vec<usize> },
    /// An out-of-line `mod name;` names no file, so the walk cannot see what
    /// it compiles.
    UnresolvedModule {
        path: String,
        line: usize,
        name: String,
    },
}

impl std::fmt::Display for Objection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { path } => {
                write!(f, "{path}: absent, renamed or unreadable")
            }
            Self::ShippedCallSites { path, lines } => {
                write!(f, "{path}: shipped block_on call sites at lines {lines:?}")
            }
            Self::RuntimeConstruction { path, lines } => {
                write!(f, "{path}: shipped runtime construction at lines {lines:?}")
            }
            Self::UnresolvedModule { path, line, name } => {
                write!(f, "{path}:{line}: `mod {name};` resolves to no file")
            }
        }
    }
}

/// Blanks comment, string, raw-string, byte-string and char content, preserving
/// every newline so line numbers survive.
///
/// This step is not optional. A brace matcher that counts braces inside string
/// literals and doc comments loses module boundaries entirely, and a `block_on`
/// named in a doc comment is not a call. The census measured the unmasked form
/// over-counting `#[cfg(test)]` regions on `proxy.rs` by 13 -> 395.
fn mask_non_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            out[i] = b'\n';
            i += 1;
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'\n' {
                    out[i] = b'\n';
                }
                if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if b == b'r' && raw_string_hashes(bytes, i).is_some() {
            let hashes = raw_string_hashes(bytes, i).expect("checked by the guard above");
            i += 1 + hashes + 1;
            loop {
                if i >= bytes.len() {
                    break;
                }
                if bytes[i] == b'\n' {
                    out[i] = b'\n';
                }
                if bytes[i] == b'"' && closing_hashes(bytes, i + 1, hashes) {
                    i += 1 + hashes;
                    break;
                }
                i += 1;
            }
        } else if b == b'\''
            && let Some(end) = char_literal_end(bytes, i)
        {
            // A char literal can hold a quote or a brace ('"', '{'). Left in,
            // it opens a phantom string or shifts every later brace match.
            i = end;
        } else if b == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\n' {
                    out[i] = b'\n';
                }
                if bytes[i] == b'\\' {
                    // A `\` line continuation escapes the newline itself. Keep
                    // it, or every later line number drifts by one.
                    if bytes.get(i + 1) == Some(&b'\n') {
                        out[i + 1] = b'\n';
                    }
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else {
            out[i] = b;
            i += 1;
        }
    }
    String::from_utf8(out).expect("masking replaces bytes with spaces and keeps newlines")
}

/// `Some(n)` when position `i` begins a raw string with `n` hashes.
fn raw_string_hashes(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut hashes = 0usize;
    while j < bytes.len() && bytes[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    (j < bytes.len() && bytes[j] == b'"').then_some(hashes)
}

fn closing_hashes(bytes: &[u8], from: usize, hashes: usize) -> bool {
    (0..hashes).all(|k| bytes.get(from + k) == Some(&b'#'))
}

/// `Some(end)` just past a char literal opening at `i`, or `None` for a
/// lifetime or label (`'a`), which has no closing quote.
fn char_literal_end(bytes: &[u8], i: usize) -> Option<usize> {
    let body = *bytes.get(i + 1)?;
    let close = if body == b'\\' {
        // '\n', '\'', '\x7f', '\u{10FFFF}': the closing quote is near.
        (i + 3..(i + 12).min(bytes.len())).find(|&k| bytes[k] == b'\'')?
    } else {
        // One UTF-8 scalar, one to four bytes, then the closing quote.
        let width = match body {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 4,
        };
        let k = i + 1 + width;
        (bytes.get(k) == Some(&b'\'')).then_some(k)?
    };
    Some(close + 1)
}

/// Byte ranges covered by an item only `cargo test` compiles, by brace matching
/// over masked source.
///
/// Resolved to whichever ITEM follows the attribute (`mod`, `fn`, `impl`), not
/// only to `mod`, because a `#[cfg(test)]` on a FUNCTION is invisible to a
/// stripper that only removes `#[cfg(test)] mod X { .. }`.
///
/// The predicate is EVALUATED, never substring-matched.
/// `any(feature = "legacy-2024-11-05", test)` mentions `test` but ships whenever
/// that default feature is on; matching the substring treated about 3,300
/// shipped lines of `lib.rs` as test code. `#[cfg(not(test))]` is shipped too.
///
/// An item without a body (`use x;`, `mod tests;`) ends at its semicolon, so it
/// can never swallow the item that follows it.
fn test_regions(masked: &str) -> Vec<(usize, usize)> {
    let bytes = masked.as_bytes();
    let mut regions = Vec::new();
    let mut search = 0usize;
    while let Some(found) = masked[search..].find("#[cfg(") {
        let at = search + found;
        let open = at + "#[cfg".len();
        let Some(close) = balanced_end(bytes, open) else {
            break;
        };
        search = close + 1;
        if !cfg_is_test_only(&masked[open + 1..close]) {
            continue;
        }
        // Step past this attribute's `]` and any further attributes on the item.
        let mut item = masked[close..]
            .find(']')
            .map_or(bytes.len(), |offset| close + offset + 1);
        loop {
            while item < bytes.len() && bytes[item].is_ascii_whitespace() {
                item += 1;
            }
            let next_attribute = (bytes.get(item) == Some(&b'#'))
                .then(|| masked[item..].find('['))
                .flatten()
                .and_then(|offset| balanced_end(bytes, item + offset));
            match next_attribute {
                Some(end) => item = end + 1,
                None => break,
            }
        }
        if let Some(end) = item_end(bytes, item) {
            regions.push((at, end));
            search = search.max(end);
        }
    }
    regions
}

/// Index of the delimiter that closes the one at `open` (`(`, `[` or `{`).
fn balanced_end(bytes: &[u8], open: usize) -> Option<usize> {
    let (opening, closing) = match *bytes.get(open)? {
        b'(' => (b'(', b')'),
        b'[' => (b'[', b']'),
        b'{' => (b'{', b'}'),
        _ => return None,
    };
    let mut depth = 0usize;
    for (index, &byte) in bytes.iter().enumerate().skip(open) {
        if byte == opening {
            depth += 1;
        } else if byte == closing {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Index of the last byte of the item starting at `start`: its closing `}`, or
/// its `;`, `,` or enclosing `}` when it has no body of its own.
fn item_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, &byte) in bytes.iter().enumerate().skip(start) {
        match byte {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            b'{' if depth == 0 => return balanced_end(bytes, index),
            b';' | b',' | b'}' if depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
}

/// Whether an item gated by `cfg(<predicate>)` compiles only under `cargo test`:
/// with `test` false and every other predicate free, the predicate never holds.
fn cfg_is_test_only(predicate: &str) -> bool {
    !cfg_can_be(predicate, true)
}

/// Whether `predicate` can evaluate to `value` outside `cargo test`, where
/// `test` is false and every other predicate may take either value.
fn cfg_can_be(predicate: &str, value: bool) -> bool {
    let predicate = predicate.trim();
    for operator in ["all", "any", "not"] {
        let Some(arguments) = predicate
            .strip_prefix(operator)
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('('))
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        let arguments = split_cfg_arguments(arguments);
        return match (operator, value) {
            ("all", true) | ("any", false) => {
                arguments.iter().all(|argument| cfg_can_be(argument, value))
            }
            ("all", false) | ("any", true) => {
                arguments.iter().any(|argument| cfg_can_be(argument, value))
            }
            _ => arguments
                .first()
                .is_some_and(|argument| cfg_can_be(argument, !value)),
        };
    }
    predicate != "test" || !value
}

/// The comma-separated arguments of a cfg operator, split at paren depth zero.
fn split_cfg_arguments(arguments: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut from) = (0usize, 0usize);
    for (index, byte) in arguments.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(arguments[from..index].trim());
                from = index + 1;
            }
            _ => {}
        }
    }
    parts.push(arguments[from..].trim());
    parts.retain(|part| !part.is_empty());
    parts
}

/// 1-based lines holding a SHIPPED `block_on` CALL site.
///
/// A call is `block_on` followed by `(`. A mention inside a `use` statement is an
/// import, not a call. Doc comments never reach this point; masking removed them.
fn shipped_block_on_sites(source: &str) -> Vec<usize> {
    let masked = mask_non_code(source);
    let regions = test_regions(&masked);
    let mut lines = Vec::new();
    let mut search = 0usize;
    while let Some(found) = masked[search..].find("block_on") {
        let at = search + found;
        search = at + "block_on".len();
        let after = masked[search..].trim_start();
        if !after.starts_with('(') {
            continue;
        }
        if regions.iter().any(|&(start, end)| at >= start && at <= end) {
            continue;
        }
        let line_start = masked[..at].rfind('\n').map_or(0, |n| n + 1);
        if masked[line_start..at].trim_start().starts_with("use ") {
            continue;
        }
        lines.push(masked[..at].matches('\n').count() + 1);
    }
    lines
}

/// The guard. Refuses on an unreadable file BEFORE it can report a false clean.
fn guard_file(root: &Path, relative: &str) -> Result<(), Objection> {
    let path = root.join(relative);
    let source = std::fs::read_to_string(&path).map_err(|_| Objection::Unreadable {
        path: relative.to_owned(),
    })?;
    guard_source(relative, &source)
}

/// The same predicate over source TEXT, so R6's negative can run against an
/// in-memory copy without touching the tree.
fn guard_source(relative: &str, source: &str) -> Result<(), Objection> {
    let lines = shipped_block_on_sites(source);
    if lines.is_empty() {
        Ok(())
    } else {
        Err(Objection::ShippedCallSites {
            path: relative.to_owned(),
            lines,
        })
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the server crate sits two levels below the repository root")
        .to_path_buf()
}

/// The library crates whose shipped graph must hold no `block_on` call and no
/// runtime construction. The CLI is the application boundary: its `main`
/// builds the one top-level runtime, so it is deliberately not listed. Nor are
/// examples or bin targets, which are consumer entry points.
const GUARDED_CRATES: [&str; 8] = [
    "crates/fastmcp-core",
    "crates/fastmcp-protocol",
    "crates/fastmcp-transport",
    "crates/fastmcp-client",
    "crates/fastmcp-server",
    "crates/fastmcp-macros",
    "crates/fastmcp-console",
    "crates/fastmcp",
];

/// `fastmcp_core::block_on` builds and drives a runtime by definition. The body
/// of that one function is the only library-side exemption; the rest of its
/// file is scanned like any other.
const BRIDGE_DEFINITION: (&str, &str) = ("crates/fastmcp-core/src/runtime.rs", "pub fn block_on");

/// One file a library target compiles outside `cargo test`.
struct ShippedFile {
    relative: String,
    source: String,
}

/// One out-of-line `mod name;` declaration.
struct ModuleDeclaration {
    name: String,
    line: usize,
    test_only: bool,
    explicit_path: Option<String>,
    /// Enclosing inline modules, outermost first; they nest the file path.
    inline: Vec<String>,
}

fn disk(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every file the library target of `crate_dir` compiles outside `cargo test`,
/// found by walking out-of-line `mod name;` declarations from `src/lib.rs`.
///
/// A test-only declaration is not followed: nothing it reaches ships, and a
/// file it shares with a shipped declaration is reached through that one. A
/// shipped declaration that names no file is a refusal, never a skip. `read`
/// supplies every source, so a planted negative can substitute one in memory.
fn shipped_crate_files(
    root: &Path,
    crate_dir: &str,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Result<Vec<ShippedFile>, Objection> {
    let lib = root.join(crate_dir).join("src").join("lib.rs");
    let mut files = std::collections::BTreeMap::new();
    let mut queue = vec![(lib.clone(), lib.parent().map(Path::to_path_buf))];
    while let Some((path, module_dir)) = queue.pop() {
        if files.contains_key(&path) {
            continue;
        }
        let source = read(&path).ok_or_else(|| Objection::Unreadable {
            path: relative_to(root, &path),
        })?;
        let module_dir = module_dir.unwrap_or_else(|| root.to_path_buf());
        for declaration in module_declarations(&source) {
            if declaration.test_only {
                continue;
            }
            // Inline blocks nest the module directory. A top-level `#[path]` is
            // relative to the declaring file's own directory instead.
            let nested = declaration
                .inline
                .iter()
                .fold(module_dir.clone(), |dir, name| dir.join(name));
            let candidates = match &declaration.explicit_path {
                Some(explicit) if declaration.inline.is_empty() => {
                    vec![path.parent().unwrap_or(root).join(explicit)]
                }
                Some(explicit) => vec![nested.join(explicit)],
                None => vec![
                    nested.join(format!("{}.rs", declaration.name)),
                    nested.join(&declaration.name).join("mod.rs"),
                ],
            };
            let Some(child) = candidates
                .into_iter()
                .find(|candidate| read(candidate).is_some())
            else {
                return Err(Objection::UnresolvedModule {
                    path: relative_to(root, &path),
                    line: declaration.line,
                    name: declaration.name,
                });
            };
            // A `mod.rs` or `#[path]` file owns its own directory; `x.rs` owns `x/`.
            let owns_parent =
                declaration.explicit_path.is_some() || child.file_name() == Some("mod.rs".as_ref());
            let child_dir = if owns_parent {
                child.parent().map(Path::to_path_buf)
            } else {
                Some(child.with_extension(""))
            };
            queue.push((child, child_dir));
        }
        files.insert(path, source);
    }
    Ok(files
        .into_iter()
        .map(|(path, source)| ShippedFile {
            relative: relative_to(root, &path),
            source,
        })
        .collect())
}

/// The out-of-line module declarations in `source`, each classified by the same
/// evaluated cfg regions the call-site scan uses.
fn module_declarations(source: &str) -> Vec<ModuleDeclaration> {
    let masked = mask_non_code(source);
    let regions = test_regions(&masked);
    let inline_spans = inline_module_spans(&masked);
    let raw_lines: Vec<&str> = source.lines().collect();
    let mut declarations = Vec::new();
    let mut offset = 0usize;
    for (index, line) in masked.split_inclusive('\n').enumerate() {
        let line_start = offset;
        offset += line.len();
        // `#[cfg(feature = "x")] mod y;` may share its line with an attribute.
        let mut trimmed = line.trim_start();
        while trimmed.starts_with("#[")
            && let Some(close) = balanced_end(trimmed.as_bytes(), 1)
        {
            trimmed = trimmed[close + 1..].trim_start();
        }
        let Some(name) = out_of_line_module_name(trimmed) else {
            continue;
        };
        let at = line_start + (line.len() - trimmed.len());
        let explicit_path = raw_lines[..index]
            .iter()
            .rev()
            .map(|raw| raw.trim())
            .take_while(|raw| raw.starts_with('#') || raw.starts_with("//"))
            .find_map(|raw| {
                let value = raw.strip_prefix("#[path")?.split('"').nth(1)?;
                Some(value.to_owned())
            });
        let mut inline: Vec<&(usize, usize, String)> = inline_spans
            .iter()
            .filter(|(open, close, _)| *open < at && at < *close)
            .collect();
        inline.sort_by_key(|(open, _, _)| *open);
        declarations.push(ModuleDeclaration {
            name: name.to_owned(),
            line: index + 1,
            test_only: regions.iter().any(|&(start, end)| at >= start && at <= end),
            explicit_path,
            inline: inline
                .into_iter()
                .map(|(_, _, name)| name.clone())
                .collect(),
        });
    }
    declarations
}

/// `Some(name)` when a masked, left-trimmed line is `[pub[(..)]] mod name;`.
fn out_of_line_module_name(trimmed: &str) -> Option<&str> {
    let rest = strip_visibility(trimmed).strip_prefix("mod")?;
    let rest = rest
        .strip_prefix(|c: char| c.is_ascii_whitespace())?
        .trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let (name, tail) = rest.split_at(end);
    (!name.is_empty() && tail.trim_start().starts_with(';')).then_some(name)
}

fn strip_visibility(item: &str) -> &str {
    let Some(rest) = item.strip_prefix("pub") else {
        return item;
    };
    let rest = rest.trim_start();
    match rest.strip_prefix('(') {
        Some(scoped) => scoped
            .find(')')
            .map_or(item, |close| scoped[close + 1..].trim_start()),
        None => rest,
    }
}

/// `(open, close, name)` for every inline `mod name { .. }` in masked source.
fn inline_module_spans(masked: &str) -> Vec<(usize, usize, String)> {
    let bytes = masked.as_bytes();
    let mut spans = Vec::new();
    let mut search = 0usize;
    while let Some(found) = masked[search..].find("mod") {
        let at = search + found;
        search = at + 3;
        let bounded_before =
            at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let rest = &masked[at + 3..];
        if !bounded_before || !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let rest = rest.trim_start();
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let (name, tail) = rest.split_at(end);
        let tail_trimmed = tail.trim_start();
        if name.is_empty() || !tail_trimmed.starts_with('{') {
            continue;
        }
        let open = masked.len() - tail_trimmed.len();
        if let Some(close) = balanced_end(bytes, open) {
            spans.push((open, close, name.to_owned()));
        }
    }
    spans
}

/// 1-based lines where shipped code builds a runtime: `RuntimeBuilder::..` or
/// `Runtime::new(..)` outside every test-only region.
fn runtime_construction_sites(source: &str) -> Vec<usize> {
    let masked = mask_non_code(source);
    let regions = test_regions(&masked);
    let bytes = masked.as_bytes();
    let mut lines = Vec::new();
    for needle in ["RuntimeBuilder::", "Runtime::new"] {
        let mut search = 0usize;
        while let Some(found) = masked[search..].find(needle) {
            let at = search + found;
            search = at + needle.len();
            let bounded =
                at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
            if !bounded || regions.iter().any(|&(start, end)| at >= start && at <= end) {
                continue;
            }
            lines.push(masked[..at].matches('\n').count() + 1);
        }
    }
    lines.sort_unstable();
    lines
}

/// The guard over one library crate's whole shipped graph. Returns how many
/// shipped files it scanned, so a caller can prove it was aimed at something.
fn guard_library_crate(
    root: &Path,
    crate_dir: &str,
    read: &dyn Fn(&Path) -> Option<String>,
) -> Result<usize, Vec<Objection>> {
    let files = shipped_crate_files(root, crate_dir, read).map_err(|objection| vec![objection])?;
    let mut objections = Vec::new();
    for file in &files {
        let exempt = (file.relative == BRIDGE_DEFINITION.0)
            .then(|| exempt_lines(&file.source, BRIDGE_DEFINITION.1))
            .flatten();
        let outside = |line: &usize| exempt.as_ref().is_none_or(|range| !range.contains(line));
        let calls: Vec<usize> = shipped_block_on_sites(&file.source)
            .into_iter()
            .filter(outside)
            .collect();
        if !calls.is_empty() {
            objections.push(Objection::ShippedCallSites {
                path: file.relative.clone(),
                lines: calls,
            });
        }
        let builds: Vec<usize> = runtime_construction_sites(&file.source)
            .into_iter()
            .filter(outside)
            .collect();
        if !builds.is_empty() {
            objections.push(Objection::RuntimeConstruction {
                path: file.relative.clone(),
                lines: builds,
            });
        }
    }
    if objections.is_empty() {
        Ok(files.len())
    } else {
        Err(objections)
    }
}

/// The 1-based line range of the item whose masked text begins with `item`.
fn exempt_lines(source: &str, item: &str) -> Option<std::ops::RangeInclusive<usize>> {
    let masked = mask_non_code(source);
    let at = masked.find(item)?;
    let open = at + masked[at..].find('{')?;
    let close = balanced_end(masked.as_bytes(), open)?;
    let line_of = |offset: usize| masked[..offset].matches('\n').count() + 1;
    Some(line_of(at)..=line_of(close))
}

/// A copy of `source` with `planted` appended as its own top-level item, and the
/// 1-based line the plant starts on.
fn plant_at_end(source: &str, planted: &str) -> (String, usize) {
    let mut out = source.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let line = out.matches('\n').count() + 1;
    out.push_str(planted);
    out.push('\n');
    (out, line)
}

/// `disk`, except that `subject` reads as `replacement`.
fn substituting<'a>(
    subject: &'a Path,
    replacement: &'a str,
) -> impl Fn(&Path) -> Option<String> + 'a {
    move |path| {
        if path == subject {
            Some(replacement.to_owned())
        } else {
            disk(path)
        }
    }
}

/// R5, zero half: the four files R1 and R2 name hold no shipped `block_on` call.
#[test]
fn fnd_04_b7_shipped_block_on_guard_four_files_are_zero() {
    let root = repository_root();
    let objections: Vec<String> = GUARDED_FILES
        .iter()
        .filter_map(|relative| guard_file(&root, relative).err())
        .map(|objection| objection.to_string())
        .collect();
    assert!(objections.is_empty(), "{}", objections.join("\n"));
}

/// The workspace zero. No guarded library crate's shipped graph calls
/// `block_on` or builds a runtime, except `fastmcp_core::block_on`'s own body.
/// The same instrument refuses a single planted site
/// (`fnd_04_b7_library_graph_planted_negative`) and fails closed
/// (`fnd_04_b7_library_graph_fails_closed`), so this zero is not a blind one.
#[test]
fn fnd_04_b7_library_graph_has_no_block_on_or_runtime_construction() {
    let root = repository_root();
    let mut objections = Vec::new();
    let mut scanned = 0usize;
    for crate_dir in GUARDED_CRATES {
        match guard_library_crate(&root, crate_dir, &disk) {
            Ok(files) => scanned += files,
            Err(found) => objections.extend(found.iter().map(ToString::to_string)),
        }
    }
    assert!(
        objections.is_empty(),
        "shipped library code still bridges or builds a runtime:\n{}",
        objections.join("\n")
    );
    assert!(
        scanned > GUARDED_CRATES.len(),
        "the walk must reach past each crate root; scanned {scanned} files"
    );
}

/// R5, fail-closed half. A named file that is absent, renamed or unreadable must
/// make the guard REFUSE — not pass, not skip — and the refusal must name it.
#[test]
fn fnd_04_b7_shipped_block_on_guard_fails_closed_positive() {
    let root = repository_root();
    let missing = "crates/fastmcp-server/src/this_file_does_not_exist.rs";

    let objection = guard_file(&root, missing)
        .expect_err("a guard that cannot read its subject must refuse, never report success");

    assert_eq!(
        objection,
        Objection::Unreadable {
            path: missing.to_owned()
        },
        "the refusal must name the file it could not read"
    );
    assert!(
        objection.to_string().contains(missing),
        "the rendered objection must carry the path: {objection}"
    );

    // The four real files must all be READABLE, so the check above is testing
    // fail-closed behaviour rather than a typo in GUARDED_FILES. Without this the
    // test would still pass if every guarded path were wrong.
    for relative in GUARDED_FILES {
        assert!(
            root.join(relative).is_file(),
            "guarded path must exist, else the guard is aimed at nothing: {relative}"
        );
    }
}

/// R6. The same guard, against an in-memory copy of a shipped file with exactly
/// ONE call site reintroduced, must refuse and name the file and the line.
#[test]
fn fnd_04_b7_shipped_block_on_guard_planted_negative() {
    let relative = "crates/fastmcp-server/src/legacy_2024.rs";
    let root = repository_root();
    let pristine = std::fs::read_to_string(root.join(relative))
        .expect("the planted negative needs a readable subject");

    // legacy_2024.rs is the file the census reports at ZERO shipped call sites,
    // so it is the one subject where a clean baseline is available today and the
    // plant is the only variable. R1's removals have not landed, so the other
    // three still hold sites and could not distinguish a plant from a survivor.
    guard_source(relative, &pristine)
        .expect("baseline must be clean, or the plant below proves nothing");

    let mut planted = String::new();
    let mut planted_line = 0usize;
    for (index, line) in pristine.lines().enumerate() {
        planted.push_str(line);
        planted.push('\n');
        if planted_line == 0 && line.starts_with("use ") {
            planted.push_str("fn fnd04_b7_planted() { let _ = block_on(async {}); }\n");
            planted_line = index + 2;
        }
    }
    assert_ne!(planted_line, 0, "the plant must have been inserted");
    assert_eq!(
        planted.matches("block_on(").count(),
        pristine.matches("block_on(").count() + 1,
        "the plant changes exactly one dimension: one added call site"
    );

    let objection = guard_source(relative, &planted)
        .expect_err("one reintroduced shipped call site must be refused");

    assert_eq!(
        objection,
        Objection::ShippedCallSites {
            path: relative.to_owned(),
            lines: vec![planted_line],
        },
        "the refusal must name the file and the exact line it objected to"
    );
}

/// The guard must not count what is not a shipped call. Each case below is a way
/// a naive matcher reports a false positive, and `block_on` appears in all of
/// them.
#[test]
fn fnd_04_b7_shipped_block_on_guard_excludes_non_calls_positive() {
    let cases: [(&str, &str); 5] = [
        ("import", "use futures::executor::block_on;\n"),
        ("line comment", "// block_on(x) is not a call here\n"),
        ("doc comment", "/// See block_on(x) for details.\n"),
        ("string", "const S: &str = \"block_on(x)\";\n"),
        ("raw string", "const R: &str = r#\"block_on(x)\"#;\n"),
    ];
    for (label, source) in cases {
        assert_eq!(
            shipped_block_on_sites(source),
            Vec::<usize>::new(),
            "{label} must not count as a shipped call site"
        );
    }

    // cfg(test) exclusion, on a FUNCTION rather than a mod — the form a
    // mod-only stripper misses.
    let gated = "#[cfg(test)]\nfn t() { block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(gated),
        Vec::<usize>::new(),
        "a call inside a cfg(test) item is not shipped"
    );

    // CONTROL: the identical body WITHOUT the attribute must be counted, so the
    // exclusion above is doing work rather than the matcher simply never firing.
    let shipped = "fn t() { block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(shipped),
        vec![1],
        "the same call outside cfg(test) MUST be counted"
    );

    // cfg(not(test)) is the opposite of a test gate and stays shipped.
    let not_test = "#[cfg(not(test))]\nfn t() { block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(not_test),
        vec![2],
        "cfg(not(test)) is shipped code"
    );
}

/// A cfg predicate that MENTIONS `test` is not thereby test-only. Each pair
/// differs only in whether the predicate can hold outside `cargo test`.
#[test]
fn fnd_04_b7_shipped_block_on_guard_evaluates_cfg_predicates() {
    let body = "fn t() { block_on(async {}); }\n";
    let cases: [(&str, Vec<usize>); 7] = [
        // Ships whenever the feature is on: lib.rs gates thousands of lines so.
        (
            "#[cfg(any(feature = \"legacy-2024-11-05\", test))]\n",
            vec![2],
        ),
        (
            "#[cfg(all(test, feature = \"legacy-2024-11-05\"))]\n",
            vec![],
        ),
        // A feature whose NAME contains "test" is not the `test` predicate.
        ("#[cfg(feature = \"test-internals\")]\n", vec![2]),
        ("#[cfg(test)]\n", vec![]),
        // not(feature) ships when the feature is off; not(test) always ships.
        ("#[cfg(not(feature = \"legacy-2024-11-05\"))]\n", vec![2]),
        ("#[cfg(all(unix, not(test)))]\n", vec![2]),
        ("#[cfg(all(unix, any(test, all(test, windows))))]\n", vec![]),
    ];
    for (attribute, expected) in cases {
        assert_eq!(
            shipped_block_on_sites(&format!("{attribute}{body}")),
            expected,
            "{attribute:?}"
        );
    }
}

/// A gated item WITHOUT a body ends at its semicolon. Before this rule the guard
/// ran on to the next `{` and excluded whatever shipped item came after.
#[test]
fn fnd_04_b7_shipped_block_on_guard_bodyless_item_does_not_swallow_the_next() {
    let declared = "#[cfg(test)]\nmod tests;\nfn shipped() { block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(declared),
        vec![3],
        "a shipped fn after `#[cfg(test)] mod tests;` must still be counted"
    );
    let inline = "#[cfg(test)]\nmod tests { fn shipped() { block_on(async {}); } }\n";
    assert_eq!(
        shipped_block_on_sites(inline),
        Vec::<usize>::new(),
        "CONTROL: the same call inside the inline test module is excluded"
    );
    let stacked = "#[cfg(test)]\n#[allow(dead_code)]\nfn gated() { block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(stacked),
        Vec::<usize>::new(),
        "further attributes between the cfg and its item do not detach the gate"
    );
}

/// Masking keeps every newline, including one a `\` line continuation escapes
/// inside a string, so reported lines match the source.
#[test]
fn fnd_04_b7_shipped_block_on_guard_keeps_lines_across_string_continuations() {
    // runtime.rs has 7 `\` continuations; dropping their newlines put a planted
    // call at line 795 instead of 802.
    let continued = "const S: &str = \"a \\\n    b\";\nfn t() { block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(continued),
        vec![3],
        "the call's line is counted through the continued string"
    );
    let single = "const S: &str = \"a b\";\n\nfn t() { block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(single),
        vec![3],
        "CONTROL: the same layout without a continuation"
    );
}

/// Char literals holding a quote or a brace must not desynchronise the scan.
#[test]
fn fnd_04_b7_shipped_block_on_guard_masks_char_literals() {
    let quote = "fn s() { let q = '\"'; block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(quote),
        vec![1],
        "a '\"' char literal must not open a string that hides the call"
    );
    // Unmasked, the '{' leaves this item's braces unbalanced, so the region never
    // closes and the gated call is counted as shipped.
    let brace = "#[cfg(test)]\nfn t() { let b = '{'; block_on(async {}); }\n";
    assert_eq!(
        shipped_block_on_sites(brace),
        Vec::<usize>::new(),
        "a '{{' char literal must not break the cfg(test) item's brace match"
    );
    let lifetime = "fn s<'a>(x: &'a str) -> &'a str { block_on(async {}); x }\n";
    assert_eq!(
        shipped_block_on_sites(lifetime),
        vec![1],
        "lifetimes are code, not char literals"
    );
}

/// The crate walk is aimed at the shipped graph. It reaches files that only a
/// `mod` chain names, follows `#[path]`, and leaves test-only modules out. A
/// walker that scanned nothing would pass every zero assertion, so each crate
/// must yield files and specific deep ones are named.
#[test]
fn fnd_04_b7_library_graph_reaches_shipped_files_and_skips_test_modules() {
    let root = repository_root();
    let walked = |crate_dir: &str| -> Vec<String> {
        shipped_crate_files(&root, crate_dir, &disk)
            .unwrap_or_else(|objection| panic!("{crate_dir}: {objection}"))
            .into_iter()
            .map(|file| file.relative)
            .collect()
    };
    for crate_dir in GUARDED_CRATES {
        assert!(
            !walked(crate_dir).is_empty(),
            "{crate_dir} yields no shipped files"
        );
    }
    let core = walked("crates/fastmcp-core");
    assert!(
        core.iter()
            .any(|path| path == "crates/fastmcp-core/src/runtime/envelope.rs"),
        "reached only through `pub mod envelope;` in runtime.rs: {core:?}"
    );
    assert!(
        !core
            .iter()
            .any(|path| path == "crates/fastmcp-core/src/limit_01_rows.rs"),
        "declared `#[cfg(test)] mod limit_01_rows;`, so not shipped"
    );
    let console = walked("crates/fastmcp-console");
    assert!(
        console
            .iter()
            .any(|path| path == "crates/fastmcp-console/src/client/traffic.rs"),
        "`#[path = \"client/traffic.rs\"] pub mod traffic;` resolves: {console:?}"
    );
    let server = walked("crates/fastmcp-server");
    assert!(
        server
            .iter()
            .any(|path| path == "crates/fastmcp-server/src/proxy.rs")
    );
    assert!(
        !server
            .iter()
            .any(|path| path == "crates/fastmcp-server/src/tests.rs"),
        "declared `#[cfg(test)] mod tests;`, so not shipped"
    );
}

/// The crate guard refuses rather than reports clean when it cannot see its
/// subject: a missing crate root, or a shipped `mod name;` naming no file. The
/// same declaration under `#[cfg(test)]` needs no file, which is the control.
#[test]
fn fnd_04_b7_library_graph_fails_closed() {
    let root = repository_root();
    let missing = "crates/fastmcp-does-not-exist";
    assert_eq!(
        shipped_crate_files(&root, missing, &disk).err(),
        Some(Objection::Unreadable {
            path: format!("{missing}/src/lib.rs"),
        }),
    );

    let lib = root.join("crates/fastmcp-core/src/lib.rs");
    let pristine = disk(&lib).expect("fastmcp-core's lib.rs is readable");
    let (shipped, line) = plant_at_end(&pristine, "mod fnd04_b7_does_not_exist;");
    assert_eq!(
        shipped_crate_files(&root, "crates/fastmcp-core", &substituting(&lib, &shipped)).err(),
        Some(Objection::UnresolvedModule {
            path: "crates/fastmcp-core/src/lib.rs".to_owned(),
            line,
            name: "fnd04_b7_does_not_exist".to_owned(),
        }),
        "a shipped declaration without a file is refused, never skipped"
    );
    let (gated, _) = plant_at_end(&pristine, "#[cfg(test)]\nmod fnd04_b7_does_not_exist;");
    assert!(
        shipped_crate_files(&root, "crates/fastmcp-core", &substituting(&lib, &gated)).is_ok(),
        "CONTROL: the same declaration under cfg(test) is not part of the shipped graph"
    );
}

/// R6 for the crate guard. One call site, or one runtime construction, planted in
/// a file only the module tree reaches is refused by file and line. The same
/// plant in a test-only module is not shipped, so it is not refused.
#[test]
fn fnd_04_b7_library_graph_planted_negative() {
    let root = repository_root();
    let core = "crates/fastmcp-core";
    guard_library_crate(&root, core, &disk)
        .unwrap_or_else(|objections| panic!("baseline must be clean: {objections:?}"));

    let envelope = root.join("crates/fastmcp-core/src/runtime/envelope.rs");
    let pristine = disk(&envelope).expect("envelope.rs is readable");
    let relative = "crates/fastmcp-core/src/runtime/envelope.rs".to_owned();
    let (planted, line) = plant_at_end(
        &pristine,
        "fn fnd04_b7_planted() { let _ = block_on(async {}); }",
    );
    assert_eq!(
        guard_library_crate(&root, core, &substituting(&envelope, &planted)),
        Err(vec![Objection::ShippedCallSites {
            path: relative.clone(),
            lines: vec![line],
        }]),
    );
    let (planted, line) = plant_at_end(
        &pristine,
        "fn fnd04_b7_planted() { let _ = asupersync::runtime::RuntimeBuilder::current_thread(); }",
    );
    assert_eq!(
        guard_library_crate(&root, core, &substituting(&envelope, &planted)),
        Err(vec![Objection::RuntimeConstruction {
            path: relative,
            lines: vec![line],
        }]),
    );

    let limit = root.join("crates/fastmcp-core/src/limit_01_rows.rs");
    let (planted, _) = plant_at_end(
        &disk(&limit).expect("limit_01_rows.rs is readable"),
        "fn fnd04_b7_planted() { let _ = block_on(async {}); }",
    );
    assert!(
        guard_library_crate(&root, core, &substituting(&limit, &planted)).is_ok(),
        "CONTROL: the identical plant in a `#[cfg(test)] mod` file is not shipped"
    );
}

/// `fastmcp_core::block_on`'s own body is the one exemption, and it covers that
/// body only. The definition really does call and build (so the exemption is
/// doing work), and a second call elsewhere in the same file is refused.
#[test]
fn fnd_04_b7_library_graph_exempts_only_the_bridge_definition() {
    let root = repository_root();
    let runtime = root.join(BRIDGE_DEFINITION.0);
    let pristine = disk(&runtime).expect("the bridge's file is readable");
    assert!(
        !shipped_block_on_sites(&pristine).is_empty()
            && !runtime_construction_sites(&pristine).is_empty(),
        "CONTROL: the definition calls block_on and builds a runtime"
    );
    let (planted, line) = plant_at_end(
        &pristine,
        "fn fnd04_b7_planted() { let _ = block_on(async {}); }",
    );
    assert_eq!(
        guard_library_crate(
            &root,
            "crates/fastmcp-core",
            &substituting(&runtime, &planted)
        ),
        Err(vec![Objection::ShippedCallSites {
            path: BRIDGE_DEFINITION.0.to_owned(),
            lines: vec![line],
        }]),
    );
}
