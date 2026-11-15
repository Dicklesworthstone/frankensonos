//! Discovery and command recovery admit complete topology evidence before it
//! can replace the model. Device reads use real simulator HTTP endpoints.

use fsonos_core::events::{self, Service, Subscriber, Want};
use fsonos_core::heal::{self, Recovery};
use fsonos_core::inventory::{survey, survey_known_first};
use fsonos_core::reconcile::{Health, Reconciler};
use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::gena::Subscription;
use fsonos_proto::soap::{self, ZONE_GROUP_TOPOLOGY};
use fsonos_proto::ssdp::{Advert, SONOS_ST, description_url};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerInfo, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

struct Views {
    lan: SimLan,
    advertised: Vec<Advert>,
    replies: HashMap<IpAddr, String>,
    reads: RefCell<Vec<IpAddr>>,
}

impl Views {
    fn new(sim: &SimHandle) -> Self {
        Self {
            lan: sim.lan(),
            advertised: sim
                .players()
                .iter()
                .map(|player| Advert {
                    location: description_url(player.ip),
                    st: SONOS_ST.into(),
                    usn: None,
                    household: Some(player.household.clone()),
                    boot_seq: None,
                })
                .collect(),
            replies: HashMap::new(),
            reads: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for Views {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let real = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#GetZoneGroupState\"") {
            self.reads.borrow_mut().push(host);
            if let Some(reply) = self.replies.get(&host) {
                return Ok(reply.clone());
            }
        }
        Ok(real)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }

    fn ssdp_search(&self, _: u8, _: Duration) -> Result<Vec<Advert>, ProtoError> {
        Ok(self.advertised.clone())
    }
}

// Subscription bookkeeping stays local; all evidence which changes topology
// is read from the real simulator. No callback worker is needed for this test.
impl Subscriber for Views {
    fn subscribe(&self, want: &Want, _: &str) -> Result<Subscription, ProtoError> {
        Ok(Subscription {
            sid: format!("uuid:{}-{}", want.player.0, want.service.tag()),
            timeout_secs: 1800,
        })
    }

    fn renew(&self, want: &Want, _: &str) -> Result<Subscription, ProtoError> {
        self.subscribe(want, "")
    }

    fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
        Ok(())
    }
}

fn callback(service: Service) -> String {
    format!("http://127.0.0.1:3400/{}", service.tag())
}

fn fixture() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("First", SimModel::Play5Gen1),
            SimPlayerSpec::new("Peer", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Other", SimModel::One)])
        .spawn()
        .unwrap()
}

fn member(player: &SimPlayerInfo) -> String {
    format!(
        "<ZoneGroupMember UUID=\"{}\" Location=\"http://{}:1400/xml/device_description.xml\" \
         ZoneName=\"{}\" SWGen=\"{}\" BootSeq=\"999\"/>",
        player.uuid, player.ip, player.room, player.generation
    )
}

fn group(coordinator: &str, members: &str) -> String {
    format!("<ZoneGroup Coordinator=\"{coordinator}\">{members}</ZoneGroup>")
}

fn reply(groups: &str, vanished: &str) -> String {
    let document = format!(
        "<ZoneGroupState><ZoneGroups>{groups}</ZoneGroups><VanishedDevices>{vanished}</VanishedDevices></ZoneGroupState>"
    );
    soap::envelope(
        &ZONE_GROUP_TOPOLOGY,
        "GetZoneGroupStateResponse",
        &soap::args_xml(&[("ZoneGroupState", &document)]),
    )
}

fn model(views: &Views) -> Vec<HouseholdState> {
    survey(views, &[], Duration::ZERO).unwrap().households
}

#[test]
fn invalid_discovery_topology_yields_to_a_healthy_peer_without_importing_its_evidence() {
    let sim = fixture();
    let first = sim.player("First").unwrap();
    let peer = sim.player("Peer").unwrap();
    let other = sim.player("Other").unwrap();
    let a = member(first);
    let b = member(peer);
    let good = model(&Views::new(&sim));
    let cases = [
        reply("", ""),
        reply(&group(&peer.uuid, &b), ""),
        reply(&group("missing-owner", &a), ""),
        reply(&(group(&first.uuid, &a) + &group(&first.uuid, &a)), ""),
        reply(&group(&first.uuid, &(a.clone() + &a)), ""),
        reply(
            &group(
                &first.uuid,
                &(a.clone() + &b.replace(&peer.ip.to_string(), &first.ip.to_string())),
            ),
            "",
        ),
        reply(
            &group(&first.uuid, &a),
            &format!("<Device UUID=\"{}\"/>", first.uuid),
        ),
        reply(&group(&first.uuid, &(a.clone() + &member(other))), ""),
        reply(
            &group(
                &first.uuid,
                &a.replace(
                    &format!("http://{}:1400/xml/device_description.xml", first.ip),
                    "unusable",
                ),
            ),
            "",
        ),
        "not XML".into(),
    ];
    for invalid in cases {
        let mut views = Views::new(&sim);
        views.replies.insert(first.ip, invalid);
        let found = survey(&views, &[], Duration::ZERO).unwrap();
        assert_eq!(found.households, good);
        assert_eq!(found.rejected_topology.len(), 1);
        assert_eq!(found.rejected_topology[0].0.0, first.uuid);
        assert_eq!(found.unreachable.len(), 1);
        assert!(found.boot_seqs.iter().all(|(_, boot)| *boot != 999));
        assert!(found.vanished.is_empty());
        assert_eq!(*views.reads.borrow(), [first.ip, peer.ip, other.ip]);
    }
}

