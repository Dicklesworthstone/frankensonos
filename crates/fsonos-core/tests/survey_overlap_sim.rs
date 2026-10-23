//! Different peers can serve different generations of one household's full
//! topology. Discovery must publish one coherent household, not duplicates.

use fsonos_core::inventory::{survey, survey_known};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::soap::{self, ZONE_GROUP_TOPOLOGY};
use fsonos_proto::ssdp::{Advert, SONOS_ST, description_url};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::HouseholdId;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::Duration;

struct Views {
    lan: SimLan,
    advertised: Vec<(IpAddr, Option<String>)>,
    stale: HashMap<IpAddr, String>,
    reads: RefCell<Vec<IpAddr>>,
}

impl Views {
    fn new(sim: &SimHandle, names: &[(&str, Option<&str>)]) -> Self {
        Self {
            lan: sim.lan(),
            advertised: names
                .iter()
                .map(|(room, id)| (sim.player(room).unwrap().ip, id.map(str::to_string)))
                .collect(),
            stale: HashMap::new(),
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
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#GetZoneGroupState\"") {
            self.reads.borrow_mut().push(host);
            if let Some(stale) = self.stale.get(&host) {
                return Ok(stale.clone());
            }
        }
        Ok(response)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }

    fn ssdp_search(&self, _: u8, _: Duration) -> Result<Vec<Advert>, ProtoError> {
        Ok(self
            .advertised
            .iter()
            .map(|(host, id)| Advert {
                location: description_url(*host),
                st: SONOS_ST.into(),
                usn: None,
                household: id.clone(),
                boot_seq: None,
            })
            .collect())
    }
}

fn response(sim: &SimHandle, room: &str) -> String {
    let service = &ZONE_GROUP_TOPOLOGY;
    sim.lan()
        .soap_post(
            sim.player(room).unwrap().ip,
            service.control_path,
            &soap::soap_action_header(service, "GetZoneGroupState"),
            &soap::envelope(service, "GetZoneGroupState", ""),
        )
        .unwrap()
}

fn mixed() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("First", SimModel::Play5Gen1),
            SimPlayerSpec::new("Returned", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Other household", SimModel::One)])
        .spawn()
        .unwrap()
}

/// Three formerly isolated views of one household. Discovery returns two
/// stale singletons; only the remembered third peer serves the current view.
fn cross_pass_views() -> (SimHandle, Views, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s2([
            SimPlayerSpec::new("First", SimModel::One),
            SimPlayerSpec::new("Second", SimModel::One),
            SimPlayerSpec::new("Third", SimModel::One),
        ])
        .spawn()
        .unwrap();
    let snapshot = |room| {
        survey(&Views::new(&sim, &[(room, None)]), &[], Duration::ZERO)
            .unwrap()
            .households
            .remove(0)
    };
    sim.set_offline("First", true).unwrap();
    sim.set_offline("Second", true).unwrap();
    let third = snapshot("Third");
    sim.set_offline("Third", true).unwrap();
    sim.set_offline("First", false).unwrap();
    let first = snapshot("First");
    let first_response = response(&sim, "First");
    sim.set_offline("First", true).unwrap();
    sim.set_offline("Second", false).unwrap();
    let second = snapshot("Second");
    let second_response = response(&sim, "Second");
    sim.set_offline("First", false).unwrap();
    sim.set_offline("Third", false).unwrap();
    let mut views = Views::new(&sim, &[("First", None), ("Second", None)]);
    views
        .stale
        .insert(sim.player("First").unwrap().ip, first_response);
    views
        .stale
        .insert(sim.player("Second").unwrap().ip, second_response);
    (sim, views, vec![first, second, third])
}

