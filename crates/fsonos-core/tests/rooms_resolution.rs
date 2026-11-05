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

fn houses_with_duplicate_room_names() -> [HouseholdState; 2] {
    let mut houses = [household(ZGS_S1), household(ZGS_S2)];
    for room in &mut houses[0].rooms {
        if room.name == "Den" {
            room.name = "Kitchen".into();
        }
    }
    for player in &mut houses[0].players {
        if player.room_name == "Den" {
            player.room_name = "Kitchen".into();
        }
    }
    houses
}

#[test]
fn ambiguous_alias_members_refuse_the_entire_target_set() {
    let houses = houses_with_duplicate_room_names();
    let aliases = Aliases::parse(
        r#"[aliases]
single = "Kitchen"
first = ["Kitchen", "Bedroom"]
last = ["Bedroom", "Kitchen"]

[defaults]
room = "last"
"#,
    )
    .unwrap();
    let ctx = ResolveContext {
        aliases: Some(&aliases),
        client: None,
    };

    // No resolved prefix or suffix escapes as a partial set, including when
    // `here` names the alias. The error names the member that needs fixing.
    for query in ["Kitchen", "single", "first", "last", "here"] {
        match resolve_targets(&houses, query, ctx) {
            Err(CoreError::AmbiguousRoom { name, candidates }) => {
                assert_eq!(name, "Kitchen", "{query}");
                assert_eq!(candidates, ["Kitchen@S1", "Kitchen@S2"], "{query}");
            }
            other => panic!("{query:?}: expected AmbiguousRoom, got {other:?}"),
        }
    }
}

#[test]
fn qualified_alias_members_preserve_explicit_sets_and_deduplicate_rooms() {
    let houses = houses_with_duplicate_room_names();
    let aliases = Aliases::parse(
        r#"[aliases]
one = "Kitchen@S1"
both = ["Kitchen@S1", "Kitchen@S2", "Bedroom"]
overlap = ["Kitchen@S1", "RINCON_000E58A0000201400"]
"#,
    )
    .unwrap();
    let ctx = ResolveContext {
        aliases: Some(&aliases),
        client: None,
    };

    let one = resolve_one(&houses, "one", ctx).unwrap();
    assert_eq!(one.room.name, "Kitchen");
    assert_eq!(one.player.generation, fsonos_types::Generation::S1);
    let both = resolve_targets(&houses, "both", ctx).unwrap();
    assert_eq!(both.matched, Matched::Alias);
    assert_eq!(rooms(&both), ["Kitchen", "Kitchen", "Bedroom"]);
    assert_eq!(
        both.targets[0].player.generation,
        fsonos_types::Generation::S1
    );
    assert_eq!(
        both.targets[1].player.generation,
        fsonos_types::Generation::S2
    );
    assert_eq!(
        resolve_targets(&houses, "overlap", ctx)
            .unwrap()
            .targets
            .len(),
        1
    );
    assert!(aliases.check(&houses).is_empty());

    // Reserved whole-house targets still intentionally include every room.
    assert_eq!(
        resolve_targets(&houses, "all", ctx).unwrap().targets.len(),
        8
    );
    assert_eq!(
        resolve_targets(&houses, "everywhere@S1", ctx)
            .unwrap()
            .targets
            .len(),
        4
    );
}

#[test]
fn alias_check_names_ambiguous_members_and_defaults_with_repair_choices() {
    let houses = houses_with_duplicate_room_names();
    let aliases = Aliases::parse(
        r#"[aliases]
single = "Kitchen"
set = ["Bedroom", "Kitchen"]
qualified = ["Kitchen@S1", "Kitchen@S2"]

[defaults]
room = "Kitchen"

[defaults.clients."alice@example.com"]
room = "Kitchen"
"#,
    )
    .unwrap();
    let warnings = aliases.check(&houses);
    assert_eq!(
        warnings.iter().map(|w| w.line).collect::<Vec<_>>(),
        [2, 3, 7, 10]
    );
    for warning in &warnings {
        assert!(
            warning.message.contains("room \"Kitchen\" is ambiguous"),
            "{warning:?}"
        );
        assert!(
            warning.message.contains("Kitchen@S1, Kitchen@S2"),
            "{warning:?}"
        );
    }
    assert!(warnings[0].message.starts_with("alias \"single\""));
    assert!(warnings[1].message.starts_with("alias \"set\""));
    assert!(warnings[2].message.starts_with("the default room:"));
    assert!(warnings[3].message.contains("alice@example.com"));
}
