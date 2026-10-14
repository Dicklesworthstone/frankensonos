//! FND-05 B: the IANA special-purpose registry drift gate. FAILS CLOSED.
//!
//! The guarded-fetch destination classifier decides which resolved addresses
//! the fetcher may reach. That decision is a frozen judgement about content
//! IANA mutates, and it FAILS OPEN: a newly designated special-purpose range
//! the classifier does not know about is classified public, admitted, and
//! connected to — with no error and no failing test anywhere. A frozen
//! deny-list cannot announce when it has stopped being correct.
//!
//! This gate removes that silence. It is a RATCHET, not a snapshot comparison:
//! it fails on ANY change in EITHER direction — a new IANA range, a classifier
//! regression, or a classifier FIX — so every movement forces a deliberate,
//! reviewed update instead of drifting quietly.
//!
//! NO NETWORK. The registries are vendored under `evidence/fnd-05/iana/`.
//! Nothing here contacts IANA. Refresh is a documented manual procedure in
//! `provenance.toml`; see its `prohibition` field before touching the pin.
//!
//! WHY `global` IS THE DISCRIMINATOR. Presence in the registry is not the test.
//! Several special-purpose prefixes are globally REACHABLE anycast services
//! (PCP, TURN, AS112, AMT) that a fetch fence may legitimately admit. The
//! security-relevant set is the prefixes IANA marks globally UNREACHABLE, and
//! that is what this gate binds.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root exists above crates/fastmcp-transport")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(workspace_root().join(relative))
        .unwrap_or_else(|error| panic!("{relative} is readable: {error}"))
}

/// Uses the crate's own bounded digest rather than pulling in a new
/// dependency; `fastmcp_core` is already a dependency of this crate and
/// `sha256_bounded` is the same primitive the transport itself uses.
fn sha256_hex(bytes: &[u8]) -> String {
    const BOUND: usize = 1 << 20;
    let digest = fastmcp_core::sha256_bounded(bytes, BOUND)
        .expect("a vendored registry snapshot stays inside the 1 MiB hashing bound");
    let mut rendered = String::with_capacity(64);
    for byte in digest.as_bytes() {
        use std::fmt::Write;
        let _ = write!(rendered, "{byte:02x}");
    }
    rendered
}

/// Strict single-value extraction from the provenance record.
///
/// Deliberately not a TOML parse: `toml` is not a dependency of this crate and
/// this gate must not require a manifest change to a crate four other lanes
/// build. The format is authored alongside this reader, and a missing field
/// panics rather than defaulting, so a silently absent pin cannot let the gate
/// pass while checking nothing.
fn field_after(block: &str, key: &str) -> String {
    let needle = format!("\n{key} = ");
    let start = block
        .find(&needle)
        .unwrap_or_else(|| panic!("provenance record declares {key}"))
        + needle.len();
    let rest = &block[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].trim().trim_matches('"').to_owned()
}

/// Splits the provenance record into its `[[registry]]` blocks.
fn provenance_blocks(marker: &str) -> Vec<String> {
    let text = read("evidence/fnd-05/iana/provenance.toml");
    text.split(marker)
        .skip(1)
        .map(|chunk| {
            let end = chunk.find("\n[").unwrap_or(chunk.len());
            format!("\n{}", chunk[..end].trim_start_matches(']'))
        })
        .collect()
}

/// The pinned snapshot's own record of what it froze.
struct Pin {
    path: String,
    byte_length: usize,
    sha256: String,
    registry_updated: String,
}

fn pins() -> Vec<Pin> {
    let blocks = provenance_blocks("[[registry]]");
    assert_eq!(
        blocks.len(),
        2,
        "exactly two registries are pinned (ipv4 and ipv6); a missing one would leave that \
         family unchecked while this gate still reported green"
    );
    blocks
        .iter()
        .map(|block| Pin {
            path: field_after(block, "path"),
            byte_length: field_after(block, "byte_length")
                .parse()
                .expect("byte_length is a non-negative integer"),
            sha256: field_after(block, "sha256"),
            registry_updated: field_after(block, "registry_updated"),
        })
        .collect()
}