#[test]
fn a_later_full_household_replaces_an_earlier_partial_view_and_its_evidence() {
    for named in [false, true] {
        let sim = mixed();
        sim.set_offline("Returned", true).unwrap();
        let stale = response(&sim, "First");
        sim.set_offline("Returned", false).unwrap();
        // The newer response must also replace the stale BootSeq for First.
        sim.set_offline("First", true).unwrap();
        sim.set_offline("First", false).unwrap();
        let mut views = Views::new(
            &sim,
            &[
                ("First", named.then_some("synthetic-s1")),
                ("Other household", named.then_some("synthetic-s2")),
                ("Returned", named.then_some("synthetic-s1")),
            ],
        );
        views.stale.insert(sim.player("First").unwrap().ip, stale);
        let found = survey(&views, &[], Duration::ZERO).unwrap();
        assert!(found.unreachable.is_empty(), "{:?}", found.unreachable);
        assert_eq!(found.households.len(), 2, "one snapshot per household");
        let players: Vec<_> = found.households.iter().flat_map(|h| &h.players).collect();
        assert_eq!(players.len(), 3);
        let ids: HashSet<_> = players.iter().map(|p| &p.id).collect();
        assert_eq!(ids.len(), players.len(), "no duplicated player identity");
        for name in ["First", "Returned", "Other household"] {
            assert!(
                resolve_room(&found.households, name).is_ok(),
                "{name} stays unambiguous"
            );
        }
        assert!(
            found.vanished.is_empty(),
            "a live player is not also vanished"
        );
        assert_eq!(
            found.boot_seqs.len(),
            3,
            "no boot evidence from the superseded view"
        );
        let first = &resolve_room(&found.households, "First").unwrap().player.id;
        assert_eq!(
            found
                .boot_seqs
                .iter()
                .find(|(id, _)| id == first)
                .unwrap()
                .1,
            2
        );
        assert_eq!(
            views.reads.borrow().len(),
            3,
            "each uncovered peer was read once"
        );
        assert_eq!(
            found.households[0].id.as_ref().map(|id| id.0.as_str()),
            named.then_some("synthetic-s1")
        );
    }
}

#[test]
fn one_new_observation_reconciles_transitively_overlapping_anonymous_views() {
    let sim = SimHousehold::builder()
        .s2([
            SimPlayerSpec::new("First", SimModel::One),
            SimPlayerSpec::new("Second", SimModel::One),
            SimPlayerSpec::new("Third", SimModel::One),
        ])
        .spawn()
        .unwrap();
    sim.set_offline("Second", true).unwrap();
    sim.set_offline("Third", true).unwrap();
    let first = response(&sim, "First");
    sim.set_offline("Second", false).unwrap();
    sim.set_offline("First", true).unwrap();
    let second = response(&sim, "Second");
    sim.set_offline("First", false).unwrap();
    sim.set_offline("Third", false).unwrap();
    let mut views = Views::new(&sim, &[("First", None), ("Second", None), ("Third", None)]);
    views.stale.insert(sim.player("First").unwrap().ip, first);
    views.stale.insert(sim.player("Second").unwrap().ip, second);
    let found = survey(&views, &[], Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 1);
    assert_eq!(found.households[0].players.len(), 3);
    assert_eq!(
        found.households[0].groups.len(),
        3,
        "the latest full graph is retained"
    );
    assert_eq!(found.boot_seqs.len(), 3);
    assert!(found.vanished.is_empty());
    assert!(found.unreachable.is_empty());
}

#[test]
fn a_later_coherent_view_removes_members_that_have_since_vanished() {
    let sim = SimHousehold::builder()
        .s2([
            SimPlayerSpec::new("First", SimModel::One),
            SimPlayerSpec::new("Departed", SimModel::One),
            SimPlayerSpec::new("Returned", SimModel::One),
        ])
        .spawn()
        .unwrap();
    sim.set_offline("Returned", true).unwrap();
    let first = response(&sim, "First");
    sim.set_offline("Returned", false).unwrap();
    sim.set_offline("Departed", true).unwrap();
    let mut views = Views::new(&sim, &[("First", None), ("Returned", None)]);
    views.stale.insert(sim.player("First").unwrap().ip, first);
    let found = survey(&views, &[], Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 1);
    assert_eq!(found.households[0].players.len(), 2);
    assert_eq!(found.households[0].groups.len(), 2);
    assert!(resolve_room(&found.households, "First").is_ok());
    assert!(resolve_room(&found.households, "Returned").is_ok());
    assert!(resolve_room(&found.households, "Departed").is_err());
    assert_eq!(found.boot_seqs.len(), 2);
    assert_eq!(found.vanished.len(), 1);
    assert_eq!(
        found.vanished[0].uuid.0,
        sim.player("Departed").unwrap().uuid
    );
    assert!(found.unreachable.is_empty());
}

