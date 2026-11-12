//! Ordered explicit selections use real simulator SOAP and one action receipt.

use fsonos_api::ErrorCode;
use fsonos_api::surface::queue::{QueueTrackInput, QueueTracksRequest};
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::control;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const FIRST: &str = "spotify:track:0000000000000000000001";
const LAST: &str = "spotify:track:0000000000000000000002";
const SELECTED: &str = "spotify:track:0000000000000000000003";
const EPISODE: &str = "spotify:episode:0000000000000000000004";

fn setup(s2: bool) -> (SimHandle, Vec<HouseholdState>, PlayerId) {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Lead", model),
        SimPlayerSpec::new("Member", model),
    ];
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let host = sim.player("Lead").unwrap().ip;
    let id = PlayerId(sim.player("Lead").unwrap().uuid.clone());
    let member = PlayerId(sim.player("Member").unwrap().uuid.clone());
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let mut houses = vec![house];
    control::queue_spotify_tracks(&lan, &houses, &id, &[(FIRST, "Original"), (LAST, "Tail")])
        .unwrap()
        .unwrap();
    control::play_queue_from(&lan, &houses, &id, 1).unwrap();
    control::join(&lan, &houses, &member, &id).unwrap();
    houses[0].apply_topology(&get_zone_group_state(&lan, host).unwrap());
    soap::set_volume(&lan, host, 90).unwrap();
    (sim, houses, id)
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
    .with_action_log(Box::new(MemStore::default()), "queue-selection")
}

fn request(next: bool) -> QueueTracksRequest {
    QueueTracksRequest {
        zone: "Member".into(),
        tracks: vec![
            QueueTrackInput {
                source_uri: SELECTED.into(),
                title: Some("First occurrence".into()),
            },
            QueueTrackInput {
                source_uri: "https://open.spotify.com/episode/0000000000000000000004".into(),
                title: Some("Episode".into()),
            },
            QueueTrackInput {
                source_uri: SELECTED.into(),
                title: Some("Repeated occurrence".into()),
            },
        ],
        next,
        expected_revision: None,
    }
}

fn uris(s: &Surface) -> Vec<String> {
    s.queue(&Client::Cli, "Lead", 0, 100)
        .unwrap()
        .items
        .into_iter()
        .map(|item| spotify_uri_from_renderer_uri(item.uri.as_deref().unwrap()).unwrap())
        .collect()
}

#[test]
fn ordered_duplicates_and_episodes_are_one_undoable_add_without_playback_changes() {
    for s2 in [false, true] {
        for next in [false, true] {
            for paused in [false, true] {
                let (sim, houses, id) = setup(s2);
                let host = sim.player("Lead").unwrap().ip;
                if paused {
                    soap::pause(&sim.lan(), host).unwrap();
                }
                let original_transport = soap::get_transport_info(&sim.lan(), host).unwrap();
                let original_source = soap::get_media_info(&sim.lan(), host).unwrap().uri;
                let s = surface(houses, sim.lan());
                let mut req = request(next);
                req.expected_revision = s.queue(&Client::Cli, "Member", 0, 1).unwrap().revision;
                let before = sim.soap_log().len();
                let added = s.queue_tracks(&Client::McpStdio, &req).unwrap();
                assert_eq!(
                    (added.first, added.count, added.playing_queue),
                    (if next { 2 } else { 3 }, 3, true)
                );
                assert_eq!(
                    uris(&s),
                    if next {
                        vec![FIRST, SELECTED, EPISODE, SELECTED, LAST]
                    } else {
                        vec![FIRST, LAST, SELECTED, EPISODE, SELECTED]
                    }
                );
                assert_eq!(
                    soap::get_transport_info(&sim.lan(), host).unwrap(),
                    original_transport
                );
                assert_eq!(
                    soap::get_media_info(&sim.lan(), host).unwrap().uri,
                    original_source
                );
                assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().track, 1);
                assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 90);
                assert!(!sim.soap_log()[before..].iter().any(|entry| matches!(
                    entry.action.as_str(),
                    "Play"
                        | "Pause"
                        | "SetAVTransportURI"
                        | "SetVolume"
                        | "RemoveAllTracksFromQueue"
                )));
                assert_eq!(
                    s.recent_actions(&Client::Cli, &ActionFilter::default())
                        .unwrap()
                        .len(),
                    1
                );
                let undo = s.undo(&Client::McpStdio, false).unwrap().unwrap();
                assert!(undo.failures.is_empty(), "{undo:?}");
                assert_eq!(undo.unqueued, [id]);
                assert_eq!(uris(&s), [FIRST, LAST]);
            }
        }
    }
}

