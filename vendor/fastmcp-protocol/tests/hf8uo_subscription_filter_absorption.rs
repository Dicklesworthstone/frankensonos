//! bd-hf8uo: a misspelled `SubscriptionFilter` key must be OBSERVABLE.
//!
//! The authoritative 2026-07-28 schema declares `SubscriptionFilter` with no
//! `additionalProperties`, and no definition anywhere in that schema is
//! closed, so unknown members are permitted and absorption must continue to
//! work. The defect was never that absorption happens; it is that it happened
//! SILENTLY — a misspelled canonical key left the typed field `None` and
//! narrowed a subscription to nothing with no party in error.
//!
//! These tests live in `tests/`, a separate compilation unit that links the
//! crate as an ordinary dependent and cannot see `#[cfg(test)]` items, so they
//! prove the shipped public surface rather than an internal one.

use fastmcp_protocol::SubscriptionFilter;

/// The canonical spelling and the misspelling observed in the wild. The two
/// test bodies differ in exactly this one token and nothing else.
const CANONICAL_KEY: &str = "resourceSubscriptions";
const MISSPELLED_KEY: &str = "resources";

fn filter_from(key: &str) -> SubscriptionFilter {
    let wire = format!(r#"{{"{key}":["file:///a.txt"],"toolsListChanged":true}}"#);
    serde_json::from_str(&wire).expect("the schema is open, so both spellings must deserialize")
}

#[test]
fn hf8uo_subscription_filter_positive() {
    let filter = filter_from(CANONICAL_KEY);

    // The canonical key populates the typed field.
    assert_eq!(
        filter.resource_subscriptions.as_deref(),
        Some(["file:///a.txt".to_owned()].as_slice()),
        "the canonical key must reach the typed field"
    );
    // The sibling field, identical in both bodies, is unaffected.
    assert_eq!(filter.tools_list_changed, Some(true));

    // Nothing was absorbed, so nothing is suspected.
    assert!(
        filter.additional.is_empty(),
        "a fully canonical payload must leave no residue, found {:?}",
        filter.additional
    );
    assert_eq!(
        filter.suspected_key_typos(),
        Vec::new(),
        "a fully canonical payload must raise no suspicion"
    );
}

#[test]
fn hf8uo_subscription_filter_planted_negative() {
    // Differs from the positive in ONE token: the key's spelling.
    let filter = filter_from(MISSPELLED_KEY);

    // Absorption still happens and must: the schema permits unknown members.
    // This is the silent half of the defect, asserted rather than assumed.
    assert_eq!(
        filter.resource_subscriptions, None,
        "the misspelled key must NOT populate the typed field"
    );
    assert!(
        filter.additional.contains_key(MISSPELLED_KEY),
        "the misspelled key must still be retained, not rejected"
    );
    // The one-variable guarantee: the untouched sibling still parses.
    assert_eq!(filter.tools_list_changed, Some(true));

    // The observable half — this is what the Bead exists to add.
    let suspected = filter.suspected_key_typos();
    assert_eq!(
        suspected.len(),
        1,
        "exactly the misspelled key must be reported, got {suspected:?}"
    );
    assert_eq!(suspected[0].received, MISSPELLED_KEY);
    assert!(
        SubscriptionFilter::CANONICAL_FIELDS.contains(&suspected[0].resembles),
        "the report must name a canonical field, got {:?}",
        suspected[0].resembles
    );
}

/// Guards the accessor against the degenerate implementation that would
/// satisfy the planted negative by reporting every absorbed key. Without this,
/// `suspected_key_typos` could simply return `additional.keys()` and pass.
#[test]
fn hf8uo_genuine_extension_is_not_reported_as_a_typo() {
    let wire = r#"{"experimentalFooBar":{"enabled":true},"toolsListChanged":true}"#;
    let filter: SubscriptionFilter = serde_json::from_str(wire).expect("extensions must parse");

    assert!(
        filter.additional.contains_key("experimentalFooBar"),
        "a genuine extension must still be retained"
    );
    assert_eq!(
        filter.suspected_key_typos(),
        Vec::new(),
        "a genuine extension must NOT be reported as a misspelling"
    );
}

/// The misspelling that motivated this Bead is thirteen edits from its
/// canonical field and differs by far more than capitalization. A matcher
/// built only from case-folding or a small edit-distance bound — the shapes
/// the Bead text describes — would miss it entirely. This pins the rule that
/// actually catches it so a later simplification cannot silently drop it.
#[test]
fn hf8uo_prefix_rule_is_required_not_decorative() {
    let folded_canonical = CANONICAL_KEY.to_lowercase();
    let folded_misspelling = MISSPELLED_KEY.to_lowercase();

    assert_ne!(
        folded_misspelling, folded_canonical,
        "case-folded equality alone cannot catch this misspelling"
    );
    assert!(
        folded_canonical.starts_with(&folded_misspelling),
        "the case-folded prefix relation is what makes this misspelling detectable"
    );
    assert!(
        filter_from(MISSPELLED_KEY).suspected_key_typos().len() == 1,
        "and the accessor must actually act on that relation"
    );
}
