//! The room-resolution order, table-tested against the scrubbed S1 + S2
//! topology fixtures and an `aliases.toml`: exact and folded names,
//! `Name@Label`, player ids, `all` / `everywhere` / `here`, aliases (single
//! and sets), unique prefixes, and suggestions for near misses.

use fsonos_core::rooms::{
    Aliases, Matched, ResolveContext, known_rooms, resolve_one, resolve_targets, suggest_rooms,
};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::{soap, topology};

const ZGS_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s1.xml");
const ZGS_S2: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s2.xml");

const ALIASES: &str = r#"[aliases]
study = "Owner’s Study"
upstairs = ["Bedroom", "Bedroom 2", "Guest's Room"]
den = "Parlor"
gone = "Garage"
loop = "upstairs"

[defaults]
room = "Den"

[defaults.clients."alice@example.com"]
room = "upstairs"
"#;

fn household(body: &str) -> HouseholdState {
    let response = soap::parse_response(body, "GetZoneGroupState").unwrap();
    let zgs =
        topology::parse_zone_group_state(response.require("ZoneGroupState").unwrap()).unwrap();
    let mut st = HouseholdState::default();
    st.apply_topology(&zgs);
    st
}

fn rooms(r: &fsonos_core::rooms::Resolution<'_>) -> Vec<String> {
    r.targets.iter().map(|t| t.room.name.clone()).collect()
}

#[test]
fn resolution_order_table() {
    let houses = [household(ZGS_S1), household(ZGS_S2)];
    let aliases = Aliases::parse(ALIASES).unwrap();
    let ctx = ResolveContext {
        aliases: Some(&aliases),
        client: None,
    };
    let alice = ResolveContext {
        aliases: Some(&aliases),
        client: Some("alice@example.com"),
    };

    let table: &[(&str, ResolveContext<'_>, Matched, &[&str])] = &[
        // A real room name wins over an alias of the same name.
        ("Den", ctx, Matched::Room, &["Den"]),
        ("den", ctx, Matched::Room, &["Den"]),
        (
            "owner's study",
            ctx,
            Matched::Room,
            &["Owner\u{2019}s Study"],
        ),
        ("Kitchen@S2", ctx, Matched::Room, &["Kitchen"]),
        (
            "rincon_000e58a0000401400",
            ctx,
            Matched::Room,
            &["Owner\u{2019}s Study"],
        ),
        ("study", ctx, Matched::Alias, &["Owner\u{2019}s Study"]),
        (
            "UPSTAIRS",
            ctx,
            Matched::Alias,
            &["Bedroom", "Bedroom 2", "Guest\u{2019}s Room"],
        ),
        (
            "all@S2",
            ctx,
            Matched::Everywhere,
            &["Lounge", "Guest\u{2019}s Room", "Parlor", "Kitchen"],
        ),
        ("here", ctx, Matched::Here, &["Den"]),
        (
            "here",
            alice,
            Matched::Here,
            &["Bedroom", "Bedroom 2", "Guest\u{2019}s Room"],
        ),
        ("par", ctx, Matched::Prefix, &["Parlor"]),
        ("Guest", ctx, Matched::Prefix, &["Guest\u{2019}s Room"]),
        // Without aliases, an alias name is just an unknown room... unless a
        // prefix catches it.
        (
            "Bedroom",
            ResolveContext::default(),
            Matched::Room,
            &["Bedroom"],
        ),
    ];
    for (query, ctx, matched, expected) in table {
        let r = resolve_targets(&houses, query, *ctx)
            .unwrap_or_else(|e| panic!("{query:?} failed: {e}"));
        assert_eq!(r.matched, *matched, "{query:?}");
        assert_eq!(rooms(&r), *expected, "{query:?}");
    }

    let everywhere = resolve_targets(&houses, "Everywhere", ctx).unwrap();
    assert_eq!(everywhere.targets.len(), 8);
    // Each target carries its own household, so a per-household command can
    // split the set.
    let s1 = everywhere
        .targets
        .iter()
        .filter(|t| std::ptr::eq(t.household, &raw const houses[0]))
        .count();
    assert_eq!(s1, 4);
}

#[test]
fn failures_are_retryable() {
    let houses = [household(ZGS_S1), household(ZGS_S2)];
    let aliases = Aliases::parse(ALIASES).unwrap();
    let ctx = ResolveContext {
        aliases: Some(&aliases),
        client: None,
    };
    let unknown = |q: &str, ctx: ResolveContext<'_>| match resolve_targets(&houses, q, ctx) {
        Err(CoreError::UnknownRoom { name, known }) => {
            assert_eq!(name, q.trim());
            assert_eq!(known.len(), 8);
        }
        other => panic!("{q:?}: expected UnknownRoom, got {other:?}"),
    };
    // Too short to be a prefix; near misses never act; an alias to an offline
    // room or to another alias resolves to nothing; `here` needs a default.
    unknown("ki", ctx);
    unknown("Kitchn", ctx);
    unknown("gone", ctx);
    unknown("loop", ctx);
    unknown("all@S9", ctx);
    unknown("here", ResolveContext::default());
    unknown("study", ResolveContext::default());

    // Near misses come back as suggestions instead.
    let known = known_rooms(&houses);
    assert_eq!(suggest_rooms("Kitchn", &known), ["Kitchen@S2"]);
    assert_eq!(suggest_rooms("bedrom", &known)[0], "Bedroom@S1");

    // An ambiguous prefix lists the rooms it could mean.
    match resolve_targets(&houses, "Bed", ctx) {
        Err(CoreError::AmbiguousRoom { candidates, .. }) => {
            assert_eq!(candidates, ["Bedroom@S1", "Bedroom 2@S1"]);
        }
        other => panic!("expected AmbiguousRoom, got {other:?}"),
    }

    // A single-room command given a set is ambiguous, naming the set.
    match resolve_one(&houses, "upstairs", ctx) {
        Err(CoreError::AmbiguousRoom { name, candidates }) => {
            assert_eq!(name, "upstairs");
            assert_eq!(
                candidates,
                ["Bedroom@S1", "Bedroom 2@S1", "Guest\u{2019}s Room@S2"]
            );
        }
        other => panic!("expected AmbiguousRoom, got {other:?}"),
    }
    assert_eq!(
        resolve_one(&houses, "study", ctx)
            .unwrap()
            .player
            .ip
            .to_string(),
        "192.0.2.12"
    );
}

#[test]
fn alias_check_warns_with_lines() {
    let houses = [household(ZGS_S1), household(ZGS_S2)];
    let warnings = Aliases::parse(ALIASES).unwrap().check(&houses);
    let lines: Vec<(usize, &str)> = warnings
        .iter()
        .map(|w| (w.line, w.message.as_str()))
        .collect();
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert_eq!(lines[0].0, 4);
    assert!(lines[0].1.contains("also a room name"), "{lines:#?}");
    assert_eq!(lines[1].0, 5);
    assert!(lines[1].1.contains("\"Garage\""), "{lines:#?}");
    assert_eq!(lines[2].0, 6);
    assert!(
        lines[2].1.contains("names the alias \"upstairs\""),
        "{lines:#?}"
    );
    assert_eq!(lines[3].0, 12);
    assert!(lines[3].1.contains("alice@example.com"), "{lines:#?}");
}
