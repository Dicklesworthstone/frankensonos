use super::*;
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_proto::didl::xml_escape;
use fsonos_proto::topology::{get_zone_group_state, parse_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

const ID: &str = "RINCON_SIM_ALPHA";
const TOPOLOGY: &str = "<ZoneGroups><ZoneGroup ID=\"sim:1\" Coordinator=\"RINCON_SIM_ALPHA\">\
    <ZoneGroupMember UUID=\"RINCON_SIM_ALPHA\" ZoneName=\"Alpha\" SWGen=\"2\" \
    Location=\"http://192.0.2.1:1400/xml/device_description.xml\"/>\
    <ZoneGroupMember UUID=\"RINCON_SIM_BETA\" ZoneName=\"Beta\" SWGen=\"2\" \
    Location=\"http://192.0.2.2:1400/xml/device_description.xml\"/>\
    </ZoneGroup></ZoneGroups>";

fn req(query: &str) -> QueueSearchRequest {
    QueueSearchRequest {
        zone: "Beta".into(),
        query: query.into(),
        offset: 0,
        limit: 50,
        expected_revision: None,
    }
}

fn token(update: u32, total: u32) -> String {
    format!("q1:{}:{ID}:{update}:{total}", ID.len())
}

fn item(position: u32, title: &str, creator: &str, album: &str, uri: &str) -> String {
    format!(
        "<item id=\"Q:0/{position}\" parentID=\"Q:0\" restricted=\"1\">\
         <dc:title>{}</dc:title><dc:creator>{}</dc:creator><upnp:album>{}</upnp:album>\
         <res protocolInfo=\"http-get:*:audio/mpeg:*\">{}</res></item>",
        xml_escape(title),
        xml_escape(creator),
        xml_escape(album),
        xml_escape(uri),
    )
}

fn page(items: &[String], total: u32, update: u32) -> String {
    let didl = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{}</DIDL-Lite>",
        items.join(""),
    );
    response(
        "Browse",
        &format!(
            "<Result>{}</Result><NumberReturned>{}</NumberReturned>\
         <TotalMatches>{total}</TotalMatches><UpdateID>{update}</UpdateID>",
            xml_escape(&didl),
            items.len(),
        ),
    )
}

fn response(action: &str, body: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <s:Body><u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
         {body}</u:{action}Response></s:Body></s:Envelope>"
    )
}

#[derive(Default)]
struct Reads {
    pages: VecDeque<(u32, String)>,
    calls: Vec<(u32, u32)>,
    topology_calls: usize,
    regroup: bool,
}

struct Script(Arc<Mutex<Reads>>);

