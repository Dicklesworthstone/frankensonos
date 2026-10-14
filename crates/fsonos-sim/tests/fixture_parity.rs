//! The simulator speaks like real players. Every element, attribute, event
//! variable and property it sends also appears in the scrubbed bodies real
//! players sent (`fsonos-proto/tests/fixtures/`), and it sends what
//! FrankenSonos reads from them. A simulator that drifted from real players
//! would let the e2e harness pass against shapes no speaker sends.

use fsonos_proto::Transport;
use fsonos_proto::gena::{Notify, parse_propertyset};
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::soap::{self, AV_TRANSPORT, CONTENT_DIRECTORY, ZONE_GROUP_TOPOLOGY, args_xml};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec, SimTransport};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Element (local name) → the attribute names it carries, over a document.
type Shape = BTreeMap<String, BTreeSet<String>>;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../fsonos-proto/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn shape(xml: &str) -> Shape {
    let doc = roxmltree::Document::parse(xml).unwrap_or_else(|e| panic!("{e}: {xml:.200}"));
    let mut shape = Shape::new();
    for node in doc.descendants().filter(roxmltree::Node::is_element) {
        shape
            .entry(node.tag_name().name().to_string())
            .or_default()
            .extend(node.attributes().map(|a| a.name().to_string()));
    }
    shape
}

fn union(shapes: impl IntoIterator<Item = Shape>) -> Shape {
    let mut all = Shape::new();
    for s in shapes {
        for (element, attrs) in s {
            all.entry(element).or_default().extend(attrs);
        }
    }
    all
}

/// The decoded text of the first `element` in `xml` (a SOAP out-argument).
fn inner(xml: &str, element: &str) -> String {
    let doc = roxmltree::Document::parse(xml).unwrap();
    doc.descendants()
        .find(|n| n.has_tag_name(element))
        .and_then(|n| n.text())
        .unwrap_or_else(|| panic!("no <{element}>"))
        .to_string()
}

/// Everything in `sim` is in `real`.
fn assert_within(what: &str, sim: &Shape, real: &Shape) {
    let mut strays = Vec::new();
    for (element, attrs) in sim {
        match real.get(element) {
            None => strays.push(format!("<{element}>")),
            Some(known) => {
                strays.extend(attrs.difference(known).map(|a| format!("<{element} {a}>")));
            }
        }
    }
    assert!(
        strays.is_empty(),
        "{what}: the simulator sends what no real player sent: {strays:?}"
    );
}

/// `sim` has `element` carrying every one of `attrs`.
fn assert_has(what: &str, sim: &Shape, element: &str, attrs: &[&str]) {
    let have = sim
        .get(element)
        .unwrap_or_else(|| panic!("{what}: no <{element}>"));
    let missing: Vec<&&str> = attrs.iter().filter(|a| !have.contains(**a)).collect();
    assert!(missing.is_empty(), "{what}: <{element}> lacks {missing:?}");
}

/// Kitchen and Office (S1, with a Bridge and a stereo pair), Living Room and
/// Bedroom (S2).
fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
            SimPlayerSpec::pair("Den", SimModel::Play1),
            SimPlayerSpec::new("Bridge", SimModel::Bridge),
        ])
        .s2([
            SimPlayerSpec::new("Living Room", SimModel::One),
            SimPlayerSpec::new("Bedroom", SimModel::Play1),
        ])
        .spawn()
        .unwrap()
}

fn transport(sim: &SimHandle, room: &str) -> SimTransport {
    sim.transport(room).unwrap()
}

#[test]
fn topology_matches_real_zone_group_state() {
    let sim = sim();
    // Something offline, so the vanished list is shown too.
    sim.set_offline("Office", true).unwrap();
    let kitchen = transport(&sim, "Kitchen");
    let zgs = soap::call(
        &kitchen,
        kitchen.ip(),
        &ZONE_GROUP_TOPOLOGY,
        "GetZoneGroupState",
        &args_xml(&[]),
    )
    .unwrap();
    let ours = shape(zgs.require("ZoneGroupState").unwrap());
    let real = union([
        shape(&inner(&fixture("zgs_s1.xml"), "ZoneGroupState")),
        shape(&inner(&fixture("zgs_s2.xml"), "ZoneGroupState")),
    ]);
    assert_within("ZoneGroupState", &ours, &real);
    assert_has("ZoneGroupState", &ours, "ZoneGroup", &["Coordinator", "ID"]);
    assert_has(
        "ZoneGroupState",
        &ours,
        "ZoneGroupMember",
        &[
            "UUID",
            "Location",
            "ZoneName",
            "SoftwareVersion",
            "BootSeq",
            "Invisible",
            "IsZoneBridge",
            "ChannelMapSet",
        ],
    );
    assert_has("ZoneGroupState", &ours, "Device", &["UUID", "ZoneName"]);
}

