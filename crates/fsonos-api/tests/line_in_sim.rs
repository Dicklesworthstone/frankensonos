//! Native input discovery and playback cross the same coordinator, policy and
//! undo boundaries as other sources, over real simulator HTTP/SOAP sockets.

use fsonos_api::{ErrorCode, PlayLineInRequest, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Lead", SimModel::Play5Gen1),
            SimPlayerSpec::new("Member", SimModel::Play5Gen1),
            SimPlayerSpec::pair("Turntable", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Other", SimModel::One)])
        .spawn()
        .unwrap()
}

fn houses(sim: &SimHandle) -> Vec<HouseholdState> {
    inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn surface(
    transport: Box<dyn Transport + Send + Sync>,
    houses: &[HouseholdState],
    policy: Policy,
) -> Surface {
    let known = houses.to_vec();
    Surface::new(
        transport,
        Box::new(move |_| Ok(known.clone())),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "http")
}

fn request(zone: &str, source: &str) -> PlayLineInRequest {
    PlayLineInRequest {
        zone: zone.into(),
        source: source.into(),
    }
}

fn queue(sim: &SimHandle, host: IpAddr) -> Vec<String> {
    browse_all(&sim.lan(), host, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn writes(sim: &SimHandle, from: usize) -> Vec<String> {
    sim.soap_log()
        .into_iter()
        .skip(from)
        .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
        .map(|call| call.action)
        .collect()
}

#[test]
fn discovery_reads_native_capability_per_player_and_stays_inside_the_destination_household() {
    let sim = sim();
    let houses = houses(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    let from = sim.soap_log().len();
    let inputs = s.line_in_sources(&Client::McpStdio, "Member").unwrap();
    assert_eq!(
        inputs.sources.len(),
        4,
        "both bonded input jacks must retain their identity"
    );
    assert!(inputs.unavailable.is_empty());
    let ids: std::collections::HashSet<_> =
        inputs.sources.iter().map(|source| &source.player).collect();
    assert_eq!(ids.len(), 4);
    assert!(
        s.line_in_sources(&Client::McpStdio, "Other")
            .unwrap()
            .sources
            .is_empty()
    );
    assert!(writes(&sim, from).is_empty());
    assert!(inputs.sources.iter().all(|source| source.room != "Other"));
    assert!(
        serde_json::to_value(&inputs).unwrap()["sources"][0]
            .get("connected")
            .is_none()
    );
}

#[test]
fn a_members_group_plays_the_exact_input_with_caps_a_retained_queue_and_undo() {
    let sim = sim();
    let initial = houses(&sim);
    let lead = resolve_room(&initial, "Lead").unwrap();
    let member = resolve_room(&initial, "Member").unwrap();
    let lead_id = lead.coordinator.id.clone();
    let lead_ip = lead.coordinator.ip;
    let member_ip = member.player.ip;
    soap::join_group(&sim.lan(), member_ip, &lead_id).unwrap();
    let houses = houses(&sim);
    control::queue_spotify_tracks(
        &sim.lan(),
        &houses,
        &lead_id,
        &[
            ("spotify:track:0000000000000000000001", "First"),
            ("spotify:track:0000000000000000000002", "Second"),
        ],
    )
    .unwrap();
    control::play_queue_from(&sim.lan(), &houses, &lead_id, 2).unwrap();
    let original_queue = queue(&sim, lead_ip);
    let original_media = soap::get_media_info(&sim.lan(), lead_ip).unwrap();
    let source_room = resolve_room(&houses, "Turntable").unwrap();
    let secondary = source_room
        .room
        .players
        .iter()
        .find(|id| **id != source_room.player.id)
        .unwrap()
        .clone();
    let source_ip = source_room.player.ip;
    for (host, volume) in [(lead_ip, 80), (member_ip, 70), (source_ip, 60)] {
        soap::set_volume(&sim.lan(), host, volume).unwrap();
    }
    let mut policy = Policy::default();
    policy.defaults.max_volume = 25;
    let s = surface(Box::new(sim.lan()), &houses, policy);
    let start = sim.soap_log().len();
    let outcome = s
        .play_line_in(
            &Client::McpStdio,
            &request("Member", &secondary.0.to_ascii_lowercase()),
        )
        .unwrap();
    assert!(outcome.changed && !outcome.notes.is_empty());
    assert_eq!(
        soap::get_media_info(&sim.lan(), lead_ip).unwrap().uri,
        format!("x-rincon-stream:{}", secondary.0)
    );
    assert_eq!(
        soap::get_transport_info(&sim.lan(), lead_ip).unwrap().state,
        TransportState::Playing
    );
    assert_eq!(queue(&sim, lead_ip), original_queue);
    assert_eq!(soap::get_volume(&sim.lan(), lead_ip).unwrap(), 25);
    assert_eq!(soap::get_volume(&sim.lan(), member_ip).unwrap(), 25);
    assert_eq!(soap::get_volume(&sim.lan(), source_ip).unwrap(), 60);
    let sent = writes(&sim, start);
    assert_eq!(
        sent.iter()
            .filter(|action| action.as_str() == "SetAVTransportURI")
            .count(),
        1
    );
    assert_eq!(
        sent.iter()
            .filter(|action| action.as_str() == "Play")
            .count(),
        1
    );
    assert!(!sent.iter().any(|action| action.contains("Queue")));
    let actions = s
        .recent_actions(&Client::McpStdio, &ActionFilter::default())
        .unwrap();
    assert!(actions[0].action.intent.contains("Turntable's line-in"));
    let restored = s.undo(&Client::McpStdio, true).unwrap().unwrap();
    assert!(restored.failures.is_empty(), "{restored:?}");
    assert_eq!(
        soap::get_media_info(&sim.lan(), lead_ip).unwrap().uri,
        original_media.uri
    );
    assert_eq!(
        soap::get_position_info(&sim.lan(), lead_ip).unwrap().track,
        2
    );
    assert_eq!(queue(&sim, lead_ip), original_queue);
}

#[test]
fn foreign_unsupported_and_denied_sources_cannot_change_playback_or_volumes() {
    let sim = sim();
    let houses = houses(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    let start = sim.soap_log().len();
    for req in [
        request("Lead", "Other"),
        request("Other", "Other"),
        request("Other", "Turntable"),
    ] {
        assert!(s.play_line_in(&Client::McpStdio, &req).is_err());
    }
    assert!(writes(&sim, start).is_empty());
    let start = sim.soap_log().len();
    let error = s
        .play_line_in(&Client::Unknown, &request("Lead", "Turntable"))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::PolicyDenied);
    assert_eq!(
        sim.soap_log().len(),
        start,
        "authorization precedes input discovery"
    );
}

/// Inject a different owner's source while the action snapshot reads volume.
/// Every request and the injected change still travel through simulator sockets.
struct DuringSnapshot {
    lan: SimLan,
    changed: Arc<AtomicBool>,
    target: IpAddr,
}

impl Transport for DuringSnapshot {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, action, body)?;
        if action.contains("#GetVolume") && !self.changed.swap(true, Ordering::SeqCst) {
            soap::set_av_transport_uri(
                &self.lan,
                self.target,
                "https://example.invalid/new-owner.mp3",
                "",
            )?;
        }
        Ok(reply)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }
}

#[test]
fn a_newer_source_during_snapshot_prevents_input_play_and_cap_changes() {
    let sim = sim();
    let houses = houses(&sim);
    let host = resolve_room(&houses, "Lead").unwrap().coordinator.ip;
    soap::set_volume(&sim.lan(), host, 80).unwrap();
    let changed = Arc::new(AtomicBool::new(false));
    let t = DuringSnapshot {
        lan: sim.lan(),
        changed: Arc::clone(&changed),
        target: host,
    };
    let s = surface(Box::new(t), &houses, Policy::default());
    let start = sim.soap_log().len();
    assert!(
        s.play_line_in(&Client::McpStdio, &request("Lead", "Turntable"))
            .is_err()
    );
    assert!(changed.load(Ordering::SeqCst));
    assert_eq!(
        writes(&sim, start),
        ["SetAVTransportURI"],
        "only the injected owner command may write"
    );
    assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 80);
    assert_eq!(
        soap::get_media_info(&sim.lan(), host).unwrap().uri,
        "https://example.invalid/new-owner.mp3"
    );
}
