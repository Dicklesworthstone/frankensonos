//! GENA eventing and fault injection over real loopback sockets, driven by
//! FrankenSonos's own client: `fsonos_proto::net::Lan` subscribes, renews
//! and unsubscribes; `fsonos_proto::net::EventSink` receives the NOTIFYs;
//! `fsonos_proto::gena` parses them.

use fsonos_proto::gena::Notify;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::soap::{self, AV_TRANSPORT, RENDERING_CONTROL, args_xml};
use fsonos_proto::topology::{get_zone_group_state, parse_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{GenaEvent, NotifyDrop, SimHandle, SimHousehold, SimTransport};
use std::time::{Duration, Instant};

const AVT: &str = "/MediaRenderer/AVTransport/Event";
const RCS: &str = "/MediaRenderer/RenderingControl/Event";
const ZGT: &str = "/ZoneGroupTopology/Event";
const GRC: &str = "/MediaRenderer/GroupRenderingControl/Event";

struct Subscriber {
    lan: Lan,
    sink: EventSink,
    /// NOTIFYs received for subscriptions other than the one being awaited.
    held: Vec<Notify>,
}

impl Subscriber {
    fn new() -> Self {
        Self {
            lan: Lan::start().unwrap(),
            sink: EventSink::start("127.0.0.1:0".parse().unwrap()).unwrap(),
            held: Vec::new(),
        }
    }

    fn subscribe(&self, sim: &SimHandle, room: &str, path: &str, timeout: u32) -> String {
        let url = format!("{}{path}", sim.player(room).unwrap().base_url);
        let tag = path.rsplit('/').nth(1).unwrap();
        self.lan
            .subscribe_at(&url, &self.sink.callback_url(tag), timeout)
            .unwrap()
            .sid
    }

    /// The next NOTIFY for `sid`, holding on to others.
    fn next(&mut self, sid: &str) -> Notify {
        if let Some(i) = self.held.iter().position(|n| n.sid == sid) {
            return self.held.remove(i);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(n) = self.sink.recv_timeout(Duration::from_millis(200)) {
                if n.sid == sid {
                    return n;
                }
                self.held.push(n);
            }
        }
        panic!("no NOTIFY for {sid} within 5 s");
    }

    /// No NOTIFY for `sid` arrives within `wait`.
    fn quiet(&mut self, sid: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if let Some(n) = self.sink.recv_timeout(Duration::from_millis(50)) {
                if n.sid == sid {
                    return false;
                }
                self.held.push(n);
            }
        }
        !self.held.iter().any(|n| n.sid == sid)
    }
}

fn call(
    t: &SimTransport,
    svc: &soap::Service,
    action: &str,
    args: &[(&str, &str)],
) -> Result<soap::SoapResponse, ProtoError> {
    soap::call(t, t.ip(), svc, action, &args_xml(args))
}

fn set_volume(t: &SimTransport, v: &str) {
    call(
        t,
        &RENDERING_CONTROL,
        "SetVolume",
        &[
            ("InstanceID", "0"),
            ("Channel", "Master"),
            ("DesiredVolume", v),
        ],
    )
    .unwrap();
}

fn play_radio(t: &SimTransport) {
    call(
        t,
        &AV_TRANSPORT,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            (
                "CurrentURI",
                "x-rincon-mp3radio://stream.example.invalid/a.mp3",
            ),
            ("CurrentURIMetaData", ""),
        ],
    )
    .unwrap();
    call(
        t,
        &AV_TRANSPORT,
        "Play",
        &[("InstanceID", "0"), ("Speed", "1")],
    )
    .unwrap();
}

fn refused_with(r: Result<impl std::fmt::Debug, ProtoError>, status: u16) {
    match r {
        Err(ProtoError::Network { detail, .. }) => {
            assert!(detail.contains(&format!("HTTP {status}")), "{detail}");
        }
        other => panic!("expected HTTP {status}, got {other:?}"),
    }
}