#[test]
fn queue_next_appends_while_radio_plays_without_switching_the_source() {
    let (sim, houses, id) = setup(true);
    let host = sim.player("Lead").unwrap().ip;
    control::play_uri(
        &sim.lan(),
        &houses,
        &id,
        "https://example.invalid/radio",
        "",
    )
    .unwrap();
    let s = surface(houses, sim.lan());
    let added = s.queue_tracks(&Client::Cli, &request(true)).unwrap();
    assert_eq!(
        (added.first, added.count, added.playing_queue),
        (3, 3, false)
    );
    assert_eq!(uris(&s), [FIRST, LAST, SELECTED, EPISODE, SELECTED]);
    assert_eq!(
        soap::get_media_info(&sim.lan(), host).unwrap().uri,
        "https://example.invalid/radio"
    );
}

#[test]
fn stale_revisions_invalid_inputs_and_denied_clients_send_no_adds() {
    let (sim, houses, _) = setup(true);
    let s = surface(houses, sim.lan());
    let mut req = request(false);
    req.expected_revision = s.queue(&Client::Cli, "Member", 0, 1).unwrap().revision;
    soap::enqueue_uri(
        &sim.lan(),
        sim.player("Lead").unwrap().ip,
        "test:outside-edit",
        "",
        0,
        false,
    )
    .unwrap();
    let before = sim.soap_log().len();
    assert_eq!(
        s.queue_tracks(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::QueueChanged
    );
    assert!(
        !sim.soap_log()[before..]
            .iter()
            .any(|entry| entry.action == "AddURIToQueue")
    );
    let before = sim.soap_log().len();
    assert_eq!(
        s.queue_tracks(&Client::Unknown, &request(false))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    req.tracks[1].source_uri = "spotify:album:0000000000000000000004".into();
    assert_eq!(
        s.queue_tracks(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(sim.soap_log().len(), before);
}

struct FaultyAdd {
    inner: SimLan,
    sent: Arc<AtomicUsize>,
    refuse_second: bool,
}

impl Transport for FaultyAdd {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#AddURIToQueue") {
            let sent = self.sent.fetch_add(1, Ordering::SeqCst);
            if self.refuse_second && sent == 1 {
                return Err(ProtoError::SoapFault {
                    code: 701,
                    reason: "injected second-item refusal".into(),
                });
            }
            let _ = self.inner.soap_post(host, path, action, body)?;
            return Err(ProtoError::Network {
                target: "synthetic-speaker".into(),
                detail: "injected lost add acknowledgement".into(),
            });
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn lost_replies_reconcile_once_and_a_partial_batch_stops_undo() {
    for refuse_second in [false, true] {
        let (sim, houses, _) = setup(true);
        let sent = Arc::new(AtomicUsize::new(0));
        let s = surface(
            houses,
            FaultyAdd {
                inner: sim.lan(),
                sent: Arc::clone(&sent),
                refuse_second,
            },
        );
        let result = s.queue_tracks(&Client::Cli, &request(false));
        if refuse_second {
            assert_eq!(result.unwrap_err().code, ErrorCode::OutcomeUncertain);
            assert_eq!(sent.load(Ordering::SeqCst), 2);
            assert_eq!(uris(&s), [FIRST, LAST, SELECTED]);
            assert!(
                s.undo(&Client::Cli, false)
                    .unwrap_err()
                    .detail
                    .contains("queue edit")
            );
        } else {
            assert_eq!(result.unwrap().count, 3);
            assert_eq!(sent.load(Ordering::SeqCst), 3);
            assert_eq!(uris(&s), [FIRST, LAST, SELECTED, EPISODE, SELECTED]);
            assert!(
                s.undo(&Client::Cli, false)
                    .unwrap()
                    .unwrap()
                    .failures
                    .is_empty()
            );
            assert_eq!(uris(&s), [FIRST, LAST]);
        }
    }
}
