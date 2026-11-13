//! Ordered explicit selections use real simulator SOAP and one action receipt.

use fsonos_api::ErrorCode;
use fsonos_api::surface::queue::{QueueAddRequest, QueueRequest, QueueTrackInput, QueueTracksRequest};
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::control;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, LoggedAction, MemStore, Store};
use fsonos_proto::control as soap;
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::didl::{SpotifyRenderParams, item_metadata, spotify_track_didl, spotify_uri_from_renderer_uri};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

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
    let original_transport = soap::get_transport_info(&sim.lan(), host).unwrap();
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
    let undo = s.undo(&Client::Cli, false).unwrap().unwrap();
    assert_eq!(undo.unqueued, [id]);
    assert_eq!(uris(&s), [FIRST, LAST]);
    assert_eq!(
        soap::get_media_info(&sim.lan(), host).unwrap().uri,
        "https://example.invalid/radio"
    );
    assert_eq!(soap::get_transport_info(&sim.lan(), host).unwrap(), original_transport);
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


/// Reload only durable action rows into a fresh surface, with no undo cache.
fn reloaded_surface(
    houses: Vec<HouseholdState>,
    lan: SimLan,
    mut rows: Vec<LoggedAction>,
) -> Surface {
    rows.sort_by_key(|row| row.id);
    let mut store = MemStore::default();
    for row in rows {
        assert_eq!(store.record_action(&row.action).unwrap(), row.id);
    }
    Surface::new(
        Box::new(lan),
        Box::new(move |_| Ok(houses.clone())),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "queue-selection")
}

/// Replace every occurrence with a new occurrence of exactly the same URI.
/// Renderer metadata comes from this synthetic household, never guessed.
fn replace_identical(lan: &SimLan, host: IpAddr, params: &SpotifyRenderParams) {
    let original = control::queue_removal::read(lan, host).unwrap();
    let objects = browse_all(lan, host, QUEUE).unwrap();
    soap::remove_all_tracks_from_queue(lan, host).unwrap();
    for object in objects {
        let uri = &object.res.as_ref().unwrap().uri;
        let metadata = match spotify_uri_from_renderer_uri(uri) {
            Some(source) if source.starts_with("spotify:track:") => {
                spotify_track_didl(&source, &object.title, params)
            }
            _ => item_metadata(&object),
        };
        soap::enqueue_uri(lan, host, uri, &metadata, 0, false).unwrap();
    }
    let replacement = control::queue_removal::read(lan, host).unwrap();
    assert_eq!(replacement.uris, original.uris);
    assert_ne!(replacement.revision, original.revision);
}

#[derive(Debug, Clone, Copy)]
enum Addition {
    Single,
    Favorite,
    Batch,
}

fn add_selection(s: &Surface, sim: &SimHandle, houses: &[HouseholdState], kind: Addition) {
    match kind {
        Addition::Single => {
            s.queue_add(&Client::Cli, &QueueAddRequest {
                zone: "Member".into(),
                source_uri: SELECTED.into(),
                title: Some("Single occurrence".into()),
                next: false,
            }).unwrap();
        }
        Addition::Favorite => {
            let id = PlayerId(sim.player("Lead").unwrap().uuid.clone());
            let favorite = fsonos_core::favorites::list(&sim.lan(), houses, &id)
                .unwrap()
                .into_iter()
                .find(|favorite| favorite.kind == fsonos_core::favorites::FavoriteKind::Container)
                .unwrap();
            s.queue_request(&Client::Cli, &QueueRequest {
                zone: "Member".into(),
                favorite: Some(favorite.id),
                ..QueueRequest::default()
            }).unwrap();
        }
        Addition::Batch => {
            s.queue_tracks(&Client::Cli, &request(false)).unwrap();
        }
    }
}

fn assert_no_queue_writes(sim: &SimHandle, since: usize) {
    assert!(!sim.soap_log()[since..].iter().any(|entry| matches!(
        entry.action.as_str(),
        "AddURIToQueue" | "RemoveTrackRangeFromQueue" | "RemoveAllTracksFromQueue"
            | "SetAVTransportURI" | "Play"
    )));
}