#[test]
fn a_remembered_peer_replaces_all_overlapping_discovery_observations() {
    for named in [false, true] {
        let (sim, views, mut previous) = cross_pass_views();
        let mut third = previous.pop().unwrap();
        third.id = named.then(|| HouseholdId("synthetic-known".into()));
        let found = survey_known(&views, &[], &[third], Duration::ZERO).unwrap();
        assert_eq!(found.households.len(), 1);
        assert_eq!(found.households[0].players.len(), 3);
        assert_eq!(found.households[0].groups.len(), 3);
        assert_eq!(
            found.households[0].id.as_ref().map(|id| id.0.as_str()),
            named.then_some("synthetic-known")
        );
        for room in ["First", "Second", "Third"] {
            let resolved = resolve_room(&found.households, room).unwrap();
            assert!(!resolved.player.model.is_empty(), "description for {room}");
        }
        assert_eq!(found.boot_seqs.len(), 3);
        assert_eq!(
            found
                .boot_seqs
                .iter()
                .find(|(id, _)| id.0 == sim.player("First").unwrap().uuid)
                .unwrap()
                .1,
            3,
            "the fresh fallback replaces earlier boot evidence"
        );
        assert!(found.vanished.is_empty());
        assert!(found.unreachable.is_empty());
        assert_eq!(
            *views.reads.borrow(),
            [
                sim.player("First").unwrap().ip,
                sim.player("Second").unwrap().ip,
                sim.player("Third").unwrap().ip
            ]
        );
    }
}

#[test]
fn remembered_identity_is_applied_before_partial_observations_are_adopted() {
    let (sim, views, _) = cross_pass_views();
    let mut known = survey(&Views::new(&sim, &[("Third", None)]), &[], Duration::ZERO)
        .unwrap()
        .households;
    known[0].id = Some(HouseholdId("synthetic-known".into()));
    let found = survey_known(&views, &[], &known, Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 1);
    assert_eq!(found.households[0].id, known[0].id);
    assert_eq!(found.households[0].players.len(), 1);
    assert!(resolve_room(&found.households, "Second").is_ok());
    assert_eq!(found.boot_seqs.len(), 1);
    assert_eq!(found.vanished.len(), 2);
    assert_eq!(
        *views.reads.borrow(),
        [
            sim.player("First").unwrap().ip,
            sim.player("Second").unwrap().ip
        ],
        "the latest coherent response remains authoritative for this household"
    );
    assert!(found.unreachable.is_empty());
}

#[test]
fn a_conflicting_fallback_keeps_both_discovery_snapshots_and_their_evidence() {
    let (sim, views, mut known) = cross_pass_views();
    known[0].id = Some(HouseholdId("synthetic-first".into()));
    known[1].id = Some(HouseholdId("synthetic-second".into()));
    let found = survey_known(&views, &[], &known, Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 2);
    assert_eq!(found.households[0].id, known[0].id);
    assert_eq!(found.households[1].id, known[1].id);
    assert!(found.households.iter().all(|h| h.players.len() == 1));
    assert!(resolve_room(&found.households, "First").is_ok());
    assert!(resolve_room(&found.households, "Second").is_ok());
    assert!(resolve_room(&found.households, "Third").is_err());
    assert_eq!(found.boot_seqs.len(), 2);
    assert_eq!(
        found
            .boot_seqs
            .iter()
            .find(|(id, _)| id.0 == sim.player("First").unwrap().uuid)
            .unwrap()
            .1,
        2,
        "rejected fallback evidence cannot replace an accepted response"
    );
    assert_eq!(found.vanished.len(), 1);
    assert_eq!(found.vanished[0].uuid.0, sim.player("Third").unwrap().uuid);
    assert_eq!(found.unreachable.len(), 1);
    assert_eq!(
        found.unreachable[0].0,
        sim.player("Third").unwrap().ip.to_string()
    );
    assert!(
        found.unreachable[0]
            .1
            .contains("conflicts with household identity hints")
    );
}

