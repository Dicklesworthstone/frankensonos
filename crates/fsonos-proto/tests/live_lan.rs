//! Live check against the owner's own players. Opt-in and never in CI: it
//! needs a host on the speaker LAN, so it is `#[ignore]`d and runs with
//!
//! ```text
//! cargo test -p fsonos-proto --test live_lan -- --ignored --nocapture
//! ```
//!
//! Read-only by default: discovery, descriptions, topology, volume/mute, and
//! transport state. `FSONOS_LIVE_WRITE=1` adds one write that is inaudible by
//! construction (SetVolume to the volume the player already has). Output
//! names real rooms and addresses: it stays on the terminal, never in git.

use fsonos_proto::control;
use fsonos_proto::description::parse_device_description;
use fsonos_proto::net::Lan;
use fsonos_proto::topology::{get_zone_group_state, host_of_location};
use fsonos_proto::{Transport, ssdp};
use std::net::IpAddr;
use std::time::Duration;

#[test]
#[ignore = "live LAN: needs the owner's speakers; run with -- --ignored"]
fn live_lan_read_path() {
    let lan = Lan::start().expect("start the LAN worker");
    let adverts = lan
        .ssdp_search(1, Duration::from_secs(3))
        .expect("SSDP search");
    let mut hosts: Vec<IpAddr> = adverts
        .iter()
        .filter_map(|a| host_of_location(&a.location))
        .collect();
    hosts.sort();
    hosts.dedup();
    println!("SSDP: {} ZonePlayer replies", hosts.len());
    assert!(
        !hosts.is_empty(),
        "no SSDP replies: check the Local Network permission and multicast on this host"
    );

    let mut renderers = Vec::new();
    for &host in &hosts {
        let desc = lan
            .http_get(&ssdp::description_url(host))
            .and_then(|body| parse_device_description(&body))
            .unwrap_or_else(|e| panic!("description from {host}: {e}"));
        println!(
            "  {host}: {} ({} {}), swGen {:?}, renderer {}",
            desc.room_name,
            desc.model_name,
            desc.model_number,
            desc.sw_gen,
            desc.is_renderer()
        );
        if desc.is_renderer() {
            renderers.push(host);
        }
    }

    // One topology read per household: keep asking players not yet covered.
    let mut covered: Vec<IpAddr> = Vec::new();
    let mut coordinators: Vec<IpAddr> = Vec::new();
    for &host in &hosts {
        if covered.contains(&host) {
            continue;
        }
        let zgs = get_zone_group_state(&lan, host)
            .unwrap_or_else(|e| panic!("ZoneGroupTopology from {host}: {e}"));
        println!("household via {host}: {} groups", zgs.groups.len());
        for group in &zgs.groups {
            for m in &group.members {
                if let Some(ip) = m.ip() {
                    covered.push(ip);
                    if m.uuid == group.coordinator && !m.is_zone_bridge {
                        coordinators.push(ip);
                    }
                }
            }
        }
    }

    for &host in &renderers {
        let volume = control::get_volume(&lan, host).expect("GetVolume");
        let mute = control::get_mute(&lan, host).expect("GetMute");
        println!("  {host}: volume {volume}, mute {mute}");
    }
    for &host in &coordinators {
        let info = control::get_transport_info(&lan, host).expect("GetTransportInfo");
        let pos = control::get_position_info(&lan, host).expect("GetPositionInfo");
        let title = pos.metadata.as_ref().map_or("-", |m| m.title.as_str());
        println!(
            "  coordinator {host}: {:?}, track {} at {:?}s: {title}",
            info.state, pos.track, pos.position_secs
        );
    }

    if std::env::var("FSONOS_LIVE_WRITE").as_deref() == Ok("1") {
        let host = renderers[0];
        let before = control::get_volume(&lan, host).expect("GetVolume");
        control::set_volume(&lan, host, before).expect("SetVolume to the same level");
        let after = control::get_volume(&lan, host).expect("GetVolume");
        println!("write check on {host}: SetVolume({before}) -> {after}");
        assert_eq!(before, after);
    }
}
