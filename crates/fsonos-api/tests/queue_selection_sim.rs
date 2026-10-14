//! Queue selection with a caller's revision over real localhost SOAP.
//! Check positive playback/caps/undo as well as rejecting stale selections.

use fsonos_api::ErrorCode;
use fsonos_api::surface::queue::{QueueDto, QueuePlayRequest};
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ROOM: &str = "Queue Room";
const OTHER: &str = "Other Room";
const RADIO: &str = "x-rincon-mp3radio://example.invalid/radio";
const A: &str = "spotify:track:SelectionA";

fn setup(generation: u8) -> (SimHandle, Vec<HouseholdState>) {
    let builder = SimHousehold::builder();
    let sim = if generation == 1 {
        builder.s1([
            SimPlayerSpec::new(ROOM, SimModel::Play5Gen1),
            SimPlayerSpec::new(OTHER, SimModel::Play5Gen1),
        ])
    } else {
        builder.s2([
            SimPlayerSpec::new(ROOM, SimModel::One),
            SimPlayerSpec::new(OTHER, SimModel::One),
        ])
    }
    .spawn()
    .unwrap();
    let seeds: Vec<_> = sim.players().iter().map(|p| p.ip).collect();
    let houses = fsonos_core::inventory::survey(&sim.lan(), &seeds, Duration::ZERO)
        .unwrap()
        .households;
    for room in [ROOM, OTHER] {
        let id = resolve_room(&houses, room).unwrap().coordinator.id.clone();
        control::queue_spotify_tracks(
            &sim.lan(),
            &houses,
            &id,
            &[
                (A, "First A"),
                ("spotify:track:SelectionB", "B"),
                (A, "Second A"),
            ],
        )
        .unwrap()
        .expect("synthetic render parameters");
    }
    (sim, houses)
}

fn surface(
    houses: Vec<HouseholdState>,
    transport: impl Transport + Send + Sync + 'static,
) -> Surface {
    let survey: Survey = Box::new(move |_| Ok(houses.clone()));
    Surface::new(
        Box::new(transport),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test")
}

fn request(view: &QueueDto, position: u32) -> QueuePlayRequest {
    QueuePlayRequest {
        zone: view.zone.clone(),
        position,
        expected_uri: view
            .items
            .iter()
            .find(|i| i.position == position)
            .unwrap()
            .uri
            .clone(),
        expected_revision: view.revision.clone(),
    }
}

fn reads_only(sim: &SimHandle, from: usize) {
    for entry in &sim.soap_log()[from..] {
        assert!(
            entry.action.starts_with("Get") || entry.action == "Browse",
            "{entry:?}"
        );
    }
}

fn agent() -> Client {
    Client::Tailnet("tag:queue-agent".into())
}

#[test]
fn fresh_revision_plays_with_caps_and_undo_without_replacing_the_queue() {
    for generation in [1, 2] {
        let (sim, houses) = setup(generation);
        let target = resolve_room(&houses, ROOM).unwrap();
        let id = target.coordinator.id.clone();
        let host = target.coordinator.ip;
        control::play_uri(&sim.lan(), &houses, &id, RADIO, "").unwrap();
        control::set_volume(&sim.lan(), &houses, &id, 90).unwrap();
        let s = surface(houses.clone(), sim.lan());
        let view = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
        let result = s.queue_play(&agent(), &request(&view, 2)).unwrap();
        assert!(result.changed);
        assert!(!result.notes.is_empty(), "volume cap should be reported");
        assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().track, 2);
        assert_eq!(control::volume(&sim.lan(), &houses, &id).unwrap(), 70);
        let after = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
        assert_eq!(after.revision, view.revision);
        assert_eq!(after.items, view.items);
        let undo = s
            .undo(&agent(), false)
            .unwrap()
            .expect("queue start is undoable");
        assert!(undo.failures.is_empty(), "{undo:?}");
        assert_eq!(soap::get_media_info(&sim.lan(), host).unwrap().uri, RADIO);
        assert_eq!(
            s.queue(&Client::Cli, ROOM, 0, 100).unwrap().revision,
            view.revision
        );
    }
}

