//! Frozen FND-03 A public protocol-identity, policy, and receipt harnesses.
//!
//! External consumer of `fastmcp_protocol::protocol_policy`, mirroring the
//! shape of the sibling `fnd_03_b.rs`. Nothing here reaches crate internals:
//! no `use super::`, no `pub(crate)` item, no `cfg(test)` module.
//!
//! # Scope boundary: what the shipped surface can and cannot prove
//!
//! The sealed legacy-receipt machinery is deliberately unforgeable from
//! outside this crate. `LegacyReceiptBinding::new` is `pub(crate)`
//! (`protocol_policy.rs:365`), `LegacyAdapterReceiptIssuer` is sealed, and it
//! has **zero implementers anywhere in the workspace** — its doc comment names
//! the future LEG-02 and LEG-03 installers as the only intended ones, and
//! neither exists yet. So no shipped code path issues a receipt today.
//!
//! The consequence for this target is precise, and is stated rather than
//! worked around:
//!
//! * **Provable here, and proved below:** that a legacy-capable policy is
//!   *refused* without a receipt, with the typed `FeatureUnavailable` raised
//!   before any connect or bind side effect. That is the externally visible
//!   half of receipt-boundedness, and it is the half that actually protects
//!   the era split.
//! * **Not provable here:** the receipt-bound *positive* — constructing a
//!   receipt and observing that the selection carries its bound policy —
//!   because nothing public can construct one. That remains covered in-crate
//!   until an installer ships, and is recorded as a finding on the owning
//!   Bead rather than papered over by widening the API to host a test.
//!
//! Exactly two `#[test]` functions live in this file, carrying the frozen IDs
//! and nothing else.

use fastmcp_protocol::protocol_policy::{
    LEGACY_PROTOCOL_VERSION, MODERN_PROTOCOL_VERSION, ProtocolEra, ProtocolPolicy,
    ProtocolPolicyError, ProtocolRole, ProtocolVersion, ProtocolVersionError,
};

/// The syntactically valid but unsupported revision. It is a negative input
/// only and can never satisfy a 2026 or exact-2024 positive.
const UNSUPPORTED_VERSION: &str = "2025-11-25";

#[test]
fn fnd_03_a_positive() {
    // ---------------------------------------------------------------------
    // Protocol identity: exactly two supported revisions, no third.
    // ---------------------------------------------------------------------
    assert_eq!(MODERN_PROTOCOL_VERSION, "2026-07-28");
    assert_eq!(LEGACY_PROTOCOL_VERSION, "2024-11-05");

    assert_eq!(
        ProtocolVersion::parse(MODERN_PROTOCOL_VERSION),
        Ok(ProtocolVersion::MODERN_2026)
    );
    assert_eq!(
        ProtocolVersion::parse(LEGACY_PROTOCOL_VERSION),
        Ok(ProtocolVersion::LEGACY_2024)
    );

    // Identity round-trips in both directions for both eras, so neither can
    // silently alias to the other.
    for (version, era, spelling) in [
        (
            ProtocolVersion::MODERN_2026,
            ProtocolEra::Modern2026,
            MODERN_PROTOCOL_VERSION,
        ),
        (
            ProtocolVersion::LEGACY_2024,
            ProtocolEra::Legacy2024,
            LEGACY_PROTOCOL_VERSION,
        ),
    ] {
        assert_eq!(version.era(), era);
        assert_eq!(era.version(), version);
        assert_eq!(version.as_str(), spelling);
        assert_eq!(ProtocolVersion::parse(spelling), Ok(version));
    }
    assert_ne!(ProtocolVersion::MODERN_2026, ProtocolVersion::LEGACY_2024);

    // ---------------------------------------------------------------------
    // Policy: Auto is the default and the alternatives are exactly two.
    // ---------------------------------------------------------------------
    assert_eq!(ProtocolPolicy::default(), ProtocolPolicy::Auto);

    assert_eq!(
        ProtocolPolicy::Auto.supported_versions(),
        [ProtocolVersion::MODERN_2026, ProtocolVersion::LEGACY_2024].as_slice()
    );
    assert_eq!(
        ProtocolPolicy::ModernOnly.supported_versions(),
        [ProtocolVersion::MODERN_2026].as_slice()
    );
    assert_eq!(
        ProtocolPolicy::LegacyOnly.supported_versions(),
        [ProtocolVersion::LEGACY_2024].as_slice()
    );

    assert!(ProtocolPolicy::Auto.permits(ProtocolVersion::MODERN_2026));
    assert!(ProtocolPolicy::Auto.permits(ProtocolVersion::LEGACY_2024));
    assert!(ProtocolPolicy::ModernOnly.permits(ProtocolVersion::MODERN_2026));
    assert!(!ProtocolPolicy::ModernOnly.permits(ProtocolVersion::LEGACY_2024));
    assert!(ProtocolPolicy::LegacyOnly.permits(ProtocolVersion::LEGACY_2024));
    assert!(!ProtocolPolicy::LegacyOnly.permits(ProtocolVersion::MODERN_2026));

    // Only ModernOnly is free of the legacy adapter requirement.
    assert!(!ProtocolPolicy::ModernOnly.requires_legacy_adapter());
    assert!(ProtocolPolicy::Auto.requires_legacy_adapter());
    assert!(ProtocolPolicy::LegacyOnly.requires_legacy_adapter());

    // ---------------------------------------------------------------------
    // Selection: immutable, and carries the role it was validated for.
    // ---------------------------------------------------------------------
    let client = ProtocolPolicy::ModernOnly
        .validate_for_client(None)
        .expect("modern-only needs no legacy receipt");
    assert_eq!(client.policy(), ProtocolPolicy::ModernOnly);
    assert_eq!(client.role(), ProtocolRole::Client);

    let server = ProtocolPolicy::ModernOnly
        .validate_for_server(None)
        .expect("modern-only needs no legacy receipt");
    assert_eq!(server.policy(), ProtocolPolicy::ModernOnly);
    assert_eq!(server.role(), ProtocolRole::Server);

    // A selection exposes only readers, so the bound policy cannot drift after
    // validation: reading it repeatedly yields the same immutable value.
    assert_eq!(client.policy(), client.policy());
    assert_eq!(client.role(), ProtocolRole::Client);
    assert_ne!(client.role(), server.role());

    // ---------------------------------------------------------------------
    // Receipt-boundedness, externally observable half.
    //
    // Every legacy-capable policy is refused without its sealed receipt, for
    // both roles, with the typed refusal naming the rejected policy and the
    // role whose receipt was required. This is raised by validation itself,
    // before any connect or bind side effect exists to perform.
    // ---------------------------------------------------------------------
    for policy in [ProtocolPolicy::Auto, ProtocolPolicy::LegacyOnly] {
        assert_eq!(
            policy.validate_for_client(None),
            Err(ProtocolPolicyError::FeatureUnavailable {
                policy,
                role: ProtocolRole::Client,
            })
        );
        assert_eq!(
            policy.validate_for_server(None),
            Err(ProtocolPolicyError::FeatureUnavailable {
                policy,
                role: ProtocolRole::Server,
            })
        );
    }

    // The refusal is role-discriminating: the same policy refused for a client
    // and for a server are distinct typed values, so a receipt for one role
    // could never be read as satisfying the other.
    assert_ne!(
        ProtocolPolicy::LegacyOnly.validate_for_client(None),
        ProtocolPolicy::LegacyOnly.validate_for_server(None)
    );

    // Refusal does not disturb the modern lane: ModernOnly still validates.
    assert!(ProtocolPolicy::ModernOnly.validate_for_client(None).is_ok());
    assert!(ProtocolPolicy::ModernOnly.validate_for_server(None).is_ok());
}

