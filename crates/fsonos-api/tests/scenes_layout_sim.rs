//! Exact scene grouping through the shared surface, over real simulator
//! sockets. Failed or late group changes must not start the wrong audience.

use fsonos_api::surface::scenes::{SceneApplyRequest, SceneDefinition};
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use serde_json::json;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const OLD: &str = "x-rincon-mp3radio://scene.example.invalid/old";
const NEW: &str = "x-rincon-mp3radio://scene.example.invalid/new";

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("A", SimModel::Play5Gen1),
            SimPlayerSpec::pair("B", SimModel::Play5Gen1),
            SimPlayerSpec::new("C", SimModel::Play5Gen1),
            SimPlayerSpec::new("D", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("A", SimModel::One)])
        .spawn()
        .unwrap()
}

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn survey() -> Survey {
    Box::new(|t| Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households))
}

fn surface(sim: &SimHandle) -> Surface {
    with_transport(Box::new(sim.lan()))
}

fn with_transport(transport: Box<dyn Transport + Send + Sync>) -> Surface {
    Surface::new(
        transport,
        survey(),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test")
}

fn id(h: &[HouseholdState], name: &str) -> PlayerId {
    resolve_room(h, name).unwrap().player.id.clone()
}

fn join(sim: &SimHandle, from: &str, to: &str) {
    let h = house(sim);
    control::join(&sim.lan(), &h, &id(&h, from), &id(&h, to)).unwrap();
}

fn play(sim: &SimHandle, name: &str, uri: &str) {
    let h = house(sim);
    control::play_uri(&sim.lan(), &h, &id(&h, name), uri, "").unwrap();
}

fn playing(sim: &SimHandle, name: &str) -> (TransportState, String) {
    let h = house(sim);
    let now = control::playback(&sim.lan(), &h, &id(&h, name)).unwrap();
    (now.transport.state, now.position.uri)
}

fn members(sim: &SimHandle, name: &str) -> Vec<String> {
    let h = house(sim);
    let lead = resolve_room(&h, name).unwrap();
    let mut names: Vec<_> = lead
        .household
        .rooms
        .iter()
        .filter(|r| r.coordinator == lead.coordinator.id)
        .map(|r| r.name.clone())
        .collect();
    names.sort();
    names
}

fn save(s: &Surface, value: serde_json::Value) {
    let def: SceneDefinition = serde_json::from_value(value).unwrap();
    s.save_scene_from(&Client::Cli, "Layout", &def).unwrap();
}

#[test]
fn solo_scene_removes_unlisted_pair_preserves_music_and_undo_restores_the_group() {
    let sim = sim();
    play(&sim, "A@S1", OLD);
    play(&sim, "A@S2", OLD);
    join(&sim, "B", "A@S1");
    join(&sim, "C", "A@S1");
    let s = surface(&sim);
    save(&s, json!({"groups": [{"coordinator": "A@S1"}]}));
    let mark = sim.soap_log().len();
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(result.complete, "{result:?}");
    assert_eq!(members(&sim, "A@S1"), ["A"]);
    assert_eq!(members(&sim, "B"), ["B"]);
    let h = house(&sim);
    assert_eq!(resolve_room(&h, "B").unwrap().room.players.len(), 2);
    assert_eq!(playing(&sim, "A@S1"), (TransportState::Playing, OLD.into()));
    assert_eq!(playing(&sim, "A@S2"), (TransportState::Playing, OLD.into()));
    assert!(sim.soap_log()[mark..].iter().all(|entry| {
        !matches!(
            entry.action.as_str(),
            "Play" | "Pause" | "Stop" | "RemoveAllTracksFromQueue"
        )
    }));
    let again = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(!again.changed && again.steps.is_empty(), "{again:?}");
    let undo = s.undo(&Client::Cli, false).unwrap().unwrap();
    assert!(undo.failures.is_empty(), "{undo:?}");
    assert_eq!(members(&sim, "A@S1"), ["A", "B", "C"]);
}

#[test]
fn subset_scene_keeps_requested_rooms_and_an_unrelated_group() {
    let sim = sim();
    join(&sim, "B", "A@S1");
    join(&sim, "C", "A@S1");
    play(&sim, "D", OLD);
    let s = surface(&sim);
    save(
        &s,
        json!({"groups": [{"coordinator": "A@S1", "members": ["B"]}]}),
    );
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(result.complete, "{result:?}");
    assert_eq!(members(&sim, "A@S1"), ["A", "B"]);
    assert_eq!(members(&sim, "C"), ["C"]);
    assert_eq!(playing(&sim, "D"), (TransportState::Playing, OLD.into()));
}

#[test]
fn a_new_coordinator_and_delayed_member_are_ready_before_music_starts() {
    let sim = sim();
    play(&sim, "C", OLD);
    join(&sim, "A@S1", "C");
    sim.join_lag("B", Duration::from_millis(300)).unwrap();
    let s = surface(&sim);
    save(
        &s,
        json!({"groups": [{"coordinator": "A@S1", "members": ["B"],
            "source": {"kind": "uri", "uri": NEW}}]}),
    );
    let mark = sim.soap_log().len();
    let started = Instant::now();
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(result.complete, "{result:?}");
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(members(&sim, "A@S1"), ["A", "B"]);
    assert_eq!(playing(&sim, "B"), (TransportState::Playing, NEW.into()));
    assert_eq!(playing(&sim, "C"), (TransportState::Playing, OLD.into()));
    let calls = sim.soap_log();
    let calls = &calls[mark..];
    let joined = calls
        .iter()
        .position(|e| {
            e.room == "B"
                && e.args
                    .iter()
                    .any(|(k, v)| k == "CurrentURI" && v.starts_with("x-rincon:"))
        })
        .unwrap();
    let source = calls
        .iter()
        .position(|e| e.args.iter().any(|(k, v)| k == "CurrentURI" && v == NEW))
        .unwrap();
    assert!(
        calls[joined + 1..source]
            .iter()
            .any(|e| e.action == "GetZoneGroupState")
    );
}

#[test]
fn a_successful_but_ignored_join_blocks_its_component_not_an_independent_group() {
    let sim = sim();
    play(&sim, "A@S1", OLD);
    sim.ignore_action("B", "SetAVTransportURI").unwrap();
    let s = surface(&sim);
    save(
        &s,
        json!({"groups": [
            {"coordinator": "A@S1", "members": ["B"], "source": {"kind": "uri", "uri": NEW}},
            {"coordinator": "D", "source": {"kind": "uri", "uri": NEW}}
        ]}),
    );
    let mark = sim.soap_log().len();
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(!result.complete && result.failed.len() >= 2, "{result:?}");
    assert_eq!(playing(&sim, "A@S1"), (TransportState::Playing, OLD.into()));
    assert_eq!(playing(&sim, "D"), (TransportState::Playing, NEW.into()));
    let calls = sim.soap_log();
    assert_eq!(
        calls[mark..]
            .iter()
            .filter(|e| e.room == "B" && e.action == "SetAVTransportURI")
            .count(),
        1,
        "the acknowledged write is never replayed"
    );
}

#[test]
fn failed_detach_prevents_fade_and_new_source_in_the_old_audience() {
    let sim = sim();
    play(&sim, "A@S1", OLD);
    join(&sim, "B", "A@S1");
    sim.upnp_fault("B", "SetAVTransportURI", 500).unwrap();
    let s = surface(&sim);
    save(
        &s,
        json!({"groups": [{"coordinator": "A@S1", "source": {"kind": "uri", "uri": NEW}}],
            "volumes": {"A@S1": 12}, "mutes": {"B": false}}),
    );
    let mark = sim.soap_log().len();
    let result = s
        .apply_scene_with(
            &Client::Cli,
            "Layout",
            &SceneApplyRequest { fade_secs: Some(1) },
        )
        .unwrap();
    assert!(!result.complete, "{result:?}");
    assert_eq!(members(&sim, "A@S1"), ["A", "B"]);
    assert_eq!(playing(&sim, "A@S1"), (TransportState::Playing, OLD.into()));
    assert!(
        sim.soap_log()[mark..]
            .iter()
            .all(|e| e.action != "SetVolume"),
        "an unconfirmed group is not silenced or faded"
    );
}

struct LostJoin {
    lan: SimLan,
    lose: AtomicBool,
}

impl Transport for LostJoin {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.lan.soap_post(host, path, action, body)?;
        if action.contains("SetAVTransportURI")
            && body.contains("x-rincon:")
            && self.lose.swap(false, Ordering::SeqCst)
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "synthetic lost join reply".into(),
            });
        }
        Ok(answer)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }

    fn ssdp_search(
        &self,
        mx: u8,
        wait: Duration,
    ) -> Result<Vec<fsonos_proto::ssdp::Advert>, ProtoError> {
        self.lan.ssdp_search(mx, wait)
    }
}