#[test]
fn initial_and_change_events_follow_each_services_policy() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let kitchen = sim.transport("Kitchen").unwrap();
    let mut sub = Subscriber::new();

    // RenderingControl: everything first, then only what changed.
    let rcs = sub.subscribe(&sim, "Kitchen", RCS, 300);
    let initial = sub.next(&rcs);
    assert_eq!(initial.seq, 0);
    assert!(initial.path.ends_with("/RenderingControl"));
    let lc = initial.last_change().unwrap().unwrap();
    assert_eq!((lc.volume(), lc.mute()), (Some(20), Some(false)));
    set_volume(&kitchen, "33");
    let change = sub.next(&rcs);
    assert_eq!(change.seq, 1);
    let lc = change.last_change().unwrap().unwrap();
    assert_eq!(lc.volume(), Some(33));
    assert_eq!(lc.mute(), None, "only the changed variable is sent");

    // AVTransport: the whole state, every time.
    let avt = sub.subscribe(&sim, "Kitchen", AVT, 300);
    let initial = sub.next(&avt);
    let lc = initial.last_change().unwrap().unwrap();
    assert_eq!(
        (initial.seq, lc.get("TransportState")),
        (0, Some("STOPPED"))
    );
    play_radio(&kitchen);
    let mut last = sub.next(&avt);
    while last.last_change().unwrap().unwrap().get("TransportState") != Some("PLAYING") {
        last = sub.next(&avt);
    }
    let lc = last.last_change().unwrap().unwrap();
    assert!(last.seq >= 1);
    assert_eq!(lc.get("CurrentPlayMode"), Some("NORMAL"), "full state");
    assert_eq!(
        lc.get("AVTransportURI"),
        Some("x-rincon-mp3radio://stream.example.invalid/a.mp3")
    );

    // ZoneGroupTopology: the full ZoneGroupState on every change.
    let zgt = sub.subscribe(&sim, "Office", ZGT, 300);
    let initial = sub.next(&zgt);
    let zgs = parse_zone_group_state(initial.property("ZoneGroupState").unwrap()).unwrap();
    assert_eq!(zgs.groups.len(), 3);
    let kitchen_uuid = sim.player("Kitchen").unwrap().uuid.clone();
    let office = sim.transport("Office").unwrap();
    call(
        &office,
        &AV_TRANSPORT,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", &format!("x-rincon:{kitchen_uuid}")),
            ("CurrentURIMetaData", ""),
        ],
    )
    .unwrap();
    let grouped = sub.next(&zgt);
    assert_eq!(grouped.seq, 1);
    let zgs = parse_zone_group_state(grouped.property("ZoneGroupState").unwrap()).unwrap();
    assert_eq!(zgs.groups.len(), 2);
    assert_eq!(grouped.property("ZoneGroupName"), Some("Kitchen + Office"));
    assert!(grouped.property("AvailableSoftwareUpdate").is_none());

    // Every NOTIFY was delivered and answered 200 by the sink.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let log = sim.gena_log();
        let notified = log
            .iter()
            .filter(|e| matches!(e.event, GenaEvent::Notified { .. }))
            .count();
        let delivered = log
            .iter()
            .filter(|e| matches!(e.event, GenaEvent::Delivered { status: 200, .. }))
            .count();
        if notified == delivered || Instant::now() > deadline {
            assert_eq!(notified, delivered);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn renew_unsubscribe_and_expiry() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let mut sub = Subscriber::new();
    let url = format!("{}{RCS}", sim.player("Kitchen").unwrap().base_url);
    let s = sub
        .lan
        .subscribe_at(&url, &sub.sink.callback_url("rcs"), 60)
        .unwrap();
    assert_eq!(s.timeout_secs, 60);
    assert!(
        s.sid
            .starts_with(&format!("uuid:{}_sub", sim.player("Kitchen").unwrap().uuid))
    );
    sub.next(&s.sid);
    assert_eq!(
        sub.lan.renew_at(&url, &s.sid, 120).unwrap().timeout_secs,
        120
    );

    // Past the timeout (on the sim clock) the subscription is gone.
    sim.clock().advance(Duration::from_secs(121));
    refused_with(sub.lan.renew_at(&url, &s.sid, 120), 412);
    assert!(
        sim.gena_log()
            .iter()
            .any(|e| e.event == GenaEvent::Expired { sid: s.sid.clone() })
    );

    let s = sub
        .lan
        .subscribe_at(&url, &sub.sink.callback_url("rcs"), 300)
        .unwrap();
    sub.next(&s.sid);
    sub.lan.unsubscribe_at(&url, &s.sid).unwrap();
    refused_with(sub.lan.unsubscribe_at(&url, &s.sid), 412);
    // No events after unsubscribing.
    set_volume(&sim.transport("Kitchen").unwrap(), "44");
    assert!(sub.quiet(&s.sid, Duration::from_millis(300)));
    // A Bridge publishes no AVTransport events.
    let bridge = format!("{}{AVT}", sim.player("Bridge").unwrap().base_url);
    refused_with(
        sub.lan
            .subscribe_at(&bridge, &sub.sink.callback_url("x"), 60),
        404,
    );
}

