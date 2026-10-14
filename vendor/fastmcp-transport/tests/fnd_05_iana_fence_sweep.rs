//! FND-05 B (bd-fnd-05-implementation-b-ysez, bars Y1-Y9): the pinned IANA
//! special-purpose registries, swept through the SHIPPED address fence.
//!
//! The fence's policy is global reachability. Every prefix the pinned
//! 2025-10-09 registries mark `global=False` must be refused, and this file
//! proves that through the public surface. It drives the real classifier via
//! `GuardedHttpFetcher::with_resolver` (the verified bd-ho7of seam), never
//! through a reimplementation of its match arms. That closes the gap recorded
//! in #2647, where the only registry-wide check compared a mirror against a list
//! written by the same author.
//!
//! OFFLINE AND SOCKET-FREE. The resolver answers `[candidate, SENTINEL]`, where
//! SENTINEL is the always-denied 10.0.0.1. The fence classifies answers in
//! order and refuses at the first non-public one, so a refusal naming the
//! candidate means DENIED and a refusal naming the sentinel means ADMITTED.
//! Either way the fetch stops before any connection. Any other outcome is an
//! instrument failure, never a verdict. A calibration pair runs in every sweep,
//! so a change to the in-order semantics turns the sweep red instead of
//! silently inverting it.
//!
//! WHAT IS NOT CLAIMED (Y9): interior coverage beyond each prefix's first and
//! last address (the arms are prefix matches, a static argument); currency
//! against the live registry (refresh stays manual per provenance.toml);
//! registry conformance for ::ffff:0:0/96 (the fence canonicalizes a mapped
//! address and applies the IPv4 policy, so a mapped PUBLIC address is admitted
//! by design); and any narrowing of the declared over-denials.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asupersync::Cx;
use fastmcp_transport::{
    GuardedHttpFetchError, GuardedHttpFetchPolicy, GuardedHttpFetcher, GuardedHttpResolver,
    GuardedHttpsUrl,
};

/// Always denied by the fence (10/8), so a fetch can never reach a socket.
const SENTINEL: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

const IPV4_REGISTRY: &str = "evidence/fnd-05/iana/iana-ipv4-special-registry.xml";
const IPV6_REGISTRY: &str = "evidence/fnd-05/iana/iana-ipv6-special-registry.xml";
const PROVENANCE: &str = "evidence/fnd-05/iana/provenance.toml";

/// The four deny arms bd-fnd-05-implementation-b-ysez adds (Y1).
const NEW_ARMS: [&str; 4] = ["100:0:0:1::/64", "2001::/23", "3fff::/20", "5f00::/16"];

/// The global=True prefixes the fence denies, as Y4(a) and Y5(i) fix them.
const EXPECTED_OVER_DENIALS: [&str; 10] = [
    "192.0.0.9/32",
    "192.0.0.10/32",
    "64:ff9b::/96",
    "2001:1::1/128",
    "2001:1::2/128",
    "2001:1::3/128",
    "2001:3::/32",
    "2001:4:112::/48",
    "2001:20::/28",
    "2001:30::/28",
];

/// The global=True prefixes the fence must still admit (Y4(a)).
const EXPECTED_REACHABLE_ADMITTED: [&str; 4] = [
    "192.31.196.0/24",
    "192.52.193.0/24",
    "192.175.48.0/24",
    "2620:4f:8000::/48",
];

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

// ---------------------------------------------------------------------------
// The admission instrument (Y3)
// ---------------------------------------------------------------------------

/// Answers `[candidate, SENTINEL]` and counts how often it was consulted.
struct PairResolver {
    candidate: IpAddr,
    calls: Arc<AtomicUsize>,
}

impl GuardedHttpResolver for PairResolver {
    fn resolve_all(
        &self,
        _cx: Cx,
        _host: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answers = vec![self.candidate, SENTINEL];
        Box::pin(async move { Ok(answers) })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Denied,
    Admitted,
}

fn policy() -> GuardedHttpFetchPolicy {
    GuardedHttpFetchPolicy::new(
        64 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(2),
        "fnd05-iana-sweep",
    )
    .expect("a finite guarded policy is admitted")
}

/// The address the fence actually classifies. Its own rule: an IPv4-mapped
/// IPv6 address is canonicalized to IPv4 first.
fn canonical(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    }
}

