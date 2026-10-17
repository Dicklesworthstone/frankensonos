//! `fsonos play <room> --favorite <title> --queue|--next` queues a native
//! favorite (an album here), as HTTP and MCP `add_to_queue` do, instead of
//! playing it at once: the queue grows and nothing starts playing.

mod e2e;

use e2e::Scenario;
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control::get_transport_info;
use fsonos_sim::SimHousehold;
use fsonos_types::TransportState;
use serde_json::Value;

#[test]
fn the_cli_queues_a_favorite_without_playing_it() {
    let mut s = Scenario::start("queue_favorite");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let queued = |s: &Scenario| browse_all(&s.lan(), kitchen, QUEUE).map_or(0, |q| q.len());
    let state = |s: &Scenario| get_transport_info(&s.lan(), kitchen).ok().map(|t| t.state);
    let before = state(&s);

    let run = s.cli(
        "queue",
        &[
            "play",
            "Kitchen",
            "--favorite",
            "Sim Symphonies",
            "--queue",
            "--json",
        ],
    );
    let added: Value = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    let first = queued(&s);
    s.check(
        "queue",
        "cli",
        "the album is added to Kitchen's queue, and nothing starts playing",
        run.ok()
            && added["count"].as_u64().is_some_and(|c| c > 0)
            && first > 0
            && state(&s) == before
            && state(&s) != Some(TransportState::Playing),
        format!(
            "{run:?} queued={first} before={before:?} now={:?}",
            state(&s)
        ),
    );

    let run = s.cli(
        "next",
        &["play", "Kitchen", "--favorite", "Sim Quartets", "--next"],
    );
    s.check(
        "next",
        "cli",
        "a playlist favorite goes in too, and the queue grows again",
        run.ok() && queued(&s) > first,
        format!("{run:?} queued={}", queued(&s)),
    );
    s.finish();
}