#[test]
fn interleaved_successive_undo_survives_action_log_reload() {
    for s2 in [false, true] {
        let (sim, houses, id) = setup(s2);
        let s = surface(houses.clone(), sim.lan());
        s.queue_tracks(&Client::Cli, &request(false)).unwrap(); // A
        s.queue_tracks(&Client::Cli, &request(false)).unwrap(); // B
        assert_eq!(s.undo(&Client::Cli, false).unwrap().unwrap().unqueued, [id.clone()]);
        s.queue_tracks(&Client::Cli, &request(false)).unwrap(); // C after undo B
        assert_eq!(s.undo(&Client::Cli, false).unwrap().unwrap().unqueued, [id.clone()]);
        let rows = s.recent_actions(&Client::Cli, &ActionFilter::default()).unwrap();
        assert_eq!(rows.iter().filter(|row| row.action.undo_of.is_some()).count(), 2);
        drop(s);
        let reloaded = reloaded_surface(houses, sim.lan(), rows);
        // This requires both logged revision transitions in chronological order.
        let undone = reloaded.undo(&Client::Cli, false).unwrap().unwrap();
        assert_eq!(undone.unqueued, [id]);
        assert!(undone.unqueue_failures.is_empty());
        assert_eq!(uris(&reloaded), [FIRST, LAST]);
    }
}

#[test]
fn identical_external_replacements_are_never_undone_even_with_force() {
    for s2 in [false, true] {
        for kind in [Addition::Single, Addition::Favorite, Addition::Batch] {
            let (sim, houses, id) = setup(s2);
            let host = sim.player("Lead").unwrap().ip;
            let params = control::spotify_params(&sim.lan(), &houses, &id).unwrap().unwrap();
            let s = surface(houses.clone(), sim.lan());
            add_selection(&s, &sim, &houses, kind);
            replace_identical(&sim.lan(), host, &params);
            let replacement = control::queue_removal::read(&sim.lan(), host).unwrap();
            for force in [false, true] {
                let start = sim.soap_log().len();
                let error = s.undo(&Client::Cli, force).unwrap_err();
                assert!(error.detail.contains("queue changed"), "{kind:?}: {error:?}");
                assert_no_queue_writes(&sim, start);
                assert_eq!(control::queue_removal::read(&sim.lan(), host).unwrap(), replacement);
            }
        }
    }
}

#[test]
fn speaker_reboot_cannot_reauthorize_old_queued_occurrences() {
    for s2 in [false, true] {
        for kind in [Addition::Single, Addition::Favorite, Addition::Batch] {
            let (sim, houses, _) = setup(s2);
            let host = sim.player("Lead").unwrap().ip;
            let s = surface(houses.clone(), sim.lan());
            add_selection(&s, &sim, &houses, kind);
            let before = control::queue_removal::read(&sim.lan(), host).unwrap();
            sim.reboot("Lead", Duration::ZERO).unwrap();
            // The simulator retains the same revision and identical contents.
            assert_eq!(control::queue_removal::read(&sim.lan(), host).unwrap(), before);
            let start = sim.soap_log().len();
            let error = s.undo(&Client::Cli, true).unwrap_err();
            assert!(error.detail.contains("boot changed"), "{kind:?}: {error:?}");
            assert_no_queue_writes(&sim, start);
        }
    }
}

#[test]
fn an_identical_external_edit_after_our_undo_invalidates_its_rebase() {
    let (sim, houses, id) = setup(true);
    let host = sim.player("Lead").unwrap().ip;
    let params = control::spotify_params(&sim.lan(), &houses, &id).unwrap().unwrap();
    let s = surface(houses.clone(), sim.lan());
    s.queue_tracks(&Client::Cli, &request(false)).unwrap();
    s.queue_tracks(&Client::Cli, &request(false)).unwrap();
    s.undo(&Client::Cli, false).unwrap().unwrap();
    let rows = s.recent_actions(&Client::Cli, &ActionFilter::default()).unwrap();
    drop(s);
    replace_identical(&sim.lan(), host, &params);
    let replacement = control::queue_removal::read(&sim.lan(), host).unwrap();
    let reloaded = reloaded_surface(houses, sim.lan(), rows);
    let start = sim.soap_log().len();
    assert!(reloaded.undo(&Client::Cli, true).unwrap_err().detail.contains("queue changed"));
    assert_no_queue_writes(&sim, start);
    assert_eq!(control::queue_removal::read(&sim.lan(), host).unwrap(), replacement);
}

#[test]
fn legacy_action_rows_without_proof_cannot_be_forced() {
    let (sim, houses, _) = setup(true);
    let s = surface(houses.clone(), sim.lan());
    s.queue_tracks(&Client::Cli, &request(false)).unwrap();
    let mut rows = s.recent_actions(&Client::Cli, &ActionFilter::default()).unwrap();
    let mut before: serde_json::Value =
        serde_json::from_str(rows[0].action.before_state.as_deref().unwrap()).unwrap();
    before["queued"][0].as_object_mut().unwrap().remove("proof");
    rows[0].action.before_state = Some(before.to_string());
    drop(s);
    let reloaded = reloaded_surface(houses, sim.lan(), rows);
    let start = sim.soap_log().len();
    let error = reloaded.undo(&Client::Cli, true).unwrap_err();
    assert!(error.detail.contains("no retained ownership proof"), "{error:?}");
    assert_no_queue_writes(&sim, start);
}