impl Transport for Script {
    fn soap_post(
        &self,
        _: IpAddr,
        _: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let mut reads = self.0.lock().unwrap();
        if action.contains("#GetZoneGroupState") {
            reads.topology_calls += 1;
            let topology = if reads.regroup && reads.topology_calls > 1 {
                TOPOLOGY.replace(
                    "Coordinator=\"RINCON_SIM_ALPHA\"",
                    "Coordinator=\"RINCON_SIM_BETA\"",
                )
            } else {
                TOPOLOGY.into()
            };
            return Ok(response(
                "GetZoneGroupState",
                &format!("<ZoneGroupState>{}</ZoneGroupState>", xml_escape(&topology)),
            ));
        }
        assert!(action.contains("#Browse"), "search performed {action}");
        let number = |tag: &str| -> u32 {
            body.split(&format!("<{tag}>"))
                .nth(1)
                .unwrap()
                .split('<')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        let start = number("StartingIndex");
        let count = number("RequestedCount");
        assert!((1..=MAX_PAGE).contains(&count));
        reads.calls.push((start, count));
        let (expected_start, reply) = reads.pages.pop_front().expect("bounded scan");
        assert_eq!(start, expected_start);
        Ok(reply)
    }
}

fn scripted(pages: Vec<(u32, String)>, policy: Policy) -> (Surface, Arc<Mutex<Reads>>) {
    let state = Arc::new(Mutex::new(Reads {
        pages: pages.into(),
        ..Reads::default()
    }));
    let mut household = HouseholdState::default();
    household.apply_topology(&parse_zone_group_state(TOPOLOGY).unwrap());
    let surface = Surface::new(
        Box::new(Script(Arc::clone(&state))),
        Box::new(move |_| Ok(vec![household.clone()])),
        policy,
        Box::new(SystemClock),
    );
    (surface, state)
}

fn positions(result: &QueueSearchDto) -> Vec<u32> {
    result.items.iter().map(|item| item.position).collect()
}

#[test]
fn later_pages_match_all_terms_across_metadata_and_preserve_repeated_occurrences() {
    let repeated = "x-sonos-spotify:spotify%3Atrack%3Asynthetic?sn=1&sid=12";
    let items: Vec<_> = (1..=207)
        .map(|position| {
            if [138, 207].contains(&position) {
                item(position, "Prélude", "Élodie", "Evening", repeated)
            } else {
                item(position, "Other", "", "", "test:prelude:elodie:evening")
            }
        })
        .collect();
    let first = page(&items[..100], 207, 7);
    let (surface, reads) = scripted(
        vec![
            (0, first.clone()),
            (100, page(&items[100..200], 207, 7)),
            (200, page(&items[200..], 207, 7)),
            (0, first),
        ],
        Policy::default(),
    );
    let result = surface
        .queue_search(&Client::Cli, &req("ELODIE prelude EVENING"))
        .unwrap();
    assert_eq!(positions(&result), [138, 207]);
    assert!(
        result
            .items
            .iter()
            .all(|item| item.uri.as_deref() == Some(repeated))
    );
    assert_eq!(result.zone, "Alpha");
    assert_eq!((result.examined, result.total), (207, 207));
    assert!(result.complete && result.next_offset.is_none());
    let reads = reads.lock().unwrap();
    assert!(reads.pages.is_empty());
    assert_eq!(reads.calls, [(0, 100), (100, 100), (200, 7), (0, 100)]);
    assert_eq!(reads.topology_calls, 2);
}

#[test]
fn a_limit_midpage_continues_at_the_next_unexamined_occurrence() {
    let items: Vec<_> = (1..=8)
        .map(|position| item(position, "Match", "", "", "test:same"))
        .collect();
    let first = page(&items, 8, 7);
    let next = page(&items[3..], 8, 7);
    let (surface, _) = scripted(
        vec![(0, first.clone()), (0, first), (3, next.clone()), (3, next)],
        Policy::default(),
    );
    let mut request = req("match");
    request.limit = 3;
    let result = surface.queue_search(&Client::Cli, &request).unwrap();
    assert_eq!(positions(&result), [1, 2, 3]);
    assert_eq!(
        (result.examined, result.next_offset, result.complete),
        (3, Some(3), false)
    );
    request.offset = result.next_offset.unwrap();
    request.expected_revision = Some(result.revision);
    let continued = surface.queue_search(&Client::Cli, &request).unwrap();
    assert_eq!(positions(&continued), [4, 5, 6]);
    assert_eq!(
        (continued.offset, continued.examined, continued.next_offset),
        (3, 3, Some(6))
    );
}

#[test]
fn page_budget_returns_an_explicit_continuation_even_with_no_matches() {
    for width in [1_u32, MAX_PAGE] {
        let scanned = u32::try_from(SEARCH_PAGE_READS - 1).unwrap() * width;
        let total = scanned + 2;
        let items: Vec<_> = (1..=total)
            .map(|position| item(position, "Other", "", "", "test:same"))
            .collect();
        let mut pages: Vec<_> = (0..scanned)
            .step_by(usize::try_from(width).unwrap())
            .map(|offset| {
                let start = usize::try_from(offset).unwrap();
                let end = usize::try_from(offset + width).unwrap();
                (offset, page(&items[start..end], total, 0))
            })
            .collect();
        pages.push(pages[0].clone());
        let (surface, reads) = scripted(pages, Policy::default());
        let result = surface
            .search_queue_section(&Client::Cli, &req("absent"), Duration::from_hours(24))
            .unwrap();
        assert!(result.items.is_empty());
        assert!(!result.complete);
        assert_eq!(
            (result.examined, result.next_offset),
            (scanned, Some(scanned))
        );
        assert_eq!(reads.lock().unwrap().calls.len(), SEARCH_PAGE_READS);
        assert!(result.text().contains("More of the queue remains"));
    }
}

#[test]
fn an_expired_work_budget_returns_a_checked_section_without_another_data_page() {
    let first = page(&[item(1, "Other", "", "", "test:same")], 2, 7);
    let (surface, reads) = scripted(vec![(0, first.clone()), (0, first)], Policy::default());
    let result = surface
        .search_queue_section(&Client::Cli, &req("absent"), Duration::ZERO)
        .unwrap();
    assert_eq!(
        (result.examined, result.next_offset, result.complete),
        (1, Some(1), false)
    );
    assert!(result.items.is_empty());
    let reads = reads.lock().unwrap();
    assert_eq!(reads.calls, [(0, 100), (0, 100)]);
    assert_eq!(reads.topology_calls, 2);
}

#[test]
fn empty_queue_is_complete_and_missing_metadata_is_not_fabricated() {
    let empty = page(&[], 0, 0);
    let (surface, _) = scripted(vec![(0, empty.clone()), (0, empty)], Policy::default());
    let result = surface.queue_search(&Client::Cli, &req("absent")).unwrap();
    assert_eq!((result.total, result.examined), (0, 0));
    assert!(result.complete && result.items.is_empty() && result.next_offset.is_none());
    for uri in ["", " ", "NOT_IMPLEMENTED"] {
        let reply = page(&[item(1, "Match", "", "", uri)], 1, 1);
        let (surface, _) = scripted(vec![(0, reply.clone()), (0, reply)], Policy::default());
        let result = surface.queue_search(&Client::Cli, &req("match")).unwrap();
        assert_eq!(positions(&result), [1]);
        assert_eq!(
            result.items[0].uri.as_deref(),
            (!uri.trim().is_empty()).then_some(uri)
        );
        assert!(result.text().contains("cannot be selected"));
    }
}

#[test]
fn changed_revisions_totals_final_contents_and_groups_never_return_a_prefix() {
    let first = page(&[item(1, "Match", "", "", "test:one")], 2, 7);
    for next in [
        page(&[item(2, "Match", "", "", "test:two")], 2, 8),
        page(&[item(2, "Match", "", "", "test:two")], 3, 7),
    ] {
        let (surface, _) = scripted(vec![(0, first.clone()), (1, next)], Policy::default());
        let failure = surface
            .queue_search(&Client::Cli, &req("match"))
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::QueueChanged);
    }
    for after in [
        page(&[item(1, "Match", "", "", "test:one")], 1, 8),
        page(&[item(1, "Replacement", "", "", "test:other")], 1, 7),
    ] {
        let first = page(&[item(1, "Match", "", "", "test:one")], 1, 7);
        let (surface, _) = scripted(vec![(0, first), (0, after)], Policy::default());
        assert_eq!(
            surface
                .queue_search(&Client::Cli, &req("match"))
                .unwrap_err()
                .code,
            ErrorCode::QueueChanged
        );
    }
    let first = page(&[item(1, "Match", "", "", "test:one")], 1, 7);
    let (surface, reads) = scripted(vec![(0, first)], Policy::default());
    reads.lock().unwrap().regroup = true;
    assert_eq!(
        surface
            .queue_search(&Client::Cli, &req("match"))
            .unwrap_err()
            .code,
        ErrorCode::QueueChanged
    );
}

