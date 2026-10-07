//! `inventory::survey` end to end over a fake LAN built from the scrubbed S1
//! and S2 fixtures: SSDP replies, device descriptions, and one
//! ZoneGroupTopology read per household.

use fsonos_core::inventory::survey;
use fsonos_proto::{ProtoError, Transport, ssdp};
use fsonos_types::{Generation, PlayerId};
use std::cell::RefCell;
use std::net::IpAddr;
use std::time::Duration;

const ZGS_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s1.xml");
const ZGS_S2: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s2.xml");
const DESC_S1_BRIDGE: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s1_bridge.xml");
const DESC_S1_PLAY5: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s1_play5.xml");
const DESC_S2_PLAY1: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s2_play1.xml");
const DESC_S2_ONE: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s2_one.xml");

/// The players the fake LAN knows: address, description, household.
const LAN: [(&str, &str, &str); 4] = [
    ("192.0.2.10", DESC_S1_BRIDGE, "Sonos_S1Household"),
    ("192.0.2.11", DESC_S1_PLAY5, "Sonos_S1Household"),
    ("192.0.2.19", DESC_S2_PLAY1, "Sonos_S2Household"),
    ("192.0.2.21", DESC_S2_ONE, "Sonos_S2Household"),
];

struct FakeLan {
    /// Answer SSDP at all (false simulates a network that drops multicast).
    ssdp: bool,
    /// An address that is advertised but never answers.
    dead: Option<&'static str>,
    topology_reads: RefCell<Vec<IpAddr>>,
}

impl FakeLan {
    fn new() -> Self {
        Self {
            ssdp: true,
            dead: None,
            topology_reads: RefCell::new(Vec::new()),
        }
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

impl Transport for FakeLan {
    fn soap_post(
        &self,
        host: IpAddr,
        _control_path: &str,
        soap_action: &str,
        _body: &str,
    ) -> Result<String, ProtoError> {
        assert!(
            soap_action.ends_with("#GetZoneGroupState\""),
            "{soap_action}"
        );
        self.topology_reads.borrow_mut().push(host);
        let s1 = LAN[..2].iter().any(|(a, ..)| ip(a) == host);
        Ok(if s1 { ZGS_S1 } else { ZGS_S2 }.to_string())
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        LAN.iter()
            .find(|(a, ..)| ssdp::description_url(ip(a)) == url && self.dead != Some(*a))
            .map(|(_, desc, _)| desc.to_string())
            .ok_or_else(|| ProtoError::Network {
                target: url.into(),
                detail: "connection refused".into(),
            })
    }

    fn ssdp_search(&self, _mx: u8, _wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        if !self.ssdp {
            return Err(ProtoError::Network {
                target: "SSDP".into(),
                detail: "network unreachable".into(),
            });
        }
        let mut adverts: Vec<ssdp::Advert> = LAN
            .iter()
            .map(|(a, _, household)| ssdp::Advert {
                location: ssdp::description_url(ip(a)),
                st: ssdp::SONOS_ST.into(),
                usn: None,
                household: Some((*household).into()),
                boot_seq: None,
            })
            .collect();
        if let Some(dead) = self.dead {
            adverts.push(ssdp::Advert {
                location: ssdp::description_url(ip(dead)),
                st: ssdp::SONOS_ST.into(),
                usn: None,
                household: None,
                boot_seq: None,
            });
        }
        Ok(adverts)
    }
}

fn pid(n: u8) -> PlayerId {
    PlayerId(format!("RINCON_000E58A000{n:02X}01400"))
}

#[test]
fn finds_both_households_with_one_topology_read_each() {
    let lan = FakeLan::new();
    let found = survey(&lan, &[], Duration::from_millis(1)).unwrap();
    assert!(found.unreachable.is_empty(), "{:?}", found.unreachable);
    assert_eq!(found.households.len(), 2);

    let s1 = &found.households[0];
    assert_eq!(s1.generation(), Some(Generation::S1));
    assert_eq!(
        s1.id.as_ref().map(|h| h.0.as_str()),
        Some("Sonos_S1Household")
    );
    assert_eq!(s1.groups.len(), 2, "bridges are not groups");
    assert_eq!(s1.players.len(), 7, "bridges are not players");
    // The described Play:5 carries its model; topology alone does not.
    let play5 = s1.player(&pid(4)).unwrap();
    assert_eq!(play5.model, "Sonos Play:5");
    assert_eq!(play5.ip, ip("192.0.2.11"));

    let s2 = &found.households[1];
    assert_eq!(s2.generation(), Some(Generation::S2));
    assert_eq!(s2.player(&pid(0x0A)).unwrap().model, "Sonos Play:1");

    // The bridge answered first for S1 and covered the whole household.
    assert_eq!(
        *lan.topology_reads.borrow(),
        [ip("192.0.2.10"), ip("192.0.2.19")]
    );
}

#[test]
fn an_unreachable_player_is_reported_not_fatal() {
    let lan = FakeLan {
        dead: Some("192.0.2.99"),
        ..FakeLan::new()
    };
    let found = survey(&lan, &[], Duration::from_millis(1)).unwrap();
    assert_eq!(found.households.len(), 2);
    assert_eq!(found.unreachable.len(), 1);
    assert_eq!(found.unreachable[0].0, "192.0.2.99");
    assert!(found.unreachable[0].1.contains("connection refused"));
}

#[test]
fn seeds_stand_in_when_ssdp_fails() {
    let lan = FakeLan {
        ssdp: false,
        ..FakeLan::new()
    };
    let found = survey(&lan, &[ip("192.0.2.21")], Duration::from_millis(1)).unwrap();
    assert!(
        found
            .ssdp_error
            .as_deref()
            .unwrap()
            .contains("network unreachable")
    );
    assert_eq!(found.households.len(), 1);
    assert_eq!(found.households[0].generation(), Some(Generation::S2));
    assert!(
        found.households[0].id.is_none(),
        "seeds carry no household header"
    );

    // Without seeds an SSDP failure is the survey's failure.
    assert!(survey(&lan, &[], Duration::from_millis(1)).is_err());
}