/// FAIL CLOSED on snapshot integrity.
///
/// If the vendored bytes do not match what `provenance.toml` says was frozen,
/// every conclusion drawn from them downstream is void, so this must fail
/// before anything else is believed.
#[test]
fn fnd_05_iana_snapshot_matches_its_pin() {
    let mut drifted = Vec::new();
    for pin in pins() {
        let bytes = std::fs::read(workspace_root().join(&pin.path))
            .unwrap_or_else(|error| panic!("{}: vendored snapshot unreadable: {error}", pin.path));
        let digest = sha256_hex(&bytes);
        if bytes.len() != pin.byte_length || digest != pin.sha256 {
            drifted.push(format!(
                "{}: pinned {} bytes / {}, found {} bytes / {}",
                pin.path,
                pin.byte_length,
                pin.sha256,
                bytes.len(),
                digest
            ));
        }
        // The registry's own <updated> element must still be the revision the
        // pin claims. This is what separates STALENESS from TAMPERING: a
        // refreshed snapshot moves this date, an edited one does not.
        let text = read(&pin.path);
        let marker = format!("<updated>{}</updated>", pin.registry_updated);
        assert!(
            text.contains(&marker),
            "{}: the pin records registry_updated={} but the vendored document does not contain \
             that <updated> element. The snapshot and its provenance disagree about WHICH \
             registry revision was frozen.",
            pin.path,
            pin.registry_updated
        );
    }
    assert!(
        drifted.is_empty(),
        "{} vendored IANA snapshot(s) do not match their pin. Either the files were edited or \
         the pin was not updated with them. Do NOT refresh the pin to clear this — see \
         provenance.toml `prohibition`:\n{}",
        drifted.len(),
        drifted.join("\n"),
    );
}

// ---------------------------------------------------------------------------
// The classifier ratchet
// ---------------------------------------------------------------------------

/// Mirrors the shipped classifier's match arms exactly.
///
/// Deliberately a reimplementation rather than a call: `is_public_guarded_ip`
/// is private, and driving the real one requires a fetch, which this offline
/// gate must not do. `fnd_05_destination_classifier.rs` binds the SHIPPED
/// function through the public surface for 22 addresses; this mirror exists so
/// the whole registry can be swept offline. If the two ever disagree, that
/// file is the authority and this mirror is the bug.
///
/// The registry-wide proof against the SHIPPED classifier now lives in
/// `fnd_05_iana_fence_sweep.rs`, which drives every global=False prefix
/// through the public fetch path. That retires #2647's objection that this
/// mirror and the declared gap list were the only registry-wide check, and
/// both were written by the same author.
fn mirror_denies(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(a) => {
            let [x, b, c, _] = a.octets();
            matches!(
                (x, b, c),
                (0 | 10 | 127 | 224..=255, _, _)
                    | (100, 64..=127, _)
                    | (169, 254, _)
                    | (172, 16..=31, _)
                    | (192, 0 | 2 | 168, _)
                    | (192, 88, 99)
                    | (198, 18..=19, _)
                    | (198, 51, 100)
                    | (203, 0, 113)
            )
        }
        IpAddr::V6(a) => {
            if a.is_unspecified() || a.is_loopback() || a.is_multicast() {
                return true;
            }
            let s = a.segments();
            // The four arms bd-fnd-05-implementation-b-ysez adds, in the same
            // separate-block form as the shipped classifier.
            if matches!(
                s,
                [0x0100, 0, 0, 1, ..]
                    | [0x2001, 0x0000..=0x01FF, ..]
                    | [0x3FFF, 0x0000..=0x0FFF, ..]
                    | [0x5F00, ..]
            ) {
                return true;
            }
            matches!(s,
                [0x0000 | 0x2002 | 0xFC00..=0xFDFF | 0xFE80..=0xFEBF, ..]
                | [0x0064, 0xFF9B, 0, 0, 0, 0, ..]
                | [0x0064, 0xFF9B, 0x0001, ..]
                | [0x0100, 0, 0, 0, ..]
                | [0x2001, 0 | 0x0DB8, ..])
        }
    }
}