#[test]
fn an_invalid_later_view_cannot_replace_an_accepted_household_or_boot_evidence() {
    let sim = fixture();
    sim.set_offline("Peer", true).unwrap();
    let first = sim.player("First").unwrap();
    let peer = sim.player("Peer").unwrap();
    let partial = sim
        .lan()
        .soap_post(
            first.ip,
            ZONE_GROUP_TOPOLOGY.control_path,
            &soap::soap_action_header(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState"),
            &soap::envelope(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", ""),
        )
        .unwrap();
    sim.set_offline("Peer", false).unwrap();
    let mut views = Views::new(&sim);
    views.replies.insert(first.ip, partial);
    views.replies.insert(
        peer.ip,
        reply(
            &group(
                &peer.uuid,
                &(member(first) + &member(first) + &member(peer)),
            ),
            "",
        ),
    );
    let found = survey(&views, &[], Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 2);
    assert!(resolve_room(&found.households, "First").is_ok());
    assert!(resolve_room(&found.households, "Peer").is_err());
    assert_eq!(found.households[0].players.len(), 1);
    assert_eq!(found.rejected_topology.len(), 1);
    assert!(found.boot_seqs.iter().all(|(_, boot)| *boot != 999));
    assert_eq!(found.vanished.len(), 1);
    assert_eq!(found.vanished[0].uuid.0, peer.uuid);
}

#[test]
fn a_malformed_household_keeps_its_last_model_and_leases_while_the_other_advances() {
    let mut sim = fixture();
    let mut views = Views::new(&sim);
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), now);
    rec.refresh(&views, &[], callback, now).unwrap();
    let legacy = rec.households[0].clone();
    let leases = rec.subscriptions.len();
    let first = sim.player("First").unwrap().clone();
    let peer = sim.player("Peer").unwrap().clone();
    let last_ok = rec
        .health
        .of(&PlayerId(first.uuid.clone()))
        .unwrap()
        .last_ok;
    let other = sim.change_address("Other").unwrap();
    views.advertised = Views::new(&sim).advertised;
    for player in [&first, &peer] {
        views.replies.insert(player.ip, reply("", ""));
    }
    let at = now + Duration::from_secs(1);
    let result = rec.refresh(&views, &[], callback, at).unwrap();
    assert_eq!(rec.households[0], legacy);
    assert_eq!(
        resolve_room(&rec.households, "Other").unwrap().player.ip,
        other
    );
    assert!(result.missing.is_empty());
    assert_eq!(rec.subscriptions.len(), leases);
    let health = rec.health.of(&PlayerId(first.uuid.clone())).unwrap();
    assert_eq!(
        health.last_ok, last_ok,
        "retained evidence is not a successful read"
    );
    assert_eq!(health.health, Health::Degraded);
    assert!(rec.schedule.next_at() <= at + events::RETRY);

    // A later coherent departure remains authoritative and retires its lease.
    views.replies.clear();
    sim.set_offline("Peer", true).unwrap();
    let result = rec
        .refresh(&views, &[], callback, at + events::RETRY)
        .unwrap();
    assert!(result.missing.contains(&PlayerId(peer.uuid.clone())));
    assert!(resolve_room(&rec.households, "Peer").is_err());
    assert!(rec.subscriptions.len() < leases);
}