/// One offline pass through the shipped fence. Panics on anything that is not
/// exactly one of the two verdicts, so an instrument failure can never be read
/// as either answer.
fn verdict(candidate: IpAddr) -> Verdict {
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(PairResolver {
        candidate,
        calls: Arc::clone(&calls),
    });
    let fetcher =
        GuardedHttpFetcher::with_resolver(policy(), resolver).expect("verified resolver seam");
    let before = (fetcher.policy().clone(), fetcher.root_identity().to_owned());
    let url = GuardedHttpsUrl::parse("https://example.invalid/iana-sweep")
        .expect("bounded https URL parses");
    let outcome = fastmcp_core::block_on(async {
        let cx = Cx::current().expect("block_on installs an ambient context");
        fetcher.fetch(&cx, &url).await.map(|_| ())
    });
    let after = (fetcher.policy().clone(), fetcher.root_identity().to_owned());
    assert_eq!(
        before, after,
        "{candidate}: the fetcher's public policy or root identity changed across a refusal"
    );
    let consulted = calls.load(Ordering::SeqCst);
    assert_eq!(
        consulted, 1,
        "INSTRUMENT FAILURE: {candidate} consulted the resolver {consulted} times, not exactly once"
    );
    let classified = canonical(candidate);
    match outcome {
        Err(GuardedHttpFetchError::DisallowedResolvedAddress(refused)) if refused == classified => {
            Verdict::Denied
        }
        Err(GuardedHttpFetchError::DisallowedResolvedAddress(refused)) if refused == SENTINEL => {
            Verdict::Admitted
        }
        other => panic!(
            "INSTRUMENT FAILURE: {candidate} (classified as {classified}) produced {other:?}, \
             which is neither verdict"
        ),
    }
}

fn address(text: &str) -> IpAddr {
    text.parse()
        .unwrap_or_else(|_| panic!("{text} is an address"))
}

/// Proves the instrument can say both things, in the same run as any sweep.
fn calibrate() {
    assert_eq!(
        verdict(address("93.184.216.34")),
        Verdict::Admitted,
        "calibration: a known-public address must read ADMITTED"
    );
    assert_eq!(
        verdict(address("10.184.216.34")),
        Verdict::Denied,
        "calibration: its one-octet private counterpart must read DENIED"
    );
}

// ---------------------------------------------------------------------------
// The pinned registries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    Ipv4,
    Ipv6,
}

/// One prefix of one registry record, with the record's `<global>` flag.
struct RegistryRow {
    family: Family,
    prefix: String,
    global_token: String,
}

/// The text before any child element, trimmed. `False <xref .../>` reads
/// `False`, so footnoted records are not skipped (#5562).
fn leading_token(raw: &str) -> String {
    raw.split('<').next().unwrap_or("").trim().to_owned()
}

fn element<'a>(record: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = record.find(&open)? + open.len();
    let end = record[start..].find(&close)? + start;
    Some(&record[start..end])
}

/// Every comma-separated prefix of every record, derived at test time from
/// the pinned XML, never from a hand-written list.
fn registry_rows() -> Vec<RegistryRow> {
    let mut rows = Vec::new();
    for (family, relative) in [(Family::Ipv4, IPV4_REGISTRY), (Family::Ipv6, IPV6_REGISTRY)] {
        let text = read(relative);
        for record in text.split("<record").skip(1) {
            let record = record.split("</record>").next().unwrap_or("");
            let global_token = element(record, "global").map_or_else(String::new, leading_token);
            let addresses = element(record, "address")
                .unwrap_or_else(|| panic!("{relative}: a <record> has no <address>"));
            for prefix in leading_token(addresses).split(',') {
                rows.push(RegistryRow {
                    family,
                    prefix: prefix.trim().to_owned(),
                    global_token: global_token.clone(),
                });
            }
        }
    }
    rows
}

/// The first and last address of a CIDR prefix. A misaligned network address
/// is a parse fault and panics.
fn first_and_last(family: Family, prefix: &str) -> (IpAddr, IpAddr) {
    let (network, bits) = prefix
        .split_once('/')
        .unwrap_or_else(|| panic!("{prefix}: not a CIDR prefix"));
    let bits: u32 = bits
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{prefix}: prefix length is not an integer"));
    match family {
        Family::Ipv4 => {
            assert!(bits <= 32, "{prefix}: IPv4 prefix length exceeds 32");
            let base = u32::from(
                network
                    .parse::<Ipv4Addr>()
                    .unwrap_or_else(|_| panic!("{prefix}: IPv4 network")),
            );
            let host = u32::MAX.checked_shr(bits).unwrap_or(0);
            assert_eq!(base & host, 0, "{prefix}: network address is not aligned");
            (
                IpAddr::V4(Ipv4Addr::from(base)),
                IpAddr::V4(Ipv4Addr::from(base | host)),
            )
        }
        Family::Ipv6 => {
            assert!(bits <= 128, "{prefix}: IPv6 prefix length exceeds 128");
            let base = u128::from(
                network
                    .parse::<Ipv6Addr>()
                    .unwrap_or_else(|_| panic!("{prefix}: IPv6 network")),
            );
            let host = u128::MAX.checked_shr(bits).unwrap_or(0);
            assert_eq!(base & host, 0, "{prefix}: network address is not aligned");
            (
                IpAddr::V6(Ipv6Addr::from(base)),
                IpAddr::V6(Ipv6Addr::from(base | host)),
            )
        }
    }
}