#[test]
fn device_descriptions_match_each_real_model() {
    let sim = sim();
    for (room, model_fixture) in [
        ("Kitchen", "device_description_s1_play5.xml"),
        ("Bridge", "device_description_s1_bridge.xml"),
        ("Living Room", "device_description_s2_one.xml"),
        ("Bedroom", "device_description_s2_play1.xml"),
    ] {
        let t = transport(&sim, room);
        let ours_text = t
            .http_get(&format!("{}/xml/device_description.xml", t.base_url()))
            .unwrap();
        let real_text = fixture(model_fixture);
        assert_within(room, &shape(&ours_text), &shape(&real_text));
        let services = |xml: &str| -> BTreeSet<String> {
            roxmltree::Document::parse(xml)
                .unwrap()
                .descendants()
                .filter(|n| n.has_tag_name("serviceId"))
                .filter_map(|n| n.text().map(str::to_string))
                .collect()
        };
        let (ours, real) = (services(&ours_text), services(&real_text));
        let strays: Vec<&String> = ours.difference(&real).collect();
        assert!(
            strays.is_empty(),
            "{room}: services no real {room} has: {strays:?}"
        );
        // A Bridge renders nothing: it has topology but no media services.
        let needed: &[&str] = if room == "Bridge" {
            &["ZoneGroupTopology"]
        } else {
            &[
                "ZoneGroupTopology",
                "ContentDirectory",
                "AVTransport",
                "RenderingControl",
            ]
        };
        for service in needed {
            let id = format!("urn:upnp-org:serviceId:{service}");
            assert!(ours.contains(&id), "{room} lacks {id}");
        }
    }
}

#[test]
fn browse_results_and_faults_match_real_ones() {
    let sim = sim();
    let kitchen = transport(&sim, "Kitchen");
    let browse = |object: &str| {
        soap::call(
            &kitchen,
            kitchen.ip(),
            &CONTENT_DIRECTORY,
            "Browse",
            &args_xml(&[
                ("ObjectID", object),
                ("BrowseFlag", "BrowseDirectChildren"),
                ("Filter", "*"),
                ("StartingIndex", "0"),
                ("RequestedCount", "100"),
                ("SortCriteria", ""),
            ]),
        )
    };
    let real_favorites = union([
        shape(&inner(&fixture("browse_favorites_s1.xml"), "Result")),
        shape(&inner(&fixture("browse_favorites_s2.xml"), "Result")),
    ]);
    let favorites = browse("FV:2").unwrap();
    assert_within(
        "favorites",
        &shape(favorites.require("Result").unwrap()),
        &real_favorites,
    );

    // A queue with something in it.
    let all = fsonos_proto::content::browse_all(&kitchen, kitchen.ip(), "FV:2").unwrap();
    let track = all
        .iter()
        .find(|o| {
            o.res
                .as_ref()
                .is_some_and(|r| r.uri.starts_with("x-sonos-spotify:"))
        })
        .unwrap();
    soap::call(
        &kitchen,
        kitchen.ip(),
        &AV_TRANSPORT,
        "AddURIToQueue",
        &args_xml(&[
            ("InstanceID", "0"),
            ("EnqueuedURI", &track.res.as_ref().unwrap().uri),
            (
                "EnqueuedURIMetaData",
                track.res_md.as_deref().unwrap_or_default(),
            ),
            ("DesiredFirstTrackNumberEnqueued", "0"),
            ("EnqueueAsNext", "0"),
        ]),
    )
    .unwrap();
    let real_queue = union([
        shape(&inner(&fixture("browse_queue_s1.xml"), "Result")),
        shape(&inner(&fixture("browse_queue_s2.xml"), "Result")),
        shape(&inner(&fixture("browse_queue_empty.xml"), "Result")),
    ]);
    let queue = browse("Q:0").unwrap();
    assert_within(
        "queue",
        &shape(queue.require("Result").unwrap()),
        &real_queue,
    );

    // A fault, as the HTTP 500 body.
    let body = kitchen
        .soap_post(
            kitchen.ip(),
            CONTENT_DIRECTORY.control_path,
            &soap::soap_action_header(&CONTENT_DIRECTORY, "Browse"),
            &soap::envelope(
                &CONTENT_DIRECTORY,
                "Browse",
                &args_xml(&[
                    ("ObjectID", "NO:SUCH"),
                    ("BrowseFlag", "BrowseDirectChildren"),
                    ("Filter", "*"),
                    ("StartingIndex", "0"),
                    ("RequestedCount", "1"),
                    ("SortCriteria", ""),
                ]),
            ),
        )
        .unwrap();
    assert_within(
        "fault",
        &shape(&body),
        &shape(&fixture("soap_fault_701.xml")),
    );
    assert!(body.contains("<errorCode>701</errorCode>"), "{body}");
}

