//! Partial-group announcements return the survivor's live queue instead of
//! replaying a stale bookmark. All protocol exchanges use localhost sockets.

use fsonos_core::announce::{AnnounceReport, Announcer, Clip};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::content;
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const CLIP: &str = "http://192.0.2.250:3400/media/continuity.wav";
const NEW: &str = "http://example.invalid/newer.mp3";

fn sim(s2: bool, pair: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        if pair {
            SimPlayerSpec::pair("Alpha", model)
        } else {
            SimPlayerSpec::new("Alpha", model)
        },
        SimPlayerSpec::new("Beta", model),
        SimPlayerSpec::new("Gamma", model),
        SimPlayerSpec::new("Delta", model),
        SimPlayerSpec::new("Epsilon", model),
    ];
    let builder = SimHousehold::builder();
    if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    }
    .spawn()
    .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    let mut h = HouseholdState::default();
    h.apply_topology(&get_zone_group_state(lan, sim.player("Alpha").unwrap().ip).unwrap());
    vec![h]
}

fn host(sim: &SimHandle, room: &str) -> IpAddr {
    sim.player(room).unwrap().ip
}

fn id(sim: &SimHandle, room: &str) -> PlayerId {
    PlayerId(sim.player(room).unwrap().uuid.clone())
}

fn metadata(uri: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
        xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
        <item id=\"file\" parentID=\"-1\" restricted=\"true\"><dc:title>Queue item</dc:title>\
        <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
        <res protocolInfo=\"http-get:*:audio/mpeg:*\" duration=\"0:10:00\">{uri}</res></item></DIDL-Lite>"
    )
}

fn queue(sim: &SimHandle, lan: &SimLan, room: &str, state: TransportState) {
    let address = host(sim, room);
    for uri in [
        "http://example.invalid/a.mp3",
        "http://example.invalid/b.mp3",
        "http://example.invalid/a.mp3",
    ] {
        soap::add_uri_to_queue(lan, address, uri, &metadata(uri), false).unwrap();
    }
    soap::play_from_queue(lan, address, &id(sim, room)).unwrap();
    soap::seek_track(lan, address, 3).unwrap();
    soap::seek_position(lan, address, 40).unwrap();
    set_play_mode(lan, address, PlayMode::ShuffleNoRepeat).unwrap();
    soap::play(lan, address).unwrap();
    match state {
        TransportState::Paused => soap::pause(lan, address).unwrap(),
        TransportState::Stopped => soap::stop(lan, address).unwrap(),
        TransportState::Playing => {}
        _ => panic!("unsupported fixture state"),
    }
}

fn join(sim: &SimHandle, lan: &SimLan, room: &str, coordinator: &str) {
    soap::join_group(lan, host(sim, room), &id(sim, coordinator)).unwrap();
}

fn uris(lan: &SimLan, address: IpAddr) -> Vec<String> {
    content::browse_all(lan, address, content::QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    None,
    LiveEdit,
    LostReply,
    MalformedReply,
    Refused,
    ClipRefused,
    NoOp,
    NotApplied,
    Regroup,
    Reboot,
    SeekAfterJoin,
    NewSourceInReleasedRoom,
}

struct During<'a> {
    sim: &'a SimHandle,
    lan: &'a SimLan,
    change: Change,
    installed: AtomicBool,
    advanced: AtomicBool,
    intervened: AtomicBool,
    handbacks: AtomicUsize,
    after: AtomicUsize,
}

impl<'a> During<'a> {
    fn new(sim: &'a SimHandle, lan: &'a SimLan, change: Change) -> Self {
        Self {
            sim,
            lan,
            change,
            installed: AtomicBool::new(false),
            advanced: AtomicBool::new(false),
            intervened: AtomicBool::new(false),
            handbacks: AtomicUsize::new(0),
            after: AtomicUsize::new(0),
        }
    }

