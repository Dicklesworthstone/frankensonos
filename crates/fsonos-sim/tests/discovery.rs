//! Deterministic discovery of the virtual players, on macOS and Linux alike:
//! the unicast SSDP responder answers FrankenSonos's own M-SEARCH and parser,
//! the seeds file feeds the direct-seed path, and `fsonos-core`'s survey
//! finds both households either way.

use fsonos_core::inventory::survey;
use fsonos_proto::ssdp::{SONOS_ST, m_search, parse_response};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimLan};
use std::net::{IpAddr, UdpSocket};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_millis(300);

/// Every IP address in a seeds file, the way `fsonos --seeds` reads it.
fn seeds(text: &str) -> Vec<IpAddr> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .flat_map(|line| line.split(|c: char| !(c.is_ascii_hexdigit() || c == '.' || c == ':')))
        .filter_map(|token| token.trim_matches(':').parse().ok())
        .collect()
}

/// `SimLan` without SSDP: only seeds can find players through it.
struct SeedsOnly(SimLan);

impl Transport for SeedsOnly {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.0.soap_post(host, path, action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.0.http_get(url)
    }
}

fn rooms(t: &impl Transport, seeds: &[IpAddr]) -> Vec<Vec<String>> {
    let found = survey(t, seeds, WAIT).unwrap();
    found
        .households
        .iter()
        .map(|h| h.rooms.iter().map(|r| r.name.clone()).collect())
        .collect()
}

#[test]
fn ssdp_finds_every_player_with_the_real_parser() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let adverts = sim.lan().ssdp_search(1, WAIT).unwrap();
    assert_eq!(adverts.len(), 5);
    for p in sim.players() {
        let a = adverts
            .iter()
            .find(|a| a.location == format!("http://{}:1400/xml/device_description.xml", p.ip))
            .unwrap_or_else(|| panic!("no advert for {}", p.room));
        assert_eq!(a.st, SONOS_ST);
        assert_eq!(
            a.usn.as_deref(),
            Some(format!("uuid:{}::{SONOS_ST}", p.uuid).as_str())
        );
        assert_eq!(a.household.as_deref(), Some(p.household.as_str()));
        assert_eq!(a.boot_seq, Some(1));
    }

    // The responder answers a raw M-SEARCH datagram too, one reply per player.
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    socket
        .send_to(m_search(1).as_bytes(), sim.ssdp_addr())
        .unwrap();
    let mut replies = 0;
    let deadline = Instant::now() + WAIT;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        if let Ok((n, _)) = socket.recv_from(&mut buf) {
            assert!(parse_response(&buf[..n]).is_some());
            replies += 1;
        }
    }
    assert_eq!(replies, 5);
}

#[test]
fn survey_finds_both_households_by_ssdp_or_by_seeds() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let expected = [
        vec!["Kitchen".to_string(), "Office".to_string()],
        vec!["Living Room".to_string(), "Bedroom".to_string()],
    ];

    // SSDP alone.
    assert_eq!(rooms(&sim.lan(), &[]), expected);

    // Seeds alone: the file lists every player's advertised address.
    let file = sim.seeds_toml();
    assert!(file.contains("players = ["));
    let seed_addrs = seeds(&file);
    assert_eq!(seed_addrs.len(), 5);
    let seeds_only = SeedsOnly(sim.lan());
    let found = survey(&seeds_only, &seed_addrs, WAIT).unwrap();
    assert!(found.ssdp_error.is_some(), "SSDP was unavailable");
    assert_eq!(found.unreachable.len(), 0);
    let names: Vec<Vec<String>> = found
        .households
        .iter()
        .map(|h| h.rooms.iter().map(|r| r.name.clone()).collect())
        .collect();
    assert_eq!(names, expected);
}

#[test]
fn address_changes_and_power_off_show_in_discovery() {
    let mut sim = SimHousehold::standard().spawn().unwrap();
    let old = sim.player("Office").unwrap().ip;
    let new = sim.change_address("Office").unwrap();

    // The regenerated seeds and the SSDP replies carry the new address.
    let seed_addrs = seeds(&sim.seeds_toml());
    assert!(seed_addrs.contains(&new) && !seed_addrs.contains(&old));
    let adverts = sim.lan().ssdp_search(1, WAIT).unwrap();
    assert!(
        adverts
            .iter()
            .any(|a| a.location.contains(&format!("//{new}:1400/")))
    );
    assert!(
        !adverts
            .iter()
            .any(|a| a.location.contains(&format!("//{old}:1400/")))
    );

    // Survey (either path) finds the Office at its new address.
    for found in [
        survey(&sim.lan(), &[], WAIT).unwrap(),
        survey(&SeedsOnly(sim.lan()), &seed_addrs, WAIT).unwrap(),
    ] {
        let office = found
            .households
            .iter()
            .flat_map(|h| &h.players)
            .find(|p| p.room_name == "Office")
            .unwrap();
        assert_eq!(office.ip, new);
    }

    // A powered-off player answers neither SSDP nor its seed.
    sim.set_offline("Kitchen", true).unwrap();
    let adverts = sim.lan().ssdp_search(1, WAIT).unwrap();
    assert_eq!(adverts.len(), 4);
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let found = survey(&SeedsOnly(sim.lan()), &[kitchen], WAIT).unwrap();
    assert_eq!(found.unreachable.len(), 1);
    assert_eq!(found.unreachable[0].0, kitchen.to_string());
}