/// Every prefix the vendored registries mark globally UNREACHABLE, probed at
/// its FIRST and LAST address.
///
/// The flag is read by its LEADING TOKEN, the text before any child element,
/// so a footnoted `False <xref .../>` counts. An exact match on the whole
/// element skipped 127.0.0.0/8, 2001::/23 and fc00::/7, and an unparsed
/// `<xref>` after 192.0.0.0/24 dropped that prefix too (#5559, #5562). A prefix
/// that cannot be parsed now panics rather than being skipped, and the caller
/// asserts the exact count.
fn globally_unreachable_probes() -> Vec<(String, IpAddr)> {
    let leading_token = |raw: &str| raw.split('<').next().unwrap_or("").trim().to_owned();
    let mut out = Vec::new();
    for (relative, v6) in [
        ("evidence/fnd-05/iana/iana-ipv4-special-registry.xml", false),
        ("evidence/fnd-05/iana/iana-ipv6-special-registry.xml", true),
    ] {
        let text = read(relative);
        for record in text.split("<record").skip(1) {
            let record = record.split("</record>").next().unwrap_or("");
            let element = |tag: &str| -> Option<String> {
                let open = format!("<{tag}>");
                let close = format!("</{tag}>");
                let start = record.find(&open)? + open.len();
                let end = record[start..].find(&close)? + start;
                Some(record[start..end].trim().to_owned())
            };
            if element("global").map(|raw| leading_token(&raw)).as_deref() != Some("False") {
                continue;
            }
            let addresses = element("address")
                .unwrap_or_else(|| panic!("{relative}: a global=False record has no <address>"));
            for part in leading_token(&addresses).split(',') {
                let part = part.trim();
                let (network, bits) = part
                    .split_once('/')
                    .unwrap_or_else(|| panic!("{relative}: {part} is not a CIDR prefix"));
                let bits: u32 = bits
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| panic!("{relative}: {part} has no integer length"));
                let (first, last) = if v6 {
                    let base = u128::from(
                        network
                            .parse::<std::net::Ipv6Addr>()
                            .unwrap_or_else(|_| panic!("{relative}: {part} network")),
                    );
                    let host = u128::MAX.checked_shr(bits).unwrap_or(0);
                    (
                        IpAddr::V6(std::net::Ipv6Addr::from(base)),
                        IpAddr::V6(std::net::Ipv6Addr::from(base | host)),
                    )
                } else {
                    let base = u32::from(
                        network
                            .parse::<std::net::Ipv4Addr>()
                            .unwrap_or_else(|_| panic!("{relative}: {part} network")),
                    );
                    let host = u32::MAX.checked_shr(bits).unwrap_or(0);
                    (
                        IpAddr::V4(std::net::Ipv4Addr::from(base)),
                        IpAddr::V4(std::net::Ipv4Addr::from(base | host)),
                    )
                };
                out.push((part.to_owned(), first));
                out.push((part.to_owned(), last));
            }
        }
    }
    out
}

/// The gaps recorded in `provenance.toml` as unmet acceptance.
fn declared_gaps() -> BTreeSet<String> {
    provenance_blocks("[[known_uncloseable_gap]]")
        .iter()
        .map(|block| field_after(block, "prefix"))
        .collect()
}

/// THE RATCHET. Fails on any change in either direction.
///
/// The set of globally-unreachable prefixes the classifier admits must equal
/// the set `provenance.toml` declares as unmet acceptance — EXACTLY.
///
/// - a NEW IANA special-purpose range the classifier misses -> extra entry -> RED
/// - a classifier REGRESSION opening an existing range       -> extra entry -> RED
/// - a classifier FIX closing a declared gap                  -> missing entry -> RED
///
/// The last case failing is intentional, not an oversight. Closing a gap must
/// be accompanied by removing it here, so the recorded state can never silently
/// disagree with reality in either direction.
///
/// A declared gap is UNMET ACCEPTANCE, never an approved exception. Adding an
/// entry to silence a new failure is the RH-3 reflex in a security control and
/// is prohibited by `provenance.toml`.
#[test]
fn fnd_05_iana_classifier_gap_set_is_exactly_as_declared() {
    let probes = globally_unreachable_probes();
    // EXACT, not a floor: a floor passed while 4 of 33 prefixes were silently
    // skipped (#5559, #5562). Bound to registry blobs eda3cd1f (ipv4) and
    // 08da69b6 (ipv6); a reviewed refresh re-freezes these numbers.
    let prefixes: BTreeSet<&String> = probes.iter().map(|(prefix, _)| prefix).collect();
    assert_eq!(
        (prefixes.len(), probes.len()),
        (33, 66),
        "the vendored registries hold 20 ipv4 + 13 ipv6 = 33 global=False prefixes, each probed \
         at its first and last address; a different count means a partial parse or a changed \
         registry, and either must be reviewed rather than accepted"
    );

    let observed: BTreeSet<String> = probes
        .iter()
        .filter(|(_, probe)| !mirror_denies(*probe))
        .map(|(prefix, _)| prefix.clone())
        .collect();
    let declared = declared_gaps();

    let unexpected: Vec<&String> = observed.difference(&declared).collect();
    let closed: Vec<&String> = declared.difference(&observed).collect();

    assert!(
        unexpected.is_empty(),
        "THE ADDRESS FENCE IS OPEN ON {} PREFIX(ES) NOT DECLARED AS KNOWN GAPS.\n\nIANA marks \
         these globally unreachable and the classifier admits them, so the guarded fetcher will \
         connect to them. This is a FINDING: report it. Do NOT add these to \
         known_uncloseable_gap to make this test green — that is the regeneration reflex in a \
         security control.\n\n{:#?}",
        unexpected.len(),
        unexpected,
    );

    assert!(
        closed.is_empty(),
        "{} declared gap(s) are no longer open — the classifier now denies them. This is GOOD \
         NEWS and an intentional failure: remove them from known_uncloseable_gap in \
         provenance.toml so the recorded state matches reality.\n\n{:#?}",
        closed.len(),
        closed,
    );
}
