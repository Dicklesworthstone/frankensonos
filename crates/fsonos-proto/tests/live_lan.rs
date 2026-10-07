//! Live check against the owner's own players. Opt-in and never in CI: it
//! needs a host on the speaker LAN, so it is `#[ignore]`d and runs with
//!
//! ```text
//! cargo test -p fsonos-proto --test live_lan -- --ignored --nocapture
//! ```
//!
//! `FSONOS_SEEDS` (player addresses separated by commas or spaces) adds direct
//! seeds to SSDP. Use it on networks where multicast does not reach every
//! player, e.g. speakers on several routed subnets.
//!
//! Read-only by default: discovery, descriptions, topology, volume/mute,
//! transport state, and a GENA round-trip (subscribe to one player's
//! RenderingControl, receive its initial NOTIFY, unsubscribe).
//! `FSONOS_LIVE_WRITE=1` adds one write that is inaudible by construction
//! (SetVolume to the volume the player already has). Output names real rooms
//! and addresses: it stays on the terminal, never in git.

use fsonos_proto::content::browse_all;
use fsonos_proto::control;
use fsonos_proto::description::parse_device_description;
use fsonos_proto::didl::{learn_spotify_params, spotify_track_uri, spotify_uri_from_renderer_uri};
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::soap::RENDERING_CONTROL;
use fsonos_proto::topology::{get_zone_group_state, host_of_location};
use fsonos_proto::{Transport, ssdp};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

fn seeds() -> Vec<IpAddr> {
    std::env::var("FSONOS_SEEDS")
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse()
                .unwrap_or_else(|e| panic!("FSONOS_SEEDS entry {s:?}: {e}"))
        })
        .collect()
}

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
    println!("SSDP: {} ZonePlayer replies", hosts.len());
    let seeds = seeds();
    println!("seeds: {}", seeds.len());
    hosts.extend(seeds);
    hosts.sort();
    hosts.dedup();
    assert!(
        !hosts.is_empty(),
        "no players: SSDP got no replies and FSONOS_SEEDS is empty \
         (check Local Network permission, multicast, and whether the speakers share this host's subnet)"
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
    let mut household_hosts: Vec<IpAddr> = Vec::new();
    for &host in &hosts {
        if covered.contains(&host) {
            continue;
        }
        let zgs = get_zone_group_state(&lan, host)
            .unwrap_or_else(|e| panic!("ZoneGroupTopology from {host}: {e}"));
        println!("household via {host}: {} groups", zgs.groups.len());
        // Favorites are browsed on a renderer: a Bridge has no
        // ContentDirectory (it answers Browse with HTTP 405).
        let mut renderer_host = None;
        for group in &zgs.groups {
            for m in &group.members {
                if let Some(ip) = m.ip() {
                    covered.push(ip);
                    if m.uuid == group.coordinator && !m.is_zone_bridge {
                        coordinators.push(ip);
                        renderer_host.get_or_insert(ip);
                    }
                }
            }
        }
        household_hosts.extend(renderer_host);
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

    gena_round_trip(&lan, renderers[0]);
    for &host in &household_hosts {
        spotify_params_round_trip(&lan, host);
    }

    if std::env::var("FSONOS_LIVE_WRITE").as_deref() == Ok("1") {
        write_checks(&lan, &renderers, &coordinators);
    }
}

/// The opt-in writes, both inaudible by construction: SetVolume to the
/// current level, and Pause on a group that is already paused or stopped.
fn write_checks(lan: &Lan, renderers: &[IpAddr], coordinators: &[IpAddr]) {
    let host = renderers[0];
    let before = control::get_volume(lan, host).expect("GetVolume");
    control::set_volume(lan, host, before).expect("SetVolume to the same level");
    let after = control::get_volume(lan, host).expect("GetVolume");
    println!("write check on {host}: SetVolume({before}) -> {after}");
    assert_eq!(before, after);

    // Pause on a coordinator that is already paused or stopped changes
    // nothing audible; the player either accepts it or refuses the
    // transition (UPnP 701). Either way the control path is exercised.
    let idle = coordinators.iter().copied().find(|&c| {
        matches!(
            control::get_transport_info(lan, c).map(|i| i.state),
            Ok(fsonos_types::TransportState::Paused | fsonos_types::TransportState::Stopped)
        )
    });
    if let Some(c) = idle {
        let before = control::get_transport_info(lan, c)
            .expect("GetTransportInfo")
            .state;
        let outcome = control::pause(lan, c);
        println!("pause check on idle coordinator {c} ({before:?}): {outcome:?}");
        assert!(
            matches!(
                outcome,
                Ok(()) | Err(fsonos_proto::ProtoError::SoapFault { code: 701, .. })
            ),
            "{outcome:?}"
        );
        let after = control::get_transport_info(lan, c)
            .expect("GetTransportInfo")
            .state;
        assert_eq!(before, after, "an idle group stays idle");
    }
}

/// Subscribe to `host`'s RenderingControl with a sink on the address this
/// host uses to reach it, expect the initial full-state NOTIFY, unsubscribe.
fn gena_round_trip(lan: &Lan, host: IpAddr) {
    let local = lan.local_address_toward(host).expect("route to the player");
    let sink = EventSink::start(SocketAddr::new(local, 0)).expect("GENA sink");
    let sub = lan
        .subscribe(
            host,
            RENDERING_CONTROL.event_path,
            &sink.callback_url("RenderingControl"),
            60,
        )
        .expect("SUBSCRIBE");
    println!(
        "GENA: subscribed to {host} RenderingControl for {}s",
        sub.timeout_secs
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let initial = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "no initial NOTIFY from {host} within 10 s: the player could not reach \
             {} (firewall, or the wrong interface)",
            sink.local_addr()
        );
        if let Some(n) = sink.recv_timeout(left)
            && n.sid == sub.sid
        {
            break n;
        }
    };
    let volume = initial
        .last_change()
        .expect("LastChange parses")
        .and_then(|lc| lc.volume());
    println!(
        "GENA: initial NOTIFY seq {} on {}, Master volume {volume:?}",
        initial.seq, initial.path
    );
    assert_eq!(initial.seq, 0, "the first event is the full state");
    assert_eq!(
        volume,
        Some(control::get_volume(lan, host).expect("GetVolume")),
        "the event agrees with GetVolume"
    );
    lan.unsubscribe(host, RENDERING_CONTROL.event_path, &sub.sid)
        .expect("UNSUBSCRIBE");
    println!("GENA: unsubscribed");
}