    fn uncertain(address: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: address.to_string(),
            detail: "synthetic lost native handback reply".into(),
        }
    }

    fn after_clip_read(&self) -> Result<(), ProtoError> {
        self.sim.clock().advance(Duration::from_secs(11));
        let beta = host(self.sim, "Beta");
        match self.change {
            Change::LiveEdit => {
                soap::add_uri_to_queue(self.lan, beta, NEW, &metadata(NEW), false)?;
                soap::seek_position(self.lan, beta, 75)?;
                set_play_mode(self.lan, beta, PlayMode::RepeatOne)?;
                soap::pause(self.lan, beta)?;
            }
            Change::Regroup => soap::join_group(self.lan, beta, &id(self.sim, "Gamma"))?,
            Change::Reboot => self.sim.reboot("Beta", Duration::ZERO).unwrap(),
            _ => return Ok(()),
        }
        self.intervened.store(true, Ordering::Release);
        self.after
            .store(self.sim.soap_log().len(), Ordering::Release);
        Ok(())
    }
}

impl Transport for During<'_> {
    fn soap_post(
        &self,
        address: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        let handback =
            name == "DelegateGroupCoordinationTo" && body.contains("<RejoinGroup>1</RejoinGroup>");
        if self.change == Change::ClipRefused && name == "SetAVTransportURI" && body.contains(CLIP)
        {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "synthetic clip install refusal".into(),
            });
        }
        if handback {
            self.handbacks.fetch_add(1, Ordering::AcqRel);
            match self.change {
                Change::Refused => {
                    return Err(ProtoError::SoapFault {
                        code: 701,
                        reason: "synthetic native handback refusal".into(),
                    });
                }
                Change::NotApplied => return Err(Self::uncertain(address)),
                Change::NoOp => self.sim.ignore_action("Beta", name).unwrap(),
                _ => {}
            }
        }
        if self.change == Change::NewSourceInReleasedRoom
            && self.advanced.load(Ordering::Acquire)
            && name == "GetPositionInfo"
            && address == host(self.sim, "Beta")
            && !self.intervened.swap(true, Ordering::AcqRel)
        {
            let alpha = host(self.sim, "Alpha");
            soap::set_av_transport_uri(self.lan, alpha, NEW, &metadata(NEW))?;
            soap::play(self.lan, alpha)?;
            self.after
                .store(self.sim.soap_log().len(), Ordering::Release);
        }
        let response = self.lan.soap_post(address, path, action, body);
        if response.is_ok() && name == "SetAVTransportURI" && body.contains(CLIP) {
            self.installed.store(true, Ordering::Release);
        }
        if response.is_ok()
            && self.installed.load(Ordering::Acquire)
            && name == "GetTransportInfo"
            && !self.advanced.swap(true, Ordering::AcqRel)
        {
            // Return the first Playing observation, then let the deterministic
            // clock finish the clip and advance every continuing group.
            self.after_clip_read()?;
        }
        if response.is_ok()
            && self.change == Change::SeekAfterJoin
            && self.advanced.load(Ordering::Acquire)
            && name == "SetAVTransportURI"
            && body.contains(&format!("x-rincon:{}", id(self.sim, "Beta").0))
            && !self.intervened.swap(true, Ordering::AcqRel)
        {
            soap::seek_position(self.lan, host(self.sim, "Beta"), 99)?;
            self.after
                .store(self.sim.soap_log().len(), Ordering::Release);
        }
        if handback && response.is_ok() {
            match self.change {
                Change::LostReply => return Err(Self::uncertain(address)),
                Change::MalformedReply => return Ok("<not-a-soap-response/>".into()),
                _ => {}
            }
        }
        response
    }
}

fn announce(t: &During<'_>, rooms: &[&str]) -> AnnounceReport {
    let h = houses(t.sim, t.lan);
    let targets: Vec<_> = rooms
        .iter()
        .map(|room| resolve_room(&h, room).unwrap())
        .collect();
    Announcer::with_timing(Duration::from_millis(1), Duration::from_millis(20))
        .announce(
            t,
            &targets,
            &Clip {
                url: CLIP.into(),
                title: "Continuity".into(),
                duration: Duration::from_millis(1),
            },
            25,
            &|_| 25,
        )
        .unwrap()
}