#[test]
fn continuation_rejects_a_changed_coordinator_even_if_counter_and_length_match() {
    let reply = page(&[item(2, "Match", "", "", "test:same")], 2, 7);
    let (surface, _) = scripted(vec![(1, reply)], Policy::default());
    let mut request = req("match");
    request.offset = 1;
    request.expected_revision = Some(token(7, 2).replace(ID, "RINCON_SIM_OTHER"));
    assert_eq!(
        surface
            .queue_search(&Client::Cli, &request)
            .unwrap_err()
            .code,
        ErrorCode::QueueChanged
    );
}

#[test]
fn inconsistent_pages_and_unbounded_metadata_are_errors_not_partial_results() {
    let entry = item(1, "Match", "", "", "test:same");
    for bad in [
        page(&[], 1, 7),
        page(std::slice::from_ref(&entry), 0, 7),
        page(std::slice::from_ref(&entry), 1, 7).replace("<NumberReturned>1", "<NumberReturned>2"),
        page(&vec![entry; 101], 101, 7),
        page(
            &[item(
                1,
                "Match",
                "",
                &"x".repeat(MAX_FIELD_BYTES + 1),
                "test:same",
            )],
            1,
            7,
        ),
        "not xml".into(),
    ] {
        let (surface, _) = scripted(vec![(0, bad)], Policy::default());
        assert_eq!(
            surface
                .queue_search(&Client::Cli, &req("match"))
                .unwrap_err()
                .code,
            ErrorCode::UpnpFault
        );
    }
}

