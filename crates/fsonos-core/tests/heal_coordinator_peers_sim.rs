//! Coordinator recovery reads any available peer, but only in the affected
//! household and without repeating an unconfirmed command. SOAP uses sockets.

use fsonos_core::heal::{self, Recovery};
use fsonos_core::inventory::survey;
use fsonos_core::{CoreError, HouseholdState, control, grouping, resolve_room};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

struct Observed<'a> {
    inner: &'a SimLan,
    topology_reads: RefCell<Vec<IpAddr>>,
}

impl<'a> Observed<'a> {
    fn new(inner: &'a SimLan) -> Self {
        Self {
            inner,
            topology_reads: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for Observed<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#GetZoneGroupState") {
            self.topology_reads.borrow_mut().push(host);
        }
        self.inner.soap_post(host, path, action, body)
    }
}

fn grouped() -> (SimHandle, SimLan, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Living Room", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let seeds: Vec<_> = sim.players().iter().map(|p| p.ip).collect();
    let houses = survey(&lan, &seeds, Duration::ZERO).unwrap().households;
    let kitchen = resolve_room(&houses, "Kitchen").unwrap();
    let office = resolve_room(&houses, "Office").unwrap();
    assert!(grouping::group(&lan, &houses, &kitchen, &[office]).is_complete());
    let houses = survey(&lan, &seeds, Duration::ZERO).unwrap().households;
    (sim, lan, houses)
}

#[test]
fn a_peer_confirms_the_new_coordinator_when_the_member_cannot_answer_topology() {
    let (sim, lan, mut houses) = grouped();
    let member = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let office = resolve_room(&houses, "Office").unwrap().player.id.clone();
    let untouched = houses[1].clone();
    sim.reelect_coordinator("Kitchen").unwrap();
    sim.upnp_fault("Kitchen", "GetZoneGroupState", 701).unwrap();
    let observed = Observed::new(&lan);
    let attempts = Cell::new(0);

    let (level, recovery) = heal::on_coordinator(&observed, &mut houses, &member, |hs, to| {
        attempts.set(attempts.get() + 1);
        control::set_group_volume(&observed, hs, to, 21)
    })
    .unwrap();

    assert_eq!(level, 21);
    assert_eq!(attempts.get(), 2);
    assert_eq!(
        recovery,
        Recovery::CoordinatorMoved {
            from: member.clone(),
            to: office.clone(),
        }
    );
    assert_eq!(houses[0].coordinator_of(&member), Some(&office));
    assert_eq!(houses[1], untouched);
    assert_eq!(
        *observed.topology_reads.borrow(),
        [
            sim.player("Kitchen").unwrap().ip,
            sim.player("Office").unwrap().ip,
        ]
    );
}

#[test]
fn a_peer_reporting_the_same_coordinator_does_not_cause_a_second_write() {
    let (sim, lan, mut houses) = grouped();
    let member = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    sim.upnp_fault("Kitchen", "GetZoneGroupState", 701).unwrap();
    let observed = Observed::new(&lan);
    let attempts = Cell::new(0);

    let error = heal::on_coordinator(&observed, &mut houses, &member, |_, _| {
        attempts.set(attempts.get() + 1);
        Err::<(), _>(CoreError::Proto(ProtoError::SoapFault {
            code: 800,
            reason: "the media was refused".into(),
        }))
    })
    .unwrap_err();

    assert_eq!(error.to_string(), "soap fault 800: the media was refused");
    assert_eq!(attempts.get(), 1);
    assert_eq!(observed.topology_reads.borrow().len(), 2);
}

#[test]
fn unavailable_topology_never_falls_back_to_another_household() {
    let (sim, lan, mut houses) = grouped();
    let member = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let before = houses.clone();
    sim.reelect_coordinator("Kitchen").unwrap();
    for room in ["Kitchen", "Office"] {
        sim.upnp_fault(room, "GetZoneGroupState", 701).unwrap();
    }
    let observed = Observed::new(&lan);
    let attempts = Cell::new(0);

    let error = heal::on_coordinator(&observed, &mut houses, &member, |hs, to| {
        attempts.set(attempts.get() + 1);
        control::set_group_volume(&observed, hs, to, 21)
    })
    .unwrap_err();

    assert!(matches!(
        error,
        CoreError::Proto(ProtoError::SoapFault { code: 800, .. })
    ));
    assert_eq!(attempts.get(), 1);
    assert_eq!(houses, before);
    assert_eq!(
        *observed.topology_reads.borrow(),
        [
            sim.player("Kitchen").unwrap().ip,
            sim.player("Office").unwrap().ip,
        ]
    );
}

#[test]
fn a_member_vanishing_during_recovery_keeps_the_original_failure_and_model() {
    let (sim, lan, mut houses) = grouped();
    let member = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let before = houses.clone();
    let observed = Observed::new(&lan);
    let attempts = Cell::new(0);

    let error = heal::on_coordinator(&observed, &mut houses, &member, |_, _| {
        attempts.set(attempts.get() + 1);
        sim.set_offline("Kitchen", true).unwrap();
        Err::<(), _>(CoreError::Proto(ProtoError::SoapFault {
            code: 800,
            reason: "original command refused".into(),
        }))
    })
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "soap fault 800: original command refused"
    );
    assert_eq!(attempts.get(), 1);
    assert_eq!(houses, before);
    assert_eq!(observed.topology_reads.borrow().len(), 2);
}

#[test]
fn a_refused_retry_does_not_start_another_recovery_or_hide_the_new_coordinator() {
    let (sim, lan, mut houses) = grouped();
    let member = resolve_room(&houses, "Kitchen").unwrap().player.id.clone();
    let office = resolve_room(&houses, "Office").unwrap().player.id.clone();
    sim.reelect_coordinator("Kitchen").unwrap();
    sim.upnp_fault("Kitchen", "GetZoneGroupState", 701).unwrap();
    let observed = Observed::new(&lan);
    let attempts = Cell::new(0);

    let error = heal::on_coordinator(&observed, &mut houses, &member, |hs, to| {
        attempts.set(attempts.get() + 1);
        if attempts.get() == 2 {
            return Err(CoreError::Proto(ProtoError::SoapFault {
                code: 701,
                reason: "retry refused".into(),
            }));
        }
        control::set_group_volume(&observed, hs, to, 21)
    })
    .unwrap_err();

    assert_eq!(error.to_string(), "soap fault 701: retry refused");
    assert_eq!(attempts.get(), 2);
    assert_eq!(houses[0].coordinator_of(&member), Some(&office));
    assert_eq!(observed.topology_reads.borrow().len(), 2);
}