#[test]
fn faults_are_observable() {
    let mut sim = SimHousehold::standard().spawn().unwrap();
    let kitchen = sim.transport("Kitchen").unwrap();
    let mut sub = Subscriber::new();

    // Dropped NOTIFYs spend their SEQ: the subscriber sees the gap.
    let rcs = sub.subscribe(&sim, "Kitchen", RCS, 300);
    assert_eq!(sub.next(&rcs).seq, 0);
    sim.drop_notifies("Kitchen", Some(NotifyDrop::Next(1)))
        .unwrap();
    set_volume(&kitchen, "21");
    set_volume(&kitchen, "22");
    let after = sub.next(&rcs);
    assert_eq!(after.seq, 2);
    assert_eq!(after.last_change().unwrap().unwrap().volume(), Some(22));
    assert!(sim.gena_log().iter().any(|e| e.event
        == GenaEvent::Dropped {
            sid: rcs.clone(),
            seq: 1
        }));

    // Injected UPnP faults, until cleared.
    sim.upnp_fault("Kitchen", "Play", 701).unwrap();
    match call(
        &kitchen,
        &AV_TRANSPORT,
        "Play",
        &[("InstanceID", "0"), ("Speed", "1")],
    ) {
        Err(ProtoError::SoapFault { code, .. }) => assert_eq!(code, 701),
        other => panic!("expected the injected fault, got {other:?}"),
    }
    sim.clear_faults("Kitchen").unwrap();
    play_radio(&kitchen);

    // Latency.
    sim.set_latency("Kitchen", Duration::from_millis(300))
        .unwrap();
    let started = Instant::now();
    call(
        &kitchen,
        &RENDERING_CONTROL,
        "GetVolume",
        &[("InstanceID", "0"), ("Channel", "Master")],
    )
    .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(300));
    sim.clear_faults("Kitchen").unwrap();

    // A reboot: unreachable for a while, then the old subscription is gone.
    let url = format!("{}{RCS}", sim.player("Kitchen").unwrap().base_url);
    sim.reboot("Kitchen", Duration::from_millis(400)).unwrap();
    refused_with(
        call(
            &kitchen,
            &RENDERING_CONTROL,
            "GetVolume",
            &[("InstanceID", "0"), ("Channel", "Master")],
        ),
        503,
    );
    std::thread::sleep(Duration::from_millis(450));
    refused_with(sub.lan.renew_at(&url, &rcs, 300), 412);
    let zgs = kitchen
        .soap_post(
            kitchen.ip(),
            "/ZoneGroupTopology/Control",
            &soap::soap_action_header(&soap::ZONE_GROUP_TOPOLOGY, "GetZoneGroupState"),
            &soap::envelope(&soap::ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", ""),
        )
        .unwrap();
    assert!(
        zgs.contains("BootSeq=&quot;2&quot;"),
        "the reboot shows in BootSeq"
    );

    // An address change: the old address stops answering, the new one works,
    // and the topology names the new Location.
    let lan = sim.lan();
    let old = sim.player("Office").unwrap().ip;
    let new = sim.change_address("Office").unwrap();
    assert_ne!(old, new);
    assert!(matches!(
        get_zone_group_state(&lan, old),
        Err(ProtoError::Network { .. })
    ));
    let state = get_zone_group_state(&lan, new).unwrap();
    assert!(
        state
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .any(|m| m.ip() == Some(new))
    );
    assert_eq!(sim.player("Office").unwrap().ip, new);
}