#[test]
fn validation_and_get_queue_policy_run_before_any_player_reads() {
    let (surface, reads) = scripted(Vec::new(), Policy::default());
    let base = req("match");
    for request in [
        QueueSearchRequest {
            zone: " ".into(),
            ..base.clone()
        },
        QueueSearchRequest {
            query: "...".into(),
            ..base.clone()
        },
        QueueSearchRequest {
            query: "x".repeat(MAX_QUERY_BYTES + 1),
            ..base.clone()
        },
        QueueSearchRequest {
            query: "word ".repeat(MAX_QUERY_TERMS + 1),
            ..base.clone()
        },
        QueueSearchRequest {
            query: "word\nword".into(),
            ..base.clone()
        },
        QueueSearchRequest {
            limit: 0,
            ..base.clone()
        },
        QueueSearchRequest {
            limit: MAX_SEARCH_RESULTS + 1,
            ..base.clone()
        },
        QueueSearchRequest {
            offset: 1,
            ..base.clone()
        },
        QueueSearchRequest {
            offset: u32::MAX,
            expected_revision: Some("r".into()),
            ..base.clone()
        },
        QueueSearchRequest {
            expected_revision: Some(" ".into()),
            ..base.clone()
        },
        QueueSearchRequest {
            expected_revision: Some("x".repeat(1025)),
            ..base
        },
    ] {
        assert_eq!(
            surface
                .queue_search(&Client::Cli, &request)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(reads.lock().unwrap().topology_calls, 0);
    let policy = Policy::from_toml("[clients.unknown]\ndeny = [\"get_queue\"]\n").unwrap();
    let (surface, reads) = scripted(Vec::new(), policy);
    assert_eq!(
        surface
            .queue_search(&Client::Unknown, &req("match"))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(reads.lock().unwrap().topology_calls, 0);
}

#[test]
fn request_defaults_and_encoded_paths_keep_delimiters_inside_values() {
    let request: QueueSearchRequest =
        serde_json::from_str(r#"{"zone":"Listening / room@S2","query":"Prélude & fugue"}"#)
            .unwrap();
    assert_eq!((request.offset, request.limit), (0, 50));
    assert_eq!(
        request.path().unwrap(),
        "/zones/Listening%20%2F%20room%40S2/queue/search?q=Pr%C3%A9lude%20%26%20fugue&offset=0&limit=50"
    );
    let mut continuation = request;
    continuation.offset = 3;
    continuation.expected_revision = Some("q1:group&limit=1".into());
    assert!(
        continuation
            .path()
            .unwrap()
            .ends_with("&expected_revision=q1%3Agroup%26limit%3D1")
    );
    assert!(
        serde_json::from_str::<QueueSearchRequest>(
            r#"{"zone":"Room","query":"Match","force":true}"#
        )
        .is_err()
    );
}

#[test]
fn real_s1_and_s2_group_queues_search_later_pages_without_mutations() {
    for model in [SimModel::Play5Gen1, SimModel::One] {
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = match model {
            SimModel::Play5Gen1 => builder.s1(players),
            _ => builder.s2(players),
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let alpha = sim.player("Alpha").unwrap();
        fsonos_proto::control::join_group(
            &lan,
            sim.player("Beta").unwrap().ip,
            &PlayerId(alpha.uuid.clone()),
        )
        .unwrap();
        for position in 1..=105 {
            let title = if [2, 104].contains(&position) {
                "Prélude"
            } else {
                "Other"
            };
            let metadata = format!(
                "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{}</DIDL-Lite>",
                item(
                    position,
                    title,
                    "Élodie",
                    "Evening",
                    "x-file-cifs://example.invalid/repeated.flac"
                ),
            );
            fsonos_proto::control::enqueue_uri(
                &lan,
                alpha.ip,
                "x-file-cifs://example.invalid/repeated.flac",
                &metadata,
                0,
                false,
            )
            .unwrap();
        }
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&lan, alpha.ip).unwrap());
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(vec![household.clone()])),
            Policy::default(),
            Box::new(SystemClock),
        );
        let start = sim.soap_log().len();
        let result = surface
            .queue_search(&Client::Unknown, &req("prelude elodie evening"))
            .unwrap();
        assert_eq!(positions(&result), [2, 104]);
        assert_eq!((result.total, result.examined), (105, 105));
        assert!(result.complete);
        assert!(
            sim.soap_log()[start..]
                .iter()
                .all(|call| matches!(call.action.as_str(), "GetZoneGroupState" | "Browse"))
        );
        sim.shutdown();
    }
}