#[test]
fn an_accepted_join_with_a_lost_reply_is_confirmed_and_never_repeated() {
    let sim = sim();
    let s = with_transport(Box::new(LostJoin {
        lan: sim.lan(),
        lose: AtomicBool::new(true),
    }));
    save(
        &s,
        json!({"groups": [{"coordinator": "A@S1", "members": ["B"],
            "source": {"kind": "uri", "uri": NEW}}]}),
    );
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(result.complete, "{result:?}");
    assert_eq!(playing(&sim, "B"), (TransportState::Playing, NEW.into()));
    assert_eq!(
        sim.soap_log()
            .iter()
            .filter(|e| e.room == "B" && e.action == "SetAVTransportURI")
            .count(),
        1
    );
}

#[test]
fn a_failed_topology_witness_is_not_retried_when_the_member_confirms_the_join() {
    let sim = sim();
    let s = surface(&sim);
    save(
        &s,
        json!({"groups": [{"coordinator": "A@S1", "members": ["B"],
            "source": {"kind": "uri", "uri": NEW}}]}),
    );
    let h = house(&sim);
    let a = id(&h, "A@S1");
    sim.upnp_fault(&a.0, "GetZoneGroupState", 701).unwrap();
    let mark = sim.soap_log().len();
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(result.complete, "{result:?}");
    let calls = sim.soap_log();
    let calls = &calls[mark..];
    let join = calls
        .iter()
        .position(|e| {
            e.room == "B"
                && e.action == "SetAVTransportURI"
                && e.args
                    .iter()
                    .any(|(k, v)| k == "CurrentURI" && v.starts_with("x-rincon:"))
        })
        .expect("the member joins the coordinator");
    let source = calls
        .iter()
        .position(|e| {
            e.player == a.0
                && e.action == "SetAVTransportURI"
                && e.args.iter().any(|(k, v)| k == "CurrentURI" && v == NEW)
        })
        .expect("the confirmed group starts the requested source");
    // The action's before-snapshot reads the current groups too (b3432d67),
    // before anything changes, and undo's after-state surveys again once the
    // scene has finished. Only the reads between the join and the source
    // start are the join's witnesses: the failed one is not retried.
    assert_eq!(
        calls[join..source]
            .iter()
            .filter(|e| e.player == a.0 && e.action == "GetZoneGroupState")
            .count(),
        1
    );
    sim.clear_faults(&a.0).unwrap();
    assert_eq!(playing(&sim, "B"), (TransportState::Playing, NEW.into()));
}