#[test]
fn reelection_and_power_off_change_the_topology() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let kitchen = sim.transport("Kitchen").unwrap();
    let office_uuid = sim.player("Office").unwrap().uuid.clone();
    let kitchen_uuid = sim.player("Kitchen").unwrap().uuid.clone();
    let office = sim.transport("Office").unwrap();
    call(
        &office,
        &AV_TRANSPORT,
        "SetAVTransportURI",
        &[
            ("InstanceID", "0"),
            ("CurrentURI", &format!("x-rincon:{kitchen_uuid}")),
            ("CurrentURIMetaData", ""),
        ],
    )
    .unwrap();
    play_radio(&kitchen);
    let mut sub = Subscriber::new();
    let zgt = sub.subscribe(&sim, "Office", ZGT, 300);
    sub.next(&zgt);

    // Re-election: Office takes over the group and its playback.
    sim.reelect_coordinator("Kitchen").unwrap();
    let n = sub.next(&zgt);
    let zgs = parse_zone_group_state(n.property("ZoneGroupState").unwrap()).unwrap();
    let group = zgs.groups.iter().find(|g| g.members.len() == 2).unwrap();
    assert_eq!(group.coordinator.0, office_uuid);
    let info = call(
        &office,
        &AV_TRANSPORT,
        "GetTransportInfo",
        &[("InstanceID", "0")],
    )
    .unwrap();
    assert_eq!(info.get("CurrentTransportState"), Some("PLAYING"));
    assert!(sim.gena_log().iter().any(|e| e.event
        == GenaEvent::CoordinatorReelected {
            to: office_uuid.clone()
        }));

    // Power-off: Kitchen answers 503 and is listed as vanished.
    sim.set_offline("Kitchen", true).unwrap();
    let n = sub.next(&zgt);
    let zgs = parse_zone_group_state(n.property("ZoneGroupState").unwrap()).unwrap();
    assert!(zgs.vanished.iter().any(|v| v.uuid.0 == kitchen_uuid));
    assert!(
        zgs.groups
            .iter()
            .flat_map(|g| &g.members)
            .all(|m| m.uuid.0 != kitchen_uuid)
    );
    refused_with(
        call(
            &kitchen,
            &RENDERING_CONTROL,
            "GetVolume",
            &[("InstanceID", "0"), ("Channel", "Master")],
        ),
        503,
    );

    // Back on: its own group again, BootSeq up.
    sim.set_offline("Kitchen", false).unwrap();
    let n = sub.next(&zgt);
    let zgs = parse_zone_group_state(n.property("ZoneGroupState").unwrap()).unwrap();
    assert_eq!(zgs.vanished.len(), 0);
    assert_eq!(zgs.groups.len(), 3);
    assert!(
        n.property("ZoneGroupState")
            .unwrap()
            .contains("BootSeq=\"2\"")
    );
}

#[test]
fn group_rendering_control_events_carry_plain_properties() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let kitchen = sim.transport("Kitchen").unwrap();
    let mut sub = Subscriber::new();
    let grc = sub.subscribe(&sim, "Kitchen", GRC, 300);
    let initial = sub.next(&grc);
    assert_eq!(initial.seq, 0);
    assert_eq!(initial.property("GroupVolume"), Some("20"));
    assert_eq!(initial.property("GroupMute"), Some("0"));
    assert_eq!(initial.property("GroupVolumeChangeable"), Some("1"));
    assert!(initial.property("LastChange").is_none());

    call(
        &kitchen,
        &soap::GROUP_RENDERING_CONTROL,
        "SetGroupVolume",
        &[("InstanceID", "0"), ("DesiredVolume", "50")],
    )
    .unwrap();
    let changed = sub.next(&grc);
    assert_eq!(
        (changed.seq, changed.property("GroupVolume")),
        (1, Some("50"))
    );
    // A room's own volume change moves the group volume too.
    set_volume(&kitchen, "30");
    assert_eq!(sub.next(&grc).property("GroupVolume"), Some("30"));
    // A Bridge has no GroupRenderingControl events.
    let bridge = format!("{}{GRC}", sim.player("Bridge").unwrap().base_url);
    refused_with(
        sub.lan
            .subscribe_at(&bridge, &sub.sink.callback_url("g"), 60),
        404,
    );
}
