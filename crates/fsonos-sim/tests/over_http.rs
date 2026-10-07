//! The simulator over real loopback sockets, driven by FrankenSonos's own
//! protocol code: `fsonos-proto` builds every request and parses every
//! response, and `fsonos-core` folds the topology into rooms.

use fsonos_core::HouseholdState;
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::description::parse_device_description;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::soap::{self, AV_TRANSPORT, RENDERING_CONTROL, args_xml};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec, SimTransport};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn sim() -> SimHandle {
    SimHousehold::standard()
        .spawn()
        .expect("spawn the simulator")
}

fn avt(
    t: &SimTransport,
    action: &str,
    args: &[(&str, &str)],
) -> Result<soap::SoapResponse, ProtoError> {
    let mut all = vec![("InstanceID", "0")];
    all.extend_from_slice(args);
    soap::call(t, t.ip(), &AV_TRANSPORT, action, &args_xml(&all))
}

fn transport_state(t: &SimTransport) -> String {
    avt(t, "GetTransportInfo", &[])
        .unwrap()
        .require("CurrentTransportState")
        .unwrap()
        .to_string()
}

fn fault_code<T: std::fmt::Debug>(r: Result<T, ProtoError>) -> u16 {
    match r {
        Err(ProtoError::SoapFault { code, .. }) => code,
        other => panic!("expected a UPnP fault, got {other:?}"),
    }
}

#[test]
fn spawns_fast_and_describes_every_player() {
    let started = Instant::now();
    let sim = sim();
    let took = started.elapsed();
    // The bead's target is 200 ms; allow headroom for loaded CI workers.
    assert!(took < Duration::from_secs(2), "spawn took {took:?}");
    println!("two-household spawn: {took:?}");

    assert_eq!(sim.players().len(), 5);
    for p in sim.players() {
        let t = sim.transport(&p.room).unwrap();
        let body = t
            .http_get(&format!("{}/xml/device_description.xml", p.base_url))
            .unwrap();
        let desc = parse_device_description(&body).unwrap();
        assert_eq!(desc.udn.0, p.uuid);
        assert_eq!(desc.room_name, p.room);
        assert_eq!(desc.model_number, p.model.model_number());
        assert_eq!(desc.sw_gen, Some(p.generation));
        assert_eq!(desc.is_renderer(), p.model.is_renderer(), "{}", p.room);
    }
    assert_eq!(sim.player("bridge").unwrap().generation, 1);
    assert_eq!(sim.player("Living Room").unwrap().generation, 2);
    sim.shutdown();
}

#[test]
fn topology_folds_into_rooms() {
    let sim = sim();
    let kitchen = sim.transport("Kitchen").unwrap();
    let zgs = get_zone_group_state(&kitchen, kitchen.ip()).unwrap();
    assert_eq!(
        zgs.groups.len(),
        3,
        "Kitchen, Office, and the Bridge's own group"
    );
    let mut s1 = HouseholdState::default();
    s1.apply_topology(&zgs);
    let rooms: Vec<&str> = s1.rooms.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(rooms, ["Kitchen", "Office"], "the bridge is not a room");
    assert_eq!(s1.players.len(), 2);

    // Group Office with Kitchen, then the topology shows one group of two.
    let office = sim.transport("Office").unwrap();
    let kitchen_uuid = sim.player("Kitchen").unwrap().uuid.clone();
    avt(
        &office,
        "SetAVTransportURI",
        &[
            ("CurrentURI", &format!("x-rincon:{kitchen_uuid}")),
            ("CurrentURIMetaData", ""),
        ],
    )
    .unwrap();
    s1.apply_topology(&get_zone_group_state(&office, office.ip()).unwrap());
    assert_eq!(s1.groups.len(), 1);
    assert_eq!(s1.groups[0].members.len(), 2);
    let office_room = s1.rooms.iter().find(|r| r.name == "Office").unwrap();
    assert_eq!(office_room.coordinator.0, kitchen_uuid);
    // The member now refuses coordinator verbs.
    assert_eq!(fault_code(avt(&office, "Play", &[("Speed", "1")])), 800);

    let living = sim.transport("Living Room").unwrap();
    assert_eq!(
        get_zone_group_state(&living, living.ip())
            .unwrap()
            .groups
            .len(),
        2
    );
}