#[test]
fn a_failed_source_does_not_pause_the_old_music_in_a_following_step() {
    let sim = sim();
    play(&sim, "A@S1", OLD);
    let s = surface(&sim);
    save(
        &s,
        json!({"groups": [{"coordinator": "A@S1", "playing": false,
            "source": {"kind": "uri", "uri": NEW}}]}),
    );
    let h = house(&sim);
    let a = id(&h, "A@S1");
    sim.upnp_fault(&a.0, "SetAVTransportURI", 714).unwrap();
    let mark = sim.soap_log().len();
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(!result.complete && result.failed.len() == 2, "{result:?}");
    assert_eq!(playing(&sim, "A@S1"), (TransportState::Playing, OLD.into()));
    assert!(
        sim.soap_log()[mark..]
            .iter()
            .all(|e| !matches!(e.action.as_str(), "Pause" | "Stop"))
    );
}

struct SurpriseMember {
    lan: SimLan,
    once: AtomicBool,
    extra: IpAddr,
    coordinator: PlayerId,
}

impl Transport for SurpriseMember {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.lan.soap_post(host, path, action, body)?;
        if action.contains("SetAVTransportURI")
            && body.contains("x-rincon:")
            && self.once.swap(false, Ordering::SeqCst)
        {
            // Another controller groups an unrequested room before read-back.
            fsonos_proto::control::join_group(&self.lan, self.extra, &self.coordinator)?;
        }
        Ok(answer)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }

    fn ssdp_search(
        &self,
        mx: u8,
        wait: Duration,
    ) -> Result<Vec<fsonos_proto::ssdp::Advert>, ProtoError> {
        self.lan.ssdp_search(mx, wait)
    }
}

#[test]
fn an_unexpected_extra_room_blocks_playback_even_when_the_requested_join_succeeded() {
    let sim = sim();
    play(&sim, "A@S1", OLD);
    let h = house(&sim);
    let s = with_transport(Box::new(SurpriseMember {
        lan: sim.lan(),
        once: AtomicBool::new(true),
        extra: resolve_room(&h, "C").unwrap().player.ip,
        coordinator: id(&h, "A@S1"),
    }));
    save(
        &s,
        json!({"groups": [{"coordinator": "A@S1", "members": ["B"],
            "source": {"kind": "uri", "uri": NEW}}]}),
    );
    let result = s.apply_scene(&Client::Cli, "Layout").unwrap();
    assert!(!result.complete, "{result:?}");
    assert_eq!(members(&sim, "A@S1"), ["A", "B", "C"]);
    assert_eq!(playing(&sim, "A@S1"), (TransportState::Playing, OLD.into()));
    assert_eq!(
        result.failed.len(),
        1,
        "only the unsafe source is withheld: {result:?}"
    );
}