fn restored(report: &AnnounceReport) {
    let house = &report.households[0];
    assert!(house.outcome.is_ok(), "{report:?}");
    assert!(house.restore_failed.is_empty(), "{report:?}");
    assert!(
        house.restored.iter().all(|(_, r)| r.skipped.is_empty()),
        "{report:?}"
    );
}

fn setup(s2: bool, state: TransportState) -> (SimHandle, SimLan) {
    let sim = sim(s2, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha", state);
    join(&sim, &lan, "Beta", "Alpha");
    soap::set_volume(&lan, host(&sim, "Alpha"), 45).unwrap();
    soap::set_volume(&lan, host(&sim, "Beta"), 60).unwrap();
    soap::set_mute(&lan, host(&sim, "Alpha"), true).unwrap();
    (sim, lan)
}

#[test]
fn a_targeted_coordinator_returns_its_live_queue_without_rewinding_unannounced_rooms() {
    for s2 in [false, true] {
        let (sim, lan) = setup(s2, TransportState::Playing);
        let alpha = host(&sim, "Alpha");
        let before = uris(&lan, alpha);
        let from = sim.soap_log().len();
        let t = During::new(&sim, &lan, Change::None);
        restored(&announce(&t, &["Alpha"]));
        assert_eq!(t.handbacks.load(Ordering::Acquire), 1);
        let h = houses(&sim, &lan);
        assert_eq!(
            resolve_room(&h, "Beta").unwrap().coordinator.id,
            id(&sim, "Alpha")
        );
        assert_eq!(uris(&lan, alpha), before);
        let position = soap::get_position_info(&lan, alpha).unwrap();
        assert_eq!(
            position.track, 3,
            "duplicate occurrences must retain their slot"
        );
        assert_eq!(position.position_secs, Some(51));
        assert_eq!(
            soap::get_transport_info(&lan, alpha).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(
            get_play_mode(&lan, alpha).unwrap(),
            PlayMode::ShuffleNoRepeat
        );
        assert_eq!(soap::get_volume(&lan, alpha).unwrap(), 45);
        assert!(soap::get_mute(&lan, alpha).unwrap());
        assert_eq!(soap::get_volume(&lan, host(&sim, "Beta")).unwrap(), 60);
        let log = sim.soap_log();
        assert!(log[from..].iter().all(|e| !matches!(
            e.action.as_str(),
            "Seek" | "AddURIToQueue" | "RemoveAllTracksFromQueue" | "RemoveTrackRangeFromQueue"
        )));
        assert!(
            log[from..]
                .iter()
                .filter(|e| e.room == "Beta")
                .all(|e| e.action.starts_with("Get")
                    || matches!(e.action.as_str(), "Browse" | "DelegateGroupCoordinationTo"))
        );
    }
}

#[test]
fn paused_and_stopped_live_queues_remain_silent_after_the_announcement() {
    for s2 in [false, true] {
        for state in [TransportState::Paused, TransportState::Stopped] {
            let (sim, lan) = setup(s2, state);
            let alpha = host(&sim, "Alpha");
            let before = soap::get_position_info(&lan, alpha).unwrap();
            let t = During::new(&sim, &lan, Change::None);
            restored(&announce(&t, &["Alpha"]));
            assert_eq!(soap::get_transport_info(&lan, alpha).unwrap().state, state);
            let after = soap::get_position_info(&lan, alpha).unwrap();
            assert_eq!(
                (after.track, after.position_secs),
                (before.track, before.position_secs)
            );
        }
    }
}

#[test]
fn a_rejected_clip_install_still_returns_the_live_group_and_original_levels() {
    for s2 in [false, true] {
        let (sim, lan) = setup(s2, TransportState::Playing);
        let alpha = host(&sim, "Alpha");
        let before = uris(&lan, alpha);
        let t = During::new(&sim, &lan, Change::ClipRefused);
        let report = announce(&t, &["Alpha"]);
        let house = &report.households[0];
        assert!(house.outcome.is_err(), "{report:?}");
        assert!(house.restore_failed.is_empty(), "{report:?}");
        assert!(
            house.restored.iter().all(|(_, r)| r.skipped.is_empty()),
            "{report:?}"
        );
        assert!(!t.installed.load(Ordering::Acquire));
        assert_eq!(t.handbacks.load(Ordering::Acquire), 1);
        let h = houses(&sim, &lan);
        assert_eq!(
            resolve_room(&h, "Beta").unwrap().coordinator.id,
            id(&sim, "Alpha")
        );
        assert_eq!(uris(&lan, alpha), before);
        assert_eq!(
            soap::get_position_info(&lan, alpha).unwrap().position_secs,
            Some(40)
        );
        assert_eq!(
            soap::get_transport_info(&lan, alpha).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&lan, alpha).unwrap(), 45);
        assert!(soap::get_mute(&lan, alpha).unwrap());
    }
}

#[test]
fn acknowledged_joins_can_settle_before_the_clip_and_before_native_handback() {
    for s2 in [false, true] {
        let (sim, lan) = setup(s2, TransportState::Playing);
        sim.join_lag("Alpha", Duration::from_millis(20)).unwrap();
        let t = During::new(&sim, &lan, Change::None);
        // Gamma leads the clip in place; Alpha's move into that temporary
        // group and its later return to Beta both acknowledge before landing.
        restored(&announce(&t, &["Alpha", "Gamma"]));
        assert_eq!(t.handbacks.load(Ordering::Acquire), 1);
        let h = houses(&sim, &lan);
        assert_eq!(
            resolve_room(&h, "Beta").unwrap().coordinator.id,
            id(&sim, "Alpha")
        );
        assert_eq!(
            soap::get_position_info(&lan, host(&sim, "Alpha"))
                .unwrap()
                .position_secs,
            Some(51)
        );
    }
}

#[test]
fn several_partial_groups_and_a_bonded_coordinator_keep_their_independent_playback() {
    for s2 in [false, true] {
        let sim = sim(s2, true);
        let lan = sim.lan();
        queue(&sim, &lan, "Alpha", TransportState::Playing);
        queue(&sim, &lan, "Gamma", TransportState::Playing);
        join(&sim, &lan, "Beta", "Alpha");
        join(&sim, &lan, "Epsilon", "Alpha");
        join(&sim, &lan, "Delta", "Gamma");
        let t = During::new(&sim, &lan, Change::None);
        restored(&announce(&t, &["Alpha", "Epsilon", "Gamma"]));
        assert_eq!(t.handbacks.load(Ordering::Acquire), 2);
        let h = houses(&sim, &lan);
        for member in ["Alpha", "Beta", "Epsilon"] {
            assert_eq!(
                resolve_room(&h, member).unwrap().coordinator.id,
                id(&sim, "Alpha")
            );
        }
        for member in ["Gamma", "Delta"] {
            assert_eq!(
                resolve_room(&h, member).unwrap().coordinator.id,
                id(&sim, "Gamma")
            );
        }
        assert_eq!(resolve_room(&h, "Alpha").unwrap().room.players.len(), 2);
        for coordinator in ["Alpha", "Gamma"] {
            let position = soap::get_position_info(&lan, host(&sim, coordinator)).unwrap();
            assert_eq!((position.track, position.position_secs), (3, Some(51)));
            assert_eq!(uris(&lan, host(&sim, coordinator)).len(), 3);
        }
    }
}

#[test]
fn edits_and_pause_in_the_surviving_group_are_carried_back_as_the_current_state() {
    for s2 in [false, true] {
        let (sim, lan) = setup(s2, TransportState::Playing);
        let t = During::new(&sim, &lan, Change::LiveEdit);
        restored(&announce(&t, &["Alpha"]));
        assert!(t.intervened.load(Ordering::Acquire));
        let alpha = host(&sim, "Alpha");
        assert_eq!(uris(&lan, alpha).last().map(String::as_str), Some(NEW));
        assert_eq!(
            soap::get_position_info(&lan, alpha).unwrap().position_secs,
            Some(75)
        );
        assert_eq!(get_play_mode(&lan, alpha).unwrap(), PlayMode::RepeatOne);
        assert_eq!(
            soap::get_transport_info(&lan, alpha).unwrap().state,
            TransportState::Paused
        );
    }
}

#[test]
fn lost_and_malformed_native_handback_replies_are_proven_without_replay() {
    for s2 in [false, true] {
        for change in [Change::LostReply, Change::MalformedReply] {
            let (sim, lan) = setup(s2, TransportState::Playing);
            let t = During::new(&sim, &lan, change);
            restored(&announce(&t, &["Alpha"]));
            assert_eq!(t.handbacks.load(Ordering::Acquire), 1);
            assert_eq!(
                soap::get_position_info(&lan, host(&sim, "Alpha"))
                    .unwrap()
                    .position_secs,
                Some(51)
            );
        }
    }
}

#[test]
fn failed_or_unapplied_native_handback_never_replays_or_clears_the_live_queue() {
    for s2 in [false, true] {
        for change in [Change::Refused, Change::NoOp, Change::NotApplied] {
            let (sim, lan) = setup(s2, TransportState::Playing);
            let before = uris(&lan, host(&sim, "Alpha"));
            let from = sim.soap_log().len();
            let t = During::new(&sim, &lan, change);
            let report = announce(&t, &["Alpha"]);
            assert!(
                !report.households[0].restore_failed.is_empty(),
                "{report:?}"
            );
            assert_eq!(t.handbacks.load(Ordering::Acquire), 1);
            assert_eq!(uris(&lan, host(&sim, "Beta")), before);
            assert_eq!(
                soap::get_position_info(&lan, host(&sim, "Beta"))
                    .unwrap()
                    .position_secs,
                Some(51)
            );
            assert!(sim.soap_log()[from..].iter().all(|e| !matches!(
                e.action.as_str(),
                "Seek" | "AddURIToQueue" | "RemoveAllTracksFromQueue"
            )));
            if change == Change::Refused {
                assert!(
                    report.households[0]
                        .restore_failed
                        .iter()
                        .any(|(_, why)| why.contains("701"))
                );
            }
        }
    }
}

#[test]
fn changed_survivor_membership_or_boot_identity_prevents_native_restoration() {
    for s2 in [false, true] {
        for change in [Change::Regroup, Change::Reboot] {
            let (sim, lan) = setup(s2, TransportState::Playing);
            let t = During::new(&sim, &lan, change);
            let report = announce(&t, &["Alpha"]);
            assert!(t.intervened.load(Ordering::Acquire));
            assert!(
                !report.households[0].restore_failed.is_empty(),
                "{report:?}"
            );
            assert_eq!(t.handbacks.load(Ordering::Acquire), 0);
            if change == Change::Regroup {
                let h = houses(&sim, &lan);
                assert_eq!(
                    resolve_room(&h, "Beta").unwrap().coordinator.id,
                    id(&sim, "Gamma")
                );
            }
        }
    }
}

#[test]
fn ownership_changes_during_rejoin_stop_all_later_writes() {
    for s2 in [false, true] {
        for change in [Change::SeekAfterJoin, Change::NewSourceInReleasedRoom] {
            let (sim, lan) = setup(s2, TransportState::Playing);
            let t = During::new(&sim, &lan, change);
            let report = announce(&t, &["Alpha"]);
            assert!(t.intervened.load(Ordering::Acquire));
            assert!(
                !report.households[0].restore_failed.is_empty(),
                "{report:?}"
            );
            assert_eq!(t.handbacks.load(Ordering::Acquire), 0);
            assert!(
                sim.soap_log()[t.after.load(Ordering::Acquire)..]
                    .iter()
                    .all(|e| e.action.starts_with("Get") || e.action == "Browse")
            );
            match change {
                Change::SeekAfterJoin => assert_eq!(
                    soap::get_position_info(&lan, host(&sim, "Beta"))
                        .unwrap()
                        .position_secs,
                    Some(99)
                ),
                Change::NewSourceInReleasedRoom => assert_eq!(
                    soap::get_media_info(&lan, host(&sim, "Alpha")).unwrap().uri,
                    NEW
                ),
                _ => unreachable!(),
            }
        }
    }
}