#[test]
fn favorites_teach_render_params_and_play() {
    let sim = sim();
    for (room, sw_gen) in [("Kitchen", 1), ("Living Room", 2)] {
        let t = sim.transport(room).unwrap();
        let favorites = content::browse_all(&t, t.ip(), content::FAVORITES).unwrap();
        assert_eq!(favorites.len(), 5);
        let expected = sim.render_params(sw_gen).unwrap();

        // Learn sid/flags/sn and the descriptor from a track favorite.
        let track = favorites
            .iter()
            .find(|f| {
                f.res
                    .as_ref()
                    .is_some_and(|r| r.uri.starts_with("x-sonos-spotify:"))
            })
            .unwrap();
        let res = track.res.as_ref().unwrap();
        let query = res.uri.split_once('?').unwrap().1;
        assert_eq!(
            query,
            format!(
                "sid={}&flags={}&sn={}",
                expected.sid, expected.flags, expected.sn
            )
        );
        let md = track.res_md_object().unwrap().unwrap();
        assert_eq!(md.desc.unwrap().value, expected.cdudn);
        assert!(md.id.starts_with(&expected.item_id_prefix));
        assert!(
            spotify_uri_from_renderer_uri(&res.uri)
                .unwrap()
                .starts_with("spotify:track:0SimS")
        );

        // Replaying the favorite verbatim renders.
        avt(
            &t,
            "SetAVTransportURI",
            &[
                ("CurrentURI", &res.uri),
                ("CurrentURIMetaData", track.res_md.as_deref().unwrap()),
            ],
        )
        .unwrap();
        avt(&t, "Play", &[("Speed", "1")]).unwrap();
        assert_eq!(transport_state(&t), "PLAYING");
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one ordered scenario; splitting it loses the sequence
fn queue_flow_and_faults_over_http() {
    let sim = sim();
    let t = sim.transport("Kitchen").unwrap();
    let params = sim.render_params(1).unwrap();
    let kitchen_uuid = sim.player("Kitchen").unwrap().uuid.clone();
    let track = |id: &str, desc: &str| {
        let enc = format!("spotify%3atrack%3a{id}");
        let meta = format!(
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
             xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\" xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\
             <item id=\"{}{enc}\" parentID=\"-1\" restricted=\"true\"><dc:title>Track {id}</dc:title>\
             <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
             <desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{desc}</desc></item></DIDL-Lite>",
            params.item_id_prefix
        );
        (enc, meta)
    };
    let enqueue = |uri: &str, meta: &str| {
        avt(
            &t,
            "AddURIToQueue",
            &[
                ("EnqueuedURI", uri),
                ("EnqueuedURIMetaData", meta),
                ("DesiredFirstTrackNumberEnqueued", "0"),
                ("EnqueueAsNext", "0"),
            ],
        )
    };

    // docs/PROTOCOL.md "Queue flow": bare URI + DIDL, queue source, seek, play, next.
    for id in ["A", "B"] {
        let (uri, meta) = track(id, &params.cdudn);
        enqueue(&uri, &meta).unwrap();
    }
    avt(
        &t,
        "SetAVTransportURI",
        &[
            ("CurrentURI", &format!("x-rincon-queue:{kitchen_uuid}#0")),
            ("CurrentURIMetaData", ""),
        ],
    )
    .unwrap();
    avt(&t, "Seek", &[("Unit", "TRACK_NR"), ("Target", "1")]).unwrap();
    avt(&t, "Play", &[("Speed", "1")]).unwrap();
    avt(&t, "Next", &[]).unwrap();
    assert_eq!(transport_state(&t), "PLAYING");
    sim.clock().advance(Duration::from_secs(65));
    let pos = avt(&t, "GetPositionInfo", &[]).unwrap();
    assert_eq!(
        (pos.get("Track"), pos.get("RelTime")),
        (Some("2"), Some("0:01:05"))
    );
    let queue =
        content::browse(&t, t.ip(), content::QUEUE, BrowseFlag::DirectChildren, 0, 0).unwrap();
    assert_eq!(queue.total_matches, 2);
    let first = queue.objects[0].to_track().unwrap();
    assert_eq!(first.source_uri, "spotify:track:A");
    assert_eq!(first.duration_secs, Some(180));

    // Faults arrive as UPnP errors with real players' codes.
    let (_, meta) = track("C", &params.cdudn);
    assert_eq!(fault_code(enqueue("spotify:track:C", &meta)), 714);
    let (uri, wrong) = track("C", "SA_RINCON2311_X_#Svc2311-0-Token");
    assert_eq!(fault_code(enqueue(&uri, &wrong)), 800);
    assert_eq!(fault_code(avt(&t, "Next", &[])), 711);
    let volume = soap::call(
        &t,
        t.ip(),
        &RENDERING_CONTROL,
        "SetVolume",
        &args_xml(&[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", "101"),
        ]),
    );
    assert_eq!(fault_code(volume), 402);
    assert_eq!(
        fault_code(content::browse(
            &t,
            t.ip(),
            "NOPE:0",
            BrowseFlag::DirectChildren,
            0,
            5
        )),
        701
    );

    // A bridge has no AVTransport at all.
    let bridge = sim.transport("Bridge").unwrap();
    assert!(
        matches!(avt(&bridge, "Play", &[("Speed", "1")]), Err(ProtoError::Network { detail, .. }) if detail == "HTTP 404")
    );

    // The log records every SOAP request answered, faults included.
    let log = sim.soap_log();
    let kitchen_calls = log.iter().filter(|e| e.room == "Kitchen").count();
    assert!(kitchen_calls >= 12, "{kitchen_calls}");
    let faults: Vec<u16> = log
        .iter()
        .filter_map(|e| e.result.as_ref().err().copied())
        .collect();
    assert_eq!(faults, [714, 800, 711, 402, 701]);
    let add = log.iter().find(|e| e.action == "AddURIToQueue").unwrap();
    assert_eq!(add.service, "AVTransport");
    assert!(
        add.args
            .iter()
            .any(|(k, v)| k == "EnqueuedURI" && v == "spotify%3atrack%3aA")
    );
}

#[test]
fn unknown_hosts_are_refused() {
    let sim = sim();
    let addr = sim.player("Kitchen").unwrap().addr;
    let mut raw = TcpStream::connect(addr).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    raw.write_all(b"GET /xml/device_description.xml HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n").unwrap();
    let mut reply = String::new();
    raw.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 421"), "{reply}");
}

/// Everything the simulator serves uses synthetic identifiers only: the
/// fixture scrub rules (fsonos-proto tests/golden.rs) applied to its output.
#[test]
fn served_documents_are_synthetic() {
    let sim = sim();
    let mut docs = Vec::new();
    for p in sim.players() {
        let t = sim.transport(&p.room).unwrap();
        docs.push(
            t.http_get(&format!("{}/xml/device_description.xml", p.base_url))
                .unwrap(),
        );
        let call = |svc: &soap::Service, action: &str, args: &[(&str, &str)]| {
            t.soap_post(
                t.ip(),
                svc.control_path,
                &soap::soap_action_header(svc, action),
                &soap::envelope(svc, action, &args_xml(args)),
            )
            .unwrap()
        };
        docs.push(call(&soap::ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", &[]));
        if p.model.is_renderer() {
            docs.push(call(
                &soap::CONTENT_DIRECTORY,
                "Browse",
                &[
                    ("ObjectID", "FV:2"),
                    ("BrowseFlag", "BrowseDirectChildren"),
                    ("Filter", "*"),
                    ("StartingIndex", "0"),
                    ("RequestedCount", "0"),
                    ("SortCriteria", ""),
                ],
            ));
        }
    }
    for doc in &docs {
        for private in ["192.168.", "://10.", "://172.", "Sonos_"] {
            assert!(!doc.contains(private), "found {private:?}");
        }
        for (at, _) in doc.match_indices("RINCON_") {
            let id = &doc[at + 7..];
            let service_descriptor =
                doc[..at].ends_with("SA_") && id.starts_with(|c: char| c.is_ascii_digit());
            assert!(
                service_descriptor || id.starts_with("000E58A0"),
                "{}",
                &doc[at..(at + 24).min(doc.len())]
            );
        }
    }
    assert!(docs.len() >= 13);
}

#[test]
fn lan_routes_by_advertised_address_and_pairs_fold_into_one_room() {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::pair("Den", SimModel::Play5Gen1),
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap();
    assert_eq!(sim.players().len(), 3);
    assert_eq!(sim.players().iter().filter(|p| p.hidden).count(), 1);
    let lan = sim.lan();
    let den = sim.player("Den").unwrap();
    assert!(!den.hidden);
    assert_eq!(den.ip.to_string(), "192.0.2.10");

    // IP-addressed client code runs unchanged: the topology names each
    // player by its advertised address, and the LAN transport reaches it.
    let mut state = HouseholdState::default();
    state.apply_topology(&get_zone_group_state(&lan, den.ip).unwrap());
    let rooms: Vec<(&str, usize)> = state
        .rooms
        .iter()
        .map(|r| (r.name.as_str(), r.players.len()))
        .collect();
    assert_eq!(rooms, [("Den", 2), ("Kitchen", 1)]);
    for p in &state.players {
        let body = lan
            .http_get(&format!("http://{}:1400/xml/device_description.xml", p.ip))
            .unwrap();
        assert_eq!(parse_device_description(&body).unwrap().udn, p.id);
    }
    let kitchen_ip = sim.player("Kitchen").unwrap().ip;
    let r = soap::call(
        &lan,
        kitchen_ip,
        &RENDERING_CONTROL,
        "GetVolume",
        &args_xml(&[("InstanceID", "0"), ("Channel", "Master")]),
    )
    .unwrap();
    assert_eq!(r.get("CurrentVolume"), Some("20"));
    assert!(matches!(
        soap::call(
            &lan,
            "192.0.2.200".parse().unwrap(),
            &RENDERING_CONTROL,
            "GetVolume",
            ""
        ),
        Err(ProtoError::Network { .. })
    ));
}

/// How many players render in the group `coordinator` leads.
fn group_size(t: &SimTransport, coordinator: &str) -> usize {
    get_zone_group_state(t, t.ip())
        .unwrap()
        .groups
        .iter()
        .filter(|g| g.coordinator.0 == coordinator)
        .map(|g| g.members.len())
        .sum()
}

#[test]
fn a_slow_join_lands_later_and_delegation_hands_the_group_over() {
    let sim = sim();
    let kitchen = sim.transport("Kitchen").unwrap();
    let office = sim.transport("Office").unwrap();
    let kitchen_uuid = sim.player("Kitchen").unwrap().uuid.clone();
    let office_uuid = sim.player("Office").unwrap().uuid.clone();
    let radio = "x-rincon-mp3radio://radio.example/stream";
    avt(
        &kitchen,
        "SetAVTransportURI",
        &[("CurrentURI", radio), ("CurrentURIMetaData", "")],
    )
    .unwrap();
    avt(&kitchen, "Play", &[("Speed", "1")]).unwrap();
    let delegate = || {
        avt(
            &kitchen,
            "DelegateGroupCoordinationTo",
            &[("NewCoordinator", &office_uuid), ("RejoinGroup", "0")],
        )
    };

    // The join is answered at once and lands in the topology later.
    sim.join_lag("Office", Duration::from_millis(300)).unwrap();
    let asked = Instant::now();
    avt(
        &office,
        "SetAVTransportURI",
        &[
            ("CurrentURI", &format!("x-rincon:{kitchen_uuid}")),
            ("CurrentURIMetaData", ""),
        ],
    )
    .unwrap();
    assert!(asked.elapsed() < Duration::from_millis(300));
    assert_eq!(group_size(&kitchen, &kitchen_uuid), 1, "not in yet");
    assert_eq!(fault_code(delegate()), 800, "Office is no member yet");
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(group_size(&kitchen, &kitchen_uuid), 2, "landed");

    // Kitchen hands the group and its playback to Office, then leaves.
    assert_eq!(
        fault_code(avt(
            &kitchen,
            "DelegateGroupCoordinationTo",
            &[("NewCoordinator", &kitchen_uuid), ("RejoinGroup", "0")],
        )),
        800,
        "not to itself"
    );
    delegate().unwrap();
    assert_eq!(group_size(&kitchen, &office_uuid), 1, "Kitchen left");
    assert_eq!(group_size(&kitchen, &kitchen_uuid), 1);
    assert_eq!(transport_state(&office), "PLAYING");
    assert_ne!(transport_state(&kitchen), "PLAYING");
    let media = avt(&office, "GetMediaInfo", &[]).unwrap();
    assert_eq!(media.require("CurrentURI").unwrap(), radio);
    assert_eq!(fault_code(delegate()), 800, "Kitchen leads nothing now");
}