/// Learn the household's Spotify render parameters from its own favorites
/// (read-only Browse of FV:2) and check they rebuild every Spotify track
/// favorite that uses them. Prints no parameter values: they are site data.
fn spotify_params_round_trip(lan: &Lan, host: IpAddr) {
    let favorites = browse_all(lan, host, "FV:2").expect("Browse FV:2");
    let tracks: Vec<&str> = favorites
        .iter()
        .filter_map(|f| f.res.as_ref().map(|r| r.uri.as_str()))
        .filter(|u| u.starts_with("x-sonos-spotify:"))
        .collect();
    let Some(params) = learn_spotify_params(&favorites) else {
        println!(
            "Spotify params via {host}: none learned ({} favorites, {} Spotify tracks)",
            favorites.len(),
            tracks.len()
        );
        return;
    };
    let flags = format!("flags={}", params.flags);
    let matching: Vec<&&str> = tracks.iter().filter(|u| u.contains(&flags)).collect();
    for uri in &matching {
        let spotify = spotify_uri_from_renderer_uri(uri).expect("a Spotify URI");
        assert!(
            spotify_track_uri(&spotify, &params).eq_ignore_ascii_case(uri),
            "learned params do not rebuild a favorite of the household via {host}"
        );
    }
    assert!(params.cdudn.starts_with("SA_RINCON"), "descriptor shape");
    println!(
        "Spotify params via {host}: learned from {} favorites; rebuilt {}/{} track favorites with the dominant flags exactly",
        favorites.len(),
        matching.len(),
        tracks.len()
    );
}
