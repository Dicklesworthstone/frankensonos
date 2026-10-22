//! Queue searches from real CLI and MCP processes against loopback players.

mod e2e;

use e2e::Scenario;
use fsonos_api::surface::queue::QueueSearchDto;
use fsonos_core::{control, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use serde_json::json;
use std::time::Duration;

fn setup(name: &str, s2: bool) -> Scenario {
    let mut scenario = Scenario::start(name);
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Lead", model),
        SimPlayerSpec::new("Member", model),
    ];
    scenario.sim(if s2 {
        SimHousehold::builder().s2(players)
    } else {
        SimHousehold::builder().s1(players)
    });
    let lan = scenario.lan();
    let host = scenario.ip("Lead");
    let houses = inventory::survey(&lan, &[host], Duration::ZERO)
        .unwrap()
        .households;
    let id = resolve_room(&houses, "Lead")
        .unwrap()
        .coordinator
        .id
        .clone();
    control::queue_spotify_tracks(
        &lan,
        &houses,
        &id,
        &[
            ("spotify:track:SearchA", "Strings and voices"),
            ("spotify:track:SearchB", "Piano"),
            ("spotify:track:SearchA", "Strings and voices"),
            ("spotify:track:SearchC", "Strings finale"),
        ],
    )
    .unwrap()
    .expect("synthetic Spotify favorite");
    control::play_queue_from(&lan, &houses, &id, 2).unwrap();
    soap::pause(&lan, host).unwrap();
    soap::set_av_transport_uri(
        &lan,
        scenario.ip("Member"),
        &format!("x-rincon:{}", id.0),
        "",
    )
    .unwrap();
    scenario
}

fn search(s: &mut Scenario, mode: &str, room: &str) {
    let host = s.ip("Lead");
    let before = browse_snapshot(&s.lan(), host, QUEUE).unwrap();
    let media = soap::get_media_info(&s.lan(), host).unwrap();
    let transport = soap::get_transport_info(&s.lan(), host).unwrap();
    let first = s.cli(
        "first-search",
        &[
            mode,
            "--json",
            "queue-search",
            room,
            "strings",
            "--limit",
            "1",
        ],
    );
    assert!(first.ok(), "{}", first.stderr);
    let first: QueueSearchDto = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(first.zone, "Lead");
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].position, 1);
    assert!(!first.complete);
    let offset = first.next_offset.unwrap().to_string();
    let next = s.cli(
        "continued-search",
        &[
            mode,
            "--json",
            "queue-search",
            room,
            "strings",
            "--offset",
            &offset,
            "--expected-revision",
            &first.revision,
        ],
    );
    assert!(next.ok(), "{}", next.stderr);
    let next: QueueSearchDto = serde_json::from_str(&next.stdout).unwrap();
    s.check(
        "occurrences",
        "cli",
        "duplicate renderer identities remain distinct queue occurrences",
        next.items
            .iter()
            .map(|item| item.position)
            .collect::<Vec<_>>()
            == [3, 4]
            && next.items[0].uri == first.items[0].uri
            && next.revision == first.revision
            && next.complete
            && next.next_offset.is_none(),
        format!("{next:?}"),
    );
    s.check(
        "read-only",
        "speaker",
        "search leaves the queue, selected source and paused playback unchanged",
        browse_snapshot(&s.lan(), host, QUEUE).unwrap() == before
            && soap::get_media_info(&s.lan(), host).unwrap() == media
            && soap::get_transport_info(&s.lan(), host).unwrap() == transport,
        "before and after reads",
    );
    let text = s.cli("search-text", &[mode, "queue-search", room, "piano"]);
    s.check(
        "search-text",
        "cli",
        "text output names the found track and revision",
        text.ok() && text.stdout.contains("Piano") && text.stdout.contains(&first.revision),
        format!("{} {}", text.stdout, text.stderr),
    );

    soap::enqueue_uri(&s.lan(), host, "test:new-controller-track", "", 0, false).unwrap();
    let changed = browse_snapshot(&s.lan(), host, QUEUE).unwrap();
    let stale = s.cli(
        "stale-search",
        &[
            mode,
            "queue-search",
            room,
            "strings",
            "--offset",
            &offset,
            "--expected-revision",
            &first.revision,
        ],
    );
    s.check(
        "stale-search",
        "cli",
        "a stale continuation is an error and never restarts a new search",
        !stale.ok()
            && stale.stderr.contains("QUEUE_CHANGED")
            && browse_snapshot(&s.lan(), host, QUEUE).unwrap() == changed,
        &stale.stderr,
    );
}

#[test]
fn direct_search_continues_exact_occurrences_on_s1_and_s2() {
    for s2 in [false, true] {
        let mut s = setup(
            if s2 {
                "queue-search-direct-s2"
            } else {
                "queue-search-direct-s1"
            },
            s2,
        );
        search(
            &mut s,
            "--direct",
            if s2 { "Member@S2" } else { "Member@S1" },
        );
        s.finish();
    }
}

#[test]
fn daemon_and_mcp_search_the_same_live_group_queue() {
    let mut s = setup("queue-search-daemon", true);
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );
    let mut mcp = s.mcp_with(&["--daemon"]);
    mcp.initialize();
    let found = mcp.request(
        "tools/call",
        &json!({"name": "search_queue", "arguments": {
            "zone": "Member@S2", "query": "strings", "limit": 2
        }}),
    );
    let text = found["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    s.check(
        "agent-search",
        "mcp",
        "the legacy MCP client receives named occurrences and continuation information",
        found["result"].is_object()
            && found["result"]["isError"] != true
            && text.contains("Strings and voices")
            && text.contains("revision")
            && text.contains("offset"),
        &found,
    );
    drop(mcp);
    search(&mut s, "--daemon", "Member@S2");
    drop(daemon);
    s.finish();
}