/// A controller can replace the queue in the final request/readback windows.
struct ReplaceAroundWrite {
    inner: SimLan,
    params: SpotifyRenderParams,
    armed: Arc<AtomicBool>,
    action: &'static str,
    after: bool,
}

impl Transport for ReplaceAroundWrite {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let replace = action.trim_matches('"').ends_with(self.action)
            && self.armed.swap(false, Ordering::SeqCst);
        if replace && !self.after {
            replace_identical(&self.inner, host, &self.params);
        }
        let result = self.inner.soap_post(host, path, action, body);
        if replace && self.after {
            replace_identical(&self.inner, host, &self.params);
        }
        result
    }
}

#[test]
fn replacement_races_around_each_add_produce_an_undo_barrier() {
    for s2 in [false, true] {
        for kind in [Addition::Single, Addition::Favorite, Addition::Batch] {
            for after in [false, true] {
                let (sim, houses, id) = setup(s2);
                let params = control::spotify_params(&sim.lan(), &houses, &id).unwrap().unwrap();
                let armed = Arc::new(AtomicBool::new(false));
                let s = surface(houses.clone(), ReplaceAroundWrite {
                    inner: sim.lan(),
                    params,
                    armed: Arc::clone(&armed),
                    action: "#AddURIToQueue",
                    after,
                });
                s.queue_tracks(&Client::Cli, &request(false)).unwrap(); // older action A
                armed.store(true, Ordering::SeqCst);
                // The add itself succeeds, but the extra revision steps cannot
                // grant ownership or bridge A across the external replacement.
                add_selection(&s, &sim, &houses, kind);
                assert!(!armed.load(Ordering::SeqCst));
                let start = sim.soap_log().len();
                let error = s.undo(&Client::Cli, true).unwrap_err();
                assert!(error.detail.contains("without verified undo ownership"),
                    "{kind:?}, after={after}: {error:?}");
                assert_no_queue_writes(&sim, start);
            }
        }
    }
}

#[test]
fn replacement_after_native_removal_does_not_issue_a_rebase_receipt() {
    for s2 in [false, true] {
        let (sim, houses, id) = setup(s2);
        let params = control::spotify_params(&sim.lan(), &houses, &id).unwrap().unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let s = surface(houses, ReplaceAroundWrite {
            inner: sim.lan(),
            params,
            armed: Arc::clone(&armed),
            action: "#RemoveTrackRangeFromQueue",
            after: true,
        });
        s.queue_tracks(&Client::Cli, &request(false)).unwrap(); // A
        s.queue_tracks(&Client::Cli, &request(false)).unwrap(); // B
        armed.store(true, Ordering::SeqCst);
        let undone = s.undo(&Client::Cli, false).unwrap().unwrap();
        assert_eq!(undone.unqueued, [id]);
        assert!(undone.unqueue_failures.is_empty(), "{undone:?}");
        let rows = s.recent_actions(&Client::Cli, &ActionFilter::default()).unwrap();
        let before: serde_json::Value = serde_json::from_str(
            rows[0].action.before_state.as_deref().unwrap_or("{}")
        ).unwrap();
        assert!(before.get("queued_restored").is_none());
        let start = sim.soap_log().len();
        assert!(s.undo(&Client::Cli, true).unwrap_err().detail.contains("queue changed"));
        assert_no_queue_writes(&sim, start);
    }
}

struct UnknownBoot(SimLan);

impl Transport for UnknownBoot {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.0.soap_post(host, path, action, body)?;
        Ok(if action.contains("#GetZoneGroupState") {
            answer.replace("BootSeq=", "MissingBootSeq=")
        } else {
            answer
        })
    }
}

#[test]
fn unknown_boot_identity_keeps_successful_adds_but_stops_undo() {
    for kind in [Addition::Single, Addition::Favorite, Addition::Batch] {
        let (sim, houses, _) = setup(true);
        let s = surface(houses.clone(), UnknownBoot(sim.lan()));
        add_selection(&s, &sim, &houses, kind);
        let start = sim.soap_log().len();
        let error = s.undo(&Client::Cli, true).unwrap_err();
        assert!(error.detail.contains("without verified undo ownership"), "{kind:?}: {error:?}");
        assert_no_queue_writes(&sim, start);
    }
}