#[test]
fn fnd_03_a_planted_negative() {
    // Accepted case, held for the byte-for-byte unchanged-state comparison.
    let accepted_input = MODERN_PROTOCOL_VERSION;
    let accepted = ProtocolVersion::parse(accepted_input).expect("the supported revision parses");
    assert_eq!(accepted, ProtocolVersion::MODERN_2026);
    assert_eq!(accepted.era(), ProtocolEra::Modern2026);
    assert_eq!(accepted.as_str(), "2026-07-28");

    // The sole changed variable is the supported-era boundary: the accepted
    // revision string becomes the syntactically valid but unsupported
    // 2025-11-25. Nothing else about the call differs.
    let planted = ProtocolVersion::parse(UNSUPPORTED_VERSION);

    assert_eq!(
        planted,
        Err(ProtocolVersionError::UnsupportedVersion {
            received: UNSUPPORTED_VERSION.to_owned(),
        }),
        "an unsupported revision must reach the typed refusal boundary"
    );

    // It is refused, not normalized: the input spelling is retained exactly,
    // and it did not alias into either supported era.
    let Err(ProtocolVersionError::UnsupportedVersion { received }) = planted else {
        panic!("the planted input must be refused");
    };
    assert_eq!(received, UNSUPPORTED_VERSION);
    assert_ne!(received, MODERN_PROTOCOL_VERSION);
    assert_ne!(received, LEGACY_PROTOCOL_VERSION);

    // No policy admits it, because no supported version equals it.
    for policy in [
        ProtocolPolicy::Auto,
        ProtocolPolicy::ModernOnly,
        ProtocolPolicy::LegacyOnly,
    ] {
        assert!(
            policy
                .supported_versions()
                .iter()
                .all(|version| version.as_str() != UNSUPPORTED_VERSION),
            "{policy:?} must not admit the unsupported revision"
        );
    }

    // Unchanged accepted state: the refusal left the supported identities,
    // the policy tables, and the selection surface byte-for-byte as they were.
    assert_eq!(ProtocolVersion::parse(accepted_input), Ok(accepted));
    assert_eq!(accepted.era(), ProtocolEra::Modern2026);
    assert_eq!(accepted.as_str(), "2026-07-28");
    assert_eq!(
        ProtocolVersion::parse(LEGACY_PROTOCOL_VERSION),
        Ok(ProtocolVersion::LEGACY_2024)
    );
    assert_eq!(ProtocolPolicy::default(), ProtocolPolicy::Auto);
    assert_eq!(
        ProtocolPolicy::Auto.supported_versions(),
        [ProtocolVersion::MODERN_2026, ProtocolVersion::LEGACY_2024].as_slice()
    );
    assert_eq!(
        ProtocolPolicy::ModernOnly
            .validate_for_client(None)
            .map(|selection| (selection.policy(), selection.role())),
        Ok((ProtocolPolicy::ModernOnly, ProtocolRole::Client))
    );
}