#[test]
fn duplicate_uri_at_the_same_position_does_not_make_an_old_revision_valid() {
    let (sim, houses) = setup(2);
    let target = resolve_room(&houses, ROOM).unwrap();
    let id = target.coordinator.id.clone();
    let host = target.coordinator.ip;
    let s = surface(houses.clone(), sim.lan());
    let before = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
    let req = request(&before, 3);
    let page = content::browse(&sim.lan(), host, QUEUE, BrowseFlag::DirectChildren, 0, 1).unwrap();
    soap::remove_track_range_from_queue(&sim.lan(), host, page.update_id, 1, 1).unwrap();
    control::queue_spotify_tracks(&sim.lan(), &houses, &id, &[(A, "Third A")])
        .unwrap()
        .expect("synthetic render parameters");
    let now = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
    assert_eq!(now.total, before.total);
    assert_eq!(
        now.items[2].uri, req.expected_uri,
        "URI alone still matches"
    );
    assert_ne!(now.revision, before.revision);
    control::set_volume(&sim.lan(), &houses, &id, 90).unwrap();
    let calls = sim.soap_log().len();
    let failure = s.queue_play(&agent(), &req).unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotReady);
    reads_only(&sim, calls);
    assert_eq!(control::volume(&sim.lan(), &houses, &id).unwrap(), 90);
    let log = s
        .recent_actions(&agent(), &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].action.decision.starts_with("deny:"));
    assert!(log[0].action.before_state.is_none());
}

#[test]
fn another_coordinators_token_is_rejected_even_with_identical_tracks() {
    let (sim, houses) = setup(2);
    let s = surface(houses, sim.lan());
    let view = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
    let mut req = request(&view, 1);
    req.zone = OTHER.into();
    let other = s.queue(&Client::Cli, OTHER, 0, 100).unwrap();
    assert_eq!(view.items[0].uri, other.items[0].uri);
    let calls = sim.soap_log().len();
    assert_eq!(
        s.queue_play(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::NotReady
    );
    reads_only(&sim, calls);
}

struct EditDuringPreparation {
    inner: SimLan,
    topologies: Arc<AtomicUsize>,
}

impl Transport for EditDuringPreparation {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#GetZoneGroupState\"")
            && self.topologies.fetch_add(1, Ordering::SeqCst) == 1
        {
            soap::enqueue_uri(&self.inner, host, "test:concurrent-edit", "", 0, false)?;
        }
        Ok(response)
    }
}

#[test]
fn a_valid_token_does_not_skip_the_final_preparation_recheck() {
    let (sim, houses) = setup(2);
    let topologies = Arc::new(AtomicUsize::new(0));
    let s = surface(
        houses,
        EditDuringPreparation {
            inner: sim.lan(),
            topologies: Arc::clone(&topologies),
        },
    );
    let view = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
    topologies.store(0, Ordering::SeqCst);
    let calls = sim.soap_log().len();
    assert_eq!(
        s.queue_play(&Client::Cli, &request(&view, 1))
            .unwrap_err()
            .code,
        ErrorCode::NotReady
    );
    assert!(topologies.load(Ordering::SeqCst) >= 2);
    assert!(!sim.soap_log()[calls..].iter().any(|e| matches!(
        e.action.as_str(),
        "SetAVTransportURI" | "Seek" | "Play" | "SetVolume"
    )));
}

#[test]
fn old_requests_can_still_select_without_either_optional_precondition() {
    let (sim, houses) = setup(1);
    let host = sim.player(ROOM).unwrap().ip;
    let s = surface(houses, sim.lan());
    let req: QueuePlayRequest = serde_json::from_value(serde_json::json!({
        "zone": ROOM, "position": 1,
    }))
    .unwrap();
    assert!(s.queue_play(&Client::Cli, &req).unwrap().changed);
    assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().track, 1);
}

#[test]
fn blank_revision_fails_before_any_speaker_read_or_write() {
    let (sim, houses) = setup(2);
    let s = surface(houses, sim.lan());
    let req = QueuePlayRequest {
        zone: ROOM.into(),
        position: 1,
        expected_uri: None,
        expected_revision: Some(" ".into()),
    };
    let calls = sim.soap_log().len();
    assert_eq!(
        s.queue_play(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(sim.soap_log().len(), calls);
}

#[test]
fn a_valid_revision_does_not_override_a_wrong_expected_uri() {
    let (sim, houses) = setup(2);
    let s = surface(houses, sim.lan());
    let view = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
    let mut req = request(&view, 1);
    req.expected_uri = Some("test:wrong-selection".into());
    let calls = sim.soap_log().len();
    assert_eq!(
        s.queue_play(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::NotReady
    );
    reads_only(&sim, calls);
}