fn successor(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(
            u32::from(v4).checked_add(1).expect("successor exists"),
        )),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(
            u128::from(v6).checked_add(1).expect("successor exists"),
        )),
    }
}

fn predecessor(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(
            u32::from(v4).checked_sub(1).expect("predecessor exists"),
        )),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(
            u128::from(v6).checked_sub(1).expect("predecessor exists"),
        )),
    }
}

/// The `prefix` of every `[[declared_over_denial]]` block in provenance.toml.
/// Each block must carry all four fields, non-empty.
fn declared_over_denials() -> BTreeSet<String> {
    let text = read(PROVENANCE);
    let mut declared = BTreeSet::new();
    for chunk in text.split("[[declared_over_denial]]").skip(1) {
        let end = chunk.find("\n[").unwrap_or(chunk.len());
        let block = &chunk[..end];
        let field = |key: &str| -> String {
            let needle = format!("\n{key} = ");
            let start = block
                .find(&needle)
                .unwrap_or_else(|| panic!("a [[declared_over_denial]] block lacks {key}"))
                + needle.len();
            let rest = &block[start..];
            let value = rest[..rest.find('\n').unwrap_or(rest.len())]
                .trim()
                .trim_matches('"')
                .to_owned();
            assert!(
                !value.is_empty(),
                "a [[declared_over_denial]] {key} is empty"
            );
            value
        };
        for key in ["family", "name", "reason"] {
            field(key);
        }
        assert!(
            declared.insert(field("prefix")),
            "a prefix is declared as an over-denial twice"
        );
    }
    declared
}

// ---------------------------------------------------------------------------
// The sweep (Y4(a), Y4(b))
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SweepReport {
    unreachable_prefixes: BTreeMap<Family, usize>,
    probes: usize,
    admitted_unreachable: Vec<(String, IpAddr)>,
    reachable_denied: BTreeSet<String>,
    reachable_admitted: BTreeSet<String>,
    token_counts: BTreeMap<(Family, String), usize>,
}

/// THE ONE DRIVER both sweep tests use. `moved` names the single prefix whose
/// LAST probe is moved one address past the prefix. That is the planted
/// negative's only difference from the positive.
fn sweep(moved: Option<&str>) -> SweepReport {
    calibrate();
    let mut report = SweepReport::default();
    let mut moved_found = false;
    for row in registry_rows() {
        *report
            .token_counts
            .entry((row.family, row.global_token.clone()))
            .or_default() += 1;
        let (first, last) = first_and_last(row.family, &row.prefix);
        match row.global_token.as_str() {
            "False" => {
                *report.unreachable_prefixes.entry(row.family).or_default() += 1;
                let last = if moved == Some(row.prefix.as_str()) {
                    moved_found = true;
                    successor(last)
                } else {
                    last
                };
                for probe in [first, last] {
                    report.probes += 1;
                    if verdict(probe) == Verdict::Admitted {
                        report
                            .admitted_unreachable
                            .push((row.prefix.clone(), probe));
                    }
                }
            }
            "True" => match (verdict(first), verdict(last)) {
                (Verdict::Denied, Verdict::Denied) => {
                    report.reachable_denied.insert(row.prefix);
                }
                (Verdict::Admitted, Verdict::Admitted) => {
                    report.reachable_admitted.insert(row.prefix);
                }
                mixed => panic!(
                    "{}: global=True prefix read {mixed:?} at its first and last address; a \
                     partly denied prefix is neither declared state",
                    row.prefix
                ),
            },
            // N/A and empty flags are counted, not swept (Y4(a)).
            _ => {}
        }
    }
    if let Some(prefix) = moved {
        assert!(
            moved_found,
            "{prefix} is not a global=False prefix of the pinned registries"
        );
    }
    report
}

fn expected_token_counts() -> BTreeMap<(Family, String), usize> {
    [
        ((Family::Ipv4, "False"), 20),
        ((Family::Ipv4, "True"), 5),
        ((Family::Ipv4, ""), 1),
        ((Family::Ipv6, "False"), 13),
        ((Family::Ipv6, "True"), 9),
        ((Family::Ipv6, "N/A"), 2),
        ((Family::Ipv6, ""), 1),
    ]
    .into_iter()
    .map(|((family, token), count)| ((family, token.to_owned()), count))
    .collect()
}

