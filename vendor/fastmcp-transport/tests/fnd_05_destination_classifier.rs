//! FND-05 B: the guarded-fetch destination classifier, proven from OUTSIDE.
//!
//! The package body enumerates exactly which embedded/transition ranges must be
//! DENIED rather than reinterpreted. This binds that enumerated requirement to
//! observable behaviour through the shipped public surface, so a later edit to
//! the classifier cannot silently widen the address fence.
//!
//! HOW IT REACHES THE CLASSIFIER: `is_public_guarded_ip` is private, so this
//! drives it the way a caller does — a supplied `GuardedHttpResolver` returns
//! the address under test and the fetch is required to reach the typed
//! `DisallowedResolvedAddress` refusal. That seam is public precisely so this
//! proof can exist outside the crate (bd-ho7of); before it, this file could
//! not have been written.
//!
//! WHAT THIS DOES **NOT** COVER — read before treating it as the drift gate.
//! This binds the classifier to the ENUMERATED LIST IN THE PACKAGE BODY. It
//! does NOT bind it to the live IANA special-purpose registry, because no
//! pinned registry snapshot exists in this repository. A range that IANA
//! designates as special AFTER this revision is still classified public and
//! still reached, and nothing here fails. That gap is recorded as unmet
//! acceptance on bd-fnd-05-implementation-b-ysez (comment 2528) together with
//! what closing it requires. This file narrows the exposure; it does not
//! remove it.

#![forbid(unsafe_code)]

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use asupersync::Cx;
use fastmcp_transport::http::{
    GuardedHttpFetchError, GuardedHttpFetchPolicy, GuardedHttpFetcher, GuardedHttpResolver,
    GuardedHttpsUrl,
};

/// Returns one fixed answer, so the address under test is the only variable.
struct OneAnswer(IpAddr);

impl GuardedHttpResolver for OneAnswer {
    fn resolve_all(
        &self,
        _cx: Cx,
        _host: String,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, GuardedHttpFetchError>> + Send + 'static>>
    {
        let answer = self.0;
        Box::pin(async move { Ok(vec![answer]) })
    }
}

fn policy() -> GuardedHttpFetchPolicy {
    GuardedHttpFetchPolicy::new(
        64 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(2),
        "fnd05-classifier",
    )
    .expect("a finite guarded policy is admitted")
}

/// Drives one address through the shipped fetch path and returns the outcome.
fn outcome_for(address: &str) -> Result<(), GuardedHttpFetchError> {
    let parsed: IpAddr = address.parse().expect("test address parses");
    let fetcher = GuardedHttpFetcher::with_resolver(policy(), Arc::new(OneAnswer(parsed)))
        .expect("the shipped resolver seam accepts a supplied resolver");
    let url =
        GuardedHttpsUrl::parse("https://example.invalid/probe").expect("bounded https URL parses");
    fastmcp_core::block_on(async {
        let cx = Cx::current().expect("block_on installs an ambient context");
        fetcher.fetch(&cx, &url).await.map(|_| ())
    })
}

/// True when the fence refused this exact address.
fn fence_refused(address: &str) -> bool {
    matches!(
        outcome_for(address),
        Err(GuardedHttpFetchError::DisallowedResolvedAddress(_))
    )
}

/// Every range the package body names must be DENIED, not reinterpreted.
///
/// Each entry is (address, the requirement it stands for). A representative
/// address is used per range; the classifier matches on prefix, so one member
/// exercises the arm.
const MUST_BE_DENIED: &[(&str, &str)] = &[
    // --- explicitly enumerated embedded/transition ranges ------------------
    ("::1.2.3.4", "IPv4-compatible IPv6"),
    ("64:ff9b::1.2.3.4", "NAT64 well-known 64:ff9b::/96"),
    ("64:ff9b:1::1", "NAT64 local-use 64:ff9b:1::/48"),
    ("2002:c000:0204::1", "6to4 2002::/16"),
    ("2001:0:c000:0204::1", "Teredo 2001::/32"),
    // --- IPv4-mapped must be canonicalized THEN judged by IPv4 policy ------
    ("::ffff:127.0.0.1", "IPv4-mapped loopback, via IPv4 policy"),
    ("::ffff:10.0.0.1", "IPv4-mapped private, via IPv4 policy"),
    (
        "::ffff:169.254.1.1",
        "IPv4-mapped link-local, via IPv4 policy",
    ),
    // --- ordinary non-public space ----------------------------------------
    ("127.0.0.1", "IPv4 loopback"),
    ("10.0.0.1", "IPv4 private 10/8"),
    ("172.16.0.1", "IPv4 private 172.16/12"),
    ("192.168.1.1", "IPv4 private 192.168/16"),
    ("169.254.1.1", "IPv4 link-local"),
    ("0.0.0.0", "IPv4 unspecified"),
    ("::", "IPv6 unspecified"),
    ("::1", "IPv6 loopback"),
    ("fc00::1", "IPv6 unique-local fc00::/7"),
    ("fd00::1", "IPv6 unique-local fd00::/8"),
    ("fe80::1", "IPv6 link-local fe80::/10"),
    ("ff02::1", "IPv6 multicast"),
    ("100::1", "IPv6 discard-only 0100::/64"),
    ("2001:db8::1", "IPv6 documentation 2001:db8::/32"),
];

#[test]
fn fnd_05_destination_classifier_denies_every_enumerated_range_positive() {
    let mut admitted = Vec::new();
    for (address, requirement) in MUST_BE_DENIED {
        if !fence_refused(address) {
            admitted.push(format!(
                "{address} ({requirement}) was NOT refused by the address fence"
            ));
        }
    }
    assert!(
        admitted.is_empty(),
        "{} of {} non-public addresses reached the connect path. The guarded fetcher's whole \
         purpose is to refuse these, so each line below is a widened address fence:\n{}",
        admitted.len(),
        MUST_BE_DENIED.len(),
        admitted.join("\n"),
    );
}

/// PLANTED NEGATIVE / DISCRIMINATOR: the fence must not simply refuse
/// everything.
///
/// Without this, the positive above would pass just as happily against a
/// classifier that rejected every address — which would be useless and would
/// also be indistinguishable from a correct one. A genuinely public address
/// must get PAST the fence. It then fails later for an unrelated reason (no
/// route on a build worker), and that difference is the discriminator: a
/// different typed error, never `DisallowedResolvedAddress`.
#[test]
fn fnd_05_destination_classifier_admits_public_space_planted_negative() {
    // Documentation-reserved but ordinary *public* unicast space per the
    // classifier's policy: not loopback, private, link-local, multicast, or any
    // enumerated embedded/transition range.
    const PUBLIC: &str = "93.184.216.34";

    let outcome = outcome_for(PUBLIC);
    assert!(
        !matches!(
            outcome,
            Err(GuardedHttpFetchError::DisallowedResolvedAddress(_))
        ),
        "a public address must pass the address fence; refusing it means the classifier denies \
         everything, which would make the positive above vacuous. Got {outcome:?}"
    );

    // One-variable contrast: the SAME shape of call with one octet moved into
    // private space is refused. Only the address differs.
    const PRIVATE: &str = "10.184.216.34";
    assert!(
        fence_refused(PRIVATE),
        "the private counterpart of the public probe must be refused, proving the outcome \
         tracks the ADDRESS and not the call shape"
    );
}
