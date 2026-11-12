//! Startup discovery must not discard a reachable household when another
//! remembered player is offline and multicast covers only part of the house.

use fsonos_core::inventory::{Survey, survey_known_first};
use fsonos_proto::{ProtoError, Transport, ssdp};
use fsonos_types::{Generation, PlayerId};
use std::cell::RefCell;
use std::net::IpAddr;
use std::time::Duration;

const ZGS_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s1.xml");
const ZGS_S2: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s2.xml");
const DESC_S1: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s1_play5.xml");
const DESC_S2: &str =
    include_str!("../../fsonos-proto/tests/fixtures/device_description_s2_one.xml");

fn ip(value: &str) -> IpAddr {
    value.parse().unwrap()
}

fn offline(target: String) -> ProtoError {
    ProtoError::Network {
        target,
        detail: "connection refused".into(),
    }
}

/// Both households answer direct reads; only S1 can advertise. A zero-length
/// discovery window observes no advertisements, matching the known-first path.
struct StartupLan {
    ssdp: bool,
    s1: IpAddr,
    waits: RefCell<Vec<Duration>>,
}

impl StartupLan {
    fn new() -> Self {
        Self {
            ssdp: true,
            s1: ip("192.0.2.11"),
            waits: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for StartupLan {
    fn soap_post(
        &self,
        host: IpAddr,
        _control_path: &str,
        action: &str,
        _body: &str,
    ) -> Result<String, ProtoError> {
        assert!(action.ends_with("#GetZoneGroupState\""), "{action}");
        if host == self.s1 {
            Ok(ZGS_S1.replace(
                "http://192.0.2.11:1400",
                &format!("http://{}:1400", self.s1),
            ))
        } else if host == ip("192.0.2.21") {
            Ok(ZGS_S2.into())
        } else {
            Err(offline(host.to_string()))
        }
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        if url == ssdp::description_url(self.s1) {
            Ok(DESC_S1.replace("192.0.2.11", &self.s1.to_string()))
        } else if url == ssdp::description_url(ip("192.0.2.21")) {
            Ok(DESC_S2.into())
        } else {
            Err(offline(url.into()))
        }
    }

    fn ssdp_search(&self, _mx: u8, wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        self.waits.borrow_mut().push(wait);
        if wait.is_zero() {
            return Ok(Vec::new());
        }
        if !self.ssdp {
            return Err(offline("SSDP".into()));
        }
        Ok(vec![ssdp::Advert {
            location: ssdp::description_url(self.s1),
            st: ssdp::SONOS_ST.into(),
            usn: None,
            household: Some("Sonos_S1Household".into()),
            boot_seq: None,
        }])
    }
}

fn both_households(found: &Survey) {
    assert_eq!(found.households.len(), 2);
    assert_eq!(found.households[0].generation(), Some(Generation::S1));
    assert_eq!(found.households[1].generation(), Some(Generation::S2));
    assert!(
        found.households[1]
            .players
            .iter()
            .any(|player| player.ip == ip("192.0.2.21"))
    );
}

#[test]
fn reachable_known_households_still_skip_the_discovery_window() {
    let lan = StartupLan::new();
    let found = survey_known_first(
        &lan,
        &[ip("192.0.2.11")],
        || vec![ip("192.0.2.21")],
        Duration::from_secs(2),
    )
    .unwrap();

    both_households(&found);
    assert!(found.unreachable.is_empty());
    assert_eq!(*lan.waits.borrow(), [Duration::ZERO]);
}

#[test]
fn partial_multicast_keeps_reachable_remembered_households() {
    for explicit_seed in [false, true] {
        let lan = StartupLan::new();
        let wait = Duration::from_secs(2);
        let mut remembered = vec![ip("192.0.2.21"), ip("192.0.2.99")];
        let seeds = if explicit_seed {
            vec![ip("192.0.2.11")]
        } else {
            remembered.push(ip("192.0.2.11"));
            Vec::new()
        };
        let found = survey_known_first(&lan, &seeds, || remembered, wait).unwrap();

        both_households(&found);
        assert_eq!(*lan.waits.borrow(), [Duration::ZERO, wait]);
        assert_eq!(found.unreachable.len(), 1);
        assert_eq!(found.unreachable[0].0, "192.0.2.99");
        assert!(
            found
                .households
                .iter()
                .flat_map(|house| &house.players)
                .all(|player| player.ip != ip("192.0.2.99"))
        );
    }
}

#[test]
fn failed_multicast_keeps_directly_reachable_households_and_reports_failures() {
    let lan = StartupLan {
        ssdp: false,
        ..StartupLan::new()
    };
    let wait = Duration::from_secs(2);
    let found = survey_known_first(
        &lan,
        &[ip("192.0.2.11")],
        || vec![ip("192.0.2.21"), ip("192.0.2.99")],
        wait,
    )
    .unwrap();

    both_households(&found);
    assert!(
        found
            .ssdp_error
            .as_deref()
            .is_some_and(|error| error.contains("SSDP"))
    );
    assert_eq!(found.unreachable.len(), 1);
    assert_eq!(found.unreachable[0].0, "192.0.2.99");
    assert_eq!(*lan.waits.borrow(), [Duration::ZERO, wait]);
}

#[test]
fn a_newly_discovered_address_replaces_a_stale_hint_without_losing_the_other_household() {
    let lan = StartupLan {
        s1: ip("192.0.2.111"),
        ..StartupLan::new()
    };
    let wait = Duration::from_secs(2);
    let found = survey_known_first(
        &lan,
        &[],
        || vec![ip("192.0.2.11"), ip("192.0.2.21")],
        wait,
    )
    .unwrap();

    both_households(&found);
    let player = found.households[0]
        .player(&PlayerId("RINCON_000E58A0000401400".into()))
        .unwrap();
    assert_eq!(player.ip, ip("192.0.2.111"));
    assert!(
        found.households[0]
            .players
            .iter()
            .all(|player| player.ip != ip("192.0.2.11"))
    );
    assert_eq!(found.unreachable.len(), 1);
    assert_eq!(found.unreachable[0].0, "192.0.2.11");
    assert_eq!(*lan.waits.borrow(), [Duration::ZERO, wait]);
}
