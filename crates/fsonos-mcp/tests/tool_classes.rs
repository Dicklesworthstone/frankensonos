//! Every tool the MCP server registers is under the house policy's tool
//! classes (p7vq.14): a read, a tool in a class (so `deny = ["volume"]`
//! names it), or one held to the policy as the tools it carries out, listed
//! here with how. A mutating tool in no class escapes every class an owner
//! denies, so registering one fails here until it is classified.

use fsonos_core::policy::{CARRIES, CLASSES, class_of};
use fsonos_mcp::{Profile, server_with};

/// Mutating tools in no class, and what holds them to the policy instead.
const HELD_AS_OTHERS: [(&str, &str); 4] = [
    (
        "echo",
        "nothing to hold: it answers its text and reaches no speaker",
    ),
    (
        "fade_and_pause",
        "pause, fade_volume and set_volume, which it carries (CARRIES)",
    ),
    (
        "search_and_play",
        "play or play_favorite, whichever it carries out",
    ),
    (
        "apply_scene",
        "each step it takes; whether it joins house-config is the owner's call",
    ),
];

/// Classed tools the HTTP API serves and MCP does not.
const HTTP_ONLY: [&str; 2] = ["spotify_sign_in", "spotify_sign_in_complete"];

#[test]
fn every_registered_tool_is_a_read_or_classed_or_held_as_the_tools_it_carries_out() {
    let tools = server_with(Vec::new(), Profile::Full).tools();
    let registered: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let unclassed: Vec<&str> = tools
        .iter()
        .filter(|t| t.annotations.as_ref().and_then(|a| a.read_only) != Some(true))
        .map(|t| t.name.as_str())
        .filter(|name| class_of(name).is_none())
        .filter(|name| !HELD_AS_OTHERS.iter().any(|(held, _)| held == name))
        .collect();
    assert!(
        unclassed.is_empty(),
        "mutating tools in no policy class (put each in CLASSES, or say what holds it): \
         {unclassed:?}"
    );

    // A class holds writes: a tool marked read-only in one is mislabelled.
    let read_classed: Vec<&str> = tools
        .iter()
        .filter(|t| t.annotations.as_ref().and_then(|a| a.read_only) == Some(true))
        .map(|t| t.name.as_str())
        .filter(|name| class_of(name).is_some())
        .collect();
    assert!(
        read_classed.is_empty(),
        "tools marked read-only sit in a class of writes: {read_classed:?}"
    );

    // The table stays true: each entry is a registered tool in no class.
    for (held, how) in HELD_AS_OTHERS {
        assert!(
            registered.contains(&held),
            "{held} ({how}) is not registered"
        );
        assert_eq!(class_of(held), None, "{held} is classed now: drop it here");
    }
    assert!(CARRIES.iter().any(|(tool, _)| *tool == "fade_and_pause"));

    // And no class names a tool that is gone.
    let gone: Vec<&str> = CLASSES
        .iter()
        .flat_map(|(_, tools)| tools.iter().copied())
        .filter(|tool| !registered.contains(tool) && !HTTP_ONLY.contains(tool))
        .collect();
    assert!(
        gone.is_empty(),
        "classes name tools MCP does not register: {gone:?}"
    );
}