fn owned_set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Y4(a) POSITIVE: every global=False prefix of the pinned registries is
/// refused at its first and last address, through the shipped fence. The
/// global=True prefixes it refuses are exactly the declared over-denials.
#[test]
fn fnd_05_iana_fence_sweep_positive() {
    let report = sweep(None);

    // EXACT parse control, by equality, bound to registry blobs eda3cd1f
    // (ipv4) and 08da69b6 (ipv6). A partial parse cannot pass.
    assert_eq!(
        report.unreachable_prefixes,
        BTreeMap::from([(Family::Ipv4, 20), (Family::Ipv6, 13)]),
        "the pinned registries hold 20 ipv4 + 13 ipv6 = 33 global=False prefixes"
    );
    assert_eq!(report.probes, 66, "33 prefixes x first and last address");
    assert_eq!(
        report.token_counts,
        expected_token_counts(),
        "every <global> leading token is counted, including N/A and empty"
    );

    assert!(
        report.admitted_unreachable.is_empty(),
        "THE ADDRESS FENCE ADMITS {} PROBE(S) IANA MARKS GLOBALLY UNREACHABLE:\n{:#?}",
        report.admitted_unreachable.len(),
        report.admitted_unreachable
    );

    let declared = declared_over_denials();
    assert_eq!(
        declared,
        owned_set(&EXPECTED_OVER_DENIALS),
        "provenance.toml declares exactly the over-denials Y5(i) fixes"
    );
    assert_eq!(
        report.reachable_denied, declared,
        "the global=True prefixes the fence denies must equal the declared over-denials: an \
         undeclared denial or a declared-but-admitted prefix is red"
    );
    assert_eq!(
        report.reachable_admitted,
        owned_set(&EXPECTED_REACHABLE_ADMITTED),
        "the remaining global=True prefixes are admitted"
    );
}

/// Y4(b) PLANTED NEGATIVE: the same driver over the same records, with exactly
/// one probe moved one address past its prefix (3fff::/20's last address +1 is
/// 3fff:1000::). The sweep reports exactly that probe as admitted, names its
/// prefix, and still reads the other 65 as denied.
#[test]
fn fnd_05_iana_fence_sweep_planted_negative() {
    let report = sweep(Some("3fff::/20"));

    assert_eq!(report.probes, 66);
    assert_eq!(
        report.admitted_unreachable,
        vec![("3fff::/20".to_owned(), address("3fff:1000::"))],
        "exactly the moved probe is admitted, and the report names its prefix"
    );
}

// ---------------------------------------------------------------------------
// The new arms' edges (Y4(c), Y4(d))
// ---------------------------------------------------------------------------

/// Y4(c) POSITIVE: the first and last address inside each new arm are denied.
#[test]
fn fnd_05_iana_fence_boundaries_positive() {
    calibrate();
    for arm in NEW_ARMS {
        let (first, last) = first_and_last(Family::Ipv6, arm);
        for inside in [first, last] {
            assert_eq!(
                verdict(inside),
                Verdict::Denied,
                "{inside} lies inside the new arm {arm} and must be denied"
            );
        }
    }
}

/// Y4(d) PLANTED NEGATIVE: the address one step OUTSIDE each edge of each new
/// arm is admitted, which proves no arm is wider than its registry prefix.
#[test]
fn fnd_05_iana_fence_boundaries_planted_negative() {
    calibrate();

    // The before-edge of 100:0:0:1::/64 is 100::ffff:ffff:ffff:ffff, which
    // lies inside the already-denied 100::/64. It is excluded by name, and
    // its verdict is asserted so the exclusion is visible, not silent.
    let excluded = address("100::ffff:ffff:ffff:ffff");
    assert_eq!(
        verdict(excluded),
        Verdict::Denied,
        "the excluded before-edge is covered by the pre-existing 100::/64 arm"
    );

    let mut outside = Vec::new();
    for arm in NEW_ARMS {
        let (first, last) = first_and_last(Family::Ipv6, arm);
        let before = predecessor(first);
        if before != excluded {
            outside.push(before);
        }
        outside.push(successor(last));
    }
    let expected: Vec<IpAddr> = [
        "100:0:0:2::",
        "2000:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        "2001:200::",
        "3ffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        "3fff:1000::",
        "5eff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        "5f01::",
    ]
    .into_iter()
    .map(address)
    .collect();
    assert_eq!(
        outside, expected,
        "the computed outside edges equal the bar's list"
    );
    for edge in outside {
        assert_eq!(
            verdict(edge),
            Verdict::Admitted,
            "{edge} lies just outside a new arm and must be admitted"
        );
    }
}
