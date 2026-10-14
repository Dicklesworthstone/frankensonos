//! Versioned queue pages and source attribution through the real SOAP parser
//! and localhost simulator. Only the timing of concurrent edits is injected.

use fsonos_api::ErrorCode;
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ROOM: &str = "Queue Room";
const RADIO: &str = "x-rincon-mp3radio://example.invalid/radio";

fn setup(generation: u8) -> (SimHandle, Vec<HouseholdState>) {
    let builder = SimHousehold::builder();
    let sim = if generation == 1 {
        builder.s1([SimPlayerSpec::new(ROOM, SimModel::Play5Gen1)])
    } else {
        builder.s2([SimPlayerSpec::new(ROOM, SimModel::One)])
    }
    .spawn()
    .unwrap();
    let host = sim.player(ROOM).unwrap().ip;
    let households = fsonos_core::inventory::survey(&sim.lan(), &[host], Duration::ZERO)
        .unwrap()
        .households;
    let id = resolve_room(&households, ROOM).unwrap().coordinator.id.clone();
    control::queue_spotify_tracks(
        &sim.lan(),
        &households,
        &id,
        &[
            ("spotify:track:SnapshotA", "First"),
            ("spotify:track:SnapshotB", "Second"),
            ("spotify:track:SnapshotC", "Third"),
        ],
    )
    .unwrap()
    .expect("synthetic Spotify favorite");
    (sim, households)
}

fn surface(
    households: Vec<HouseholdState>,
    transport: impl Transport + Send + Sync + 'static,
) -> Surface {
    let survey: Survey = Box::new(move |_| Ok(households.clone()));
    Surface::new(
        Box::new(transport),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
}

#[test]
fn pages_share_a_revision_and_verify_the_playing_track_outside_the_page() {
    for generation in [1, 2] {
        let (sim, houses) = setup(generation);
        let id = resolve_room(&houses, ROOM).unwrap().coordinator.id.clone();
        control::play_queue_from(&sim.lan(), &houses, &id, 3).unwrap();
        let s = surface(houses, sim.lan());
        let first = s.queue(&Client::Cli, ROOM, 0, 1).unwrap();
        let last = s.queue(&Client::Cli, ROOM, 2, 1).unwrap();
        assert_eq!(first.playing, Some(3));
        assert_eq!(last.playing, Some(3));
        assert!(first.revision.is_some());
        assert_eq!(first.revision, last.revision);
        assert_eq!(first.items[0].title, "First");
        assert_eq!(last.items[0].title, "Third");
        let json = serde_json::to_value(&first).unwrap();
        assert_eq!(json["revision"].as_str(), first.revision.as_deref());
    }
}

#[test]
fn a_non_queue_source_does_not_claim_a_queue_position_or_read_one() {
    let (sim, houses) = setup(2);
    let id = resolve_room(&houses, ROOM).unwrap().coordinator.id.clone();
    control::play_uri(&sim.lan(), &houses, &id, RADIO, "").unwrap();
    let s = surface(houses, sim.lan());
    let before = sim.soap_log().len();
    let queue = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
    assert_eq!(queue.total, 3);
    assert_eq!(queue.playing, None);
    assert!(
        !sim.soap_log()[before..]
            .iter()
            .any(|entry| entry.action == "GetPositionInfo")
    );
}

struct Editing {
    inner: SimLan,
    edits: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}

impl Transport for Editing {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if action.ends_with("#GetMediaInfo\"")
            && self
                .edits
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            soap::enqueue_uri(&self.inner, host, "test:concurrent-track", "", 0, false)?;
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn an_edit_during_the_read_discards_the_old_page_and_restarts() {
    let (sim, houses) = setup(2);
    let edits = Arc::new(AtomicUsize::new(1));
    let s = surface(
        houses,
        Editing {
            inner: sim.lan(),
            edits: Arc::clone(&edits),
            reads: Arc::new(AtomicUsize::new(0)),
        },
    );
    let result = s.queue(&Client::Cli, ROOM, 0, 100).unwrap();
    assert_eq!(result.total, 4);
    assert_eq!(result.items.len(), 4);
    assert_eq!(edits.load(Ordering::SeqCst), 0);
    assert!(result.revision.is_some());
}

#[test]
fn continuous_queue_edits_are_bounded_and_never_return_a_mixed_page() {
    let (sim, houses) = setup(2);
    let reads = Arc::new(AtomicUsize::new(0));
    let s = surface(
        houses,
        Editing {
            inner: sim.lan(),
            edits: Arc::new(AtomicUsize::new(100)),
            reads: Arc::clone(&reads),
        },
    );
    let failure = s.queue(&Client::Cli, ROOM, 0, 100).unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotReady);
    assert!(reads.load(Ordering::SeqCst) <= 18, "three bounded attempts");
}

#[test]
fn overflowing_ranges_fail_without_sending_a_browse() {
    let (sim, houses) = setup(2);
    let s = surface(houses, sim.lan());
    let before = sim.soap_log().len();
    let failure = s.queue(&Client::Cli, ROOM, u32::MAX, 1).unwrap_err();
    assert_eq!(failure.code, ErrorCode::InvalidArgument);
    assert_eq!(sim.soap_log().len(), before);
}