/// The initial NOTIFY a fresh subscription to `path` on `room` gets.
fn initial_event(sim: &SimHandle, room: &str, path: &str) -> Notify {
    let lan = Lan::start().unwrap();
    let sink = EventSink::start("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("{}{path}", sim.player(room).unwrap().base_url);
    let tag = path.rsplit('/').nth(1).unwrap();
    let sid = lan
        .subscribe_at(&url, &sink.callback_url(tag), 300)
        .unwrap()
        .sid;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(n) = sink.recv_timeout(Duration::from_millis(200))
            && n.sid == sid
        {
            return n;
        }
    }
    panic!("no initial NOTIFY from {room}{path}");
}

/// Property names, plus the shape of a `LastChange` (or of a
/// `ZoneGroupState`) carried in one.
fn event_shape(properties: &[(String, String)]) -> Shape {
    let mut shape = Shape::new();
    for (name, value) in properties {
        shape.entry(format!("property:{name}")).or_default();
        if name == "LastChange" || name == "ZoneGroupState" {
            shape = union([shape, self::shape(value)]);
        }
    }
    shape
}

fn fixture_event(name: &str) -> Shape {
    event_shape(&parse_propertyset(&fixture(name)).unwrap())
}

#[test]
fn events_match_real_notifies_service_by_service() {
    let sim = sim();
    // Playing, so the transport event has a track in it.
    let kitchen = transport(&sim, "Kitchen");
    for (action, args) in [
        (
            "SetAVTransportURI",
            vec![
                ("InstanceID", "0"),
                (
                    "CurrentURI",
                    "x-rincon-mp3radio://stream.example.invalid/a.mp3",
                ),
                ("CurrentURIMetaData", ""),
            ],
        ),
        ("Play", vec![("InstanceID", "0"), ("Speed", "1")]),
    ] {
        soap::call(
            &kitchen,
            kitchen.ip(),
            &AV_TRANSPORT,
            action,
            &args_xml(&args),
        )
        .unwrap();
    }
    for (path, real) in [
        (
            "/MediaRenderer/AVTransport/Event",
            vec![
                "gena_notify_avt_initial_s1.xml",
                "gena_notify_avt_initial_s2.xml",
                "gena_notify_avt_pause_s1.xml",
                "gena_notify_avt_playing_s2.xml",
            ],
        ),
        (
            "/MediaRenderer/RenderingControl/Event",
            vec![
                "gena_notify_rcs_initial_s1.xml",
                "gena_notify_rcs_volume_s1.xml",
            ],
        ),
        (
            "/MediaRenderer/GroupRenderingControl/Event",
            vec!["gena_notify_grc_s1.xml"],
        ),
        (
            "/ZoneGroupTopology/Event",
            vec!["gena_notify_zgt_s1.xml", "gena_notify_zgt_s2.xml"],
        ),
    ] {
        let ours = event_shape(&initial_event(&sim, "Kitchen", path).properties);
        let real = union(real.into_iter().map(fixture_event));
        assert_within(path, &ours, &real);
    }
}