#[test]
fn current_advertisement_replaces_a_partial_views_inherited_cached_identity() {
    let sim = mixed();
    sim.set_offline("Returned", true).unwrap();
    let mut known = survey(&Views::new(&sim, &[("First", None)]), &[], Duration::ZERO)
        .unwrap()
        .households;
    known[0].id = Some(HouseholdId("synthetic-cached-old".into()));
    let stale = response(&sim, "First");
    sim.set_offline("Returned", false).unwrap();
    sim.set_offline("First", true).unwrap();
    sim.set_offline("First", false).unwrap();
    let mut views = Views::new(
        &sim,
        &[("First", None), ("Returned", Some("synthetic-current"))],
    );
    views.stale.insert(sim.player("First").unwrap().ip, stale);
    let found = survey_known(&views, &[], &known, Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 1);
    assert_eq!(
        found.households[0].id,
        Some(HouseholdId("synthetic-current".into()))
    );
    assert_eq!(found.households[0].players.len(), 2);
    assert!(resolve_room(&found.households, "First").is_ok());
    assert!(resolve_room(&found.households, "Returned").is_ok());
    assert_eq!(found.boot_seqs.len(), 2);
    assert_eq!(
        found
            .boot_seqs
            .iter()
            .find(|(id, _)| id.0 == sim.player("First").unwrap().uuid)
            .unwrap()
            .1,
        2
    );
    assert!(found.vanished.is_empty());
    assert!(found.unreachable.is_empty());
    assert_eq!(
        *views.reads.borrow(),
        [
            sim.player("First").unwrap().ip,
            sim.player("Returned").unwrap().ip
        ]
    );
}

#[test]
fn conflicting_advertised_identities_are_reported_instead_of_merged() {
    let sim = mixed();
    sim.set_offline("Returned", true).unwrap();
    let stale = response(&sim, "First");
    sim.set_offline("Returned", false).unwrap();
    let mut views = Views::new(
        &sim,
        &[
            ("First", Some("synthetic-original")),
            ("Returned", Some("synthetic-conflict")),
            ("Other household", Some("synthetic-unrelated")),
        ],
    );
    views.stale.insert(sim.player("First").unwrap().ip, stale);
    let found = survey(&views, &[], Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 2);
    assert_eq!(
        found.households[0].id.as_ref().unwrap().0,
        "synthetic-original"
    );
    assert_eq!(found.households[0].players.len(), 1);
    assert!(resolve_room(&found.households, "First").is_ok());
    assert!(resolve_room(&found.households, "Other household").is_ok());
    assert!(resolve_room(&found.households, "Returned").is_err());
    assert_eq!(found.unreachable.len(), 1);
    assert_eq!(
        found.unreachable[0].0,
        sim.player("Returned").unwrap().ip.to_string()
    );
    assert!(
        found.unreachable[0]
            .1
            .contains("conflicts with household identity hints")
    );
}

#[test]
fn an_ordinary_complete_household_still_needs_one_topology_read() {
    let sim = mixed();
    let views = Views::new(
        &sim,
        &[
            ("First", None),
            ("Returned", None),
            ("Other household", None),
        ],
    );
    let found = survey(&views, &[], Duration::ZERO).unwrap();
    assert_eq!(found.households.len(), 2);
    assert_eq!(
        *views.reads.borrow(),
        [
            sim.player("First").unwrap().ip,
            sim.player("Other household").unwrap().ip
        ]
    );
    assert!(found.unreachable.is_empty());
}