#[test]
fn bridge_and_invisible_pair_responders_still_bootstrap_the_complete_household() {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Bridge", SimModel::Bridge),
            SimPlayerSpec::pair("Pair", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap();
    for responder in sim
        .players()
        .iter()
        .filter(|player| player.hidden || player.model == SimModel::Bridge)
    {
        let mut views = Views::new(&sim);
        views
            .advertised
            .retain(|advert| advert.location == description_url(responder.ip));
        let found = survey_known_first(&views, &[responder.ip], Vec::new, Duration::ZERO).unwrap();
        assert_eq!(found.households.len(), 1);
        assert_eq!(found.households[0].players.len(), 2);
        assert_eq!(found.households[0].rooms.len(), 1);
        assert!(found.rejected_topology.is_empty());
        assert!(found.unreachable.is_empty());
    }
}

#[test]
fn coordinator_recovery_ignores_incoherent_or_foreign_views_before_one_proven_retry() {
    let sim = fixture();
    let mut views = Views::new(&sim);
    let mut houses = model(&views);
    let first = resolve_room(&houses, "First").unwrap().player.id.clone();
    let peer = resolve_room(&houses, "Peer").unwrap().player.id.clone();
    control::join(&views, &houses, &peer, &first).unwrap();
    houses = model(&views);
    let untouched = houses[1].clone();
    sim.reelect_coordinator("First").unwrap();
    let bad = reply(
        &group(
            &sim.player("Other").unwrap().uuid,
            &(member(sim.player("First").unwrap()) + &member(sim.player("Other").unwrap())),
        ),
        "",
    );
    views.replies.insert(sim.player("First").unwrap().ip, bad);
    views.reads.borrow_mut().clear();
    let attempts = Cell::new(0);
    let (_, recovery) = heal::on_coordinator(&views, &mut houses, &first, |model, coordinator| {
        attempts.set(attempts.get() + 1);
        control::set_group_volume(&views, model, coordinator, 19)
    })
    .unwrap();
    assert_eq!(attempts.get(), 2);
    assert_eq!(
        recovery,
        Recovery::CoordinatorMoved {
            from: first,
            to: peer
        }
    );
    assert_eq!(houses[1], untouched);
    assert_eq!(
        *views.reads.borrow(),
        [
            sim.player("First").unwrap().ip,
            sim.player("Peer").unwrap().ip
        ]
    );
}

#[test]
fn wholly_invalid_recovery_preserves_the_original_failure_and_household() {
    let sim = fixture();
    let mut views = Views::new(&sim);
    let mut houses = model(&views);
    let before = houses.clone();
    let first = resolve_room(&houses, "First").unwrap().player.id.clone();
    for room in ["First", "Peer"] {
        views
            .replies
            .insert(sim.player(room).unwrap().ip, reply("", ""));
    }
    let attempts = Cell::new(0);
    let error = heal::on_coordinator(&views, &mut houses, &first, |_, _| {
        attempts.set(attempts.get() + 1);
        Err::<(), _>(CoreError::Proto(ProtoError::SoapFault {
            code: 800,
            reason: "original refusal".into(),
        }))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "soap fault 800: original refusal");
    assert_eq!(attempts.get(), 1);
    assert_eq!(houses, before);
}

#[test]
fn fresh_descriptions_repair_stale_topology_addresses_during_command_recovery() {
    let mut sim = fixture();
    let original = Views::new(&sim);
    let mut houses = model(&original);
    let first = resolve_room(&houses, "First").unwrap().player.id.clone();
    let old_address = control::locate(&houses, &first).unwrap().ip;
    let stale = original
        .lan
        .soap_post(
            old_address,
            ZONE_GROUP_TOPOLOGY.control_path,
            &soap::soap_action_header(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState"),
            &soap::envelope(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", ""),
        )
        .unwrap();
    let new_address = sim.change_address("First").unwrap();
    let mut views = Views::new(&sim);
    views.replies.insert(new_address, stale);
    let untouched = houses[1].clone();
    let calls = Cell::new(0);
    let (_, recovery) = heal::readdressing(&views, &[], &mut houses, &first, |model| {
        calls.set(calls.get() + 1);
        control::set_volume(&views, model, &first, 23)
    })
    .unwrap();
    assert_eq!(
        recovery,
        Recovery::Readdressed {
            player: first.clone(),
            from: old_address,
            to: new_address
        }
    );
    assert_eq!(calls.get(), 2);
    assert_eq!(control::locate(&houses, &first).unwrap().ip, new_address);
    assert_eq!(houses[1], untouched);
}

#[test]
fn malformed_replies_from_every_household_preserve_state_without_new_health_evidence() {
    let sim = fixture();
    let mut views = Views::new(&sim);
    let now = Instant::now();
    let mut rec = Reconciler::new(Duration::from_mins(5), Duration::from_mins(30), now);
    rec.refresh(&views, &[], callback, now).unwrap();
    let before = rec.households.clone();
    let leases = rec.subscriptions.len();
    for player in sim.players() {
        views.replies.insert(player.ip, reply("", ""));
    }
    let report = rec
        .refresh(&views, &[], callback, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(rec.households, before);
    assert_eq!(rec.subscriptions.len(), leases);
    assert!(report.missing.is_empty());
    assert!(report.rebooted.is_empty());
    for player in sim.players() {
        let health = rec.health.of(&PlayerId(player.uuid.clone())).unwrap();
        assert_eq!(health.last_ok, Some(now));
        assert_eq!(health.health, Health::Degraded);
    }
}
