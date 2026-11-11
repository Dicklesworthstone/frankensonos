//! Source replacements expose the same durable, reviewed Undo on every
//! surface. Unsupported or uncertain attempts remain explicit barriers.

use fsonos_api::log::ActionDto;
use fsonos_api::plan::plan_play;
use fsonos_api::surface::sonos_playlists::PlaySonosPlaylistRequest;
use fsonos_api::{ErrorCode, OutcomeDto, PlayFavoriteRequest, PlayRequest, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, LoggedAction, MemStore};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DUPLICATE: &str = "x-file-cifs://example.invalid/repeated.flac";
const LAST: &str = "x-file-cifs://example.invalid/last.flac";
const EXTERNAL: &str = "x-file-cifs://example.invalid/another-controller.flac";

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    RecoveredPlayReply,
    LostPlayReply,
    ReadAfterStart,
    EditDuringCap,
}

struct Wire {
    inner: SimLan,
    fault: Mutex<Fault>,
    started: AtomicBool,
    edited: AtomicBool,
}

fn lost() -> ProtoError {
    ProtoError::Network {
        target: "test-player".into(),
        detail: "injected lost response".into(),
    }
}

impl Transport for Wire {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let fault = *self.fault.lock().unwrap();
        if matches!(fault, Fault::LostPlayReply)
            && self.started.load(Ordering::Acquire)
            && action.ends_with("#GetMediaInfo\"")
        {
            // The start itself happened, but neither its acknowledgement nor
            // the reconciliation read can establish that result for us.
            return Err(lost());
        }
        if matches!(fault, Fault::ReadAfterStart)
            && self.started.load(Ordering::Acquire)
            && action.ends_with("#Browse\"")
        {
            return Err(lost());
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#SetVolume\"")
            && matches!(fault, Fault::EditDuringCap)
            && !self.edited.swap(true, Ordering::AcqRel)
        {
            soap::add_uri_to_queue(&self.inner, host, EXTERNAL, "", false).unwrap();
        }
        if action.ends_with("#Play\"") {
            self.started.store(true, Ordering::Release);
            if matches!(fault, Fault::LostPlayReply | Fault::RecoveredPlayReply) {
                return Err(lost());
            }
        }
        Ok(reply)
    }
}

struct Fixture {
    sim: SimHandle,
    surface: Surface,
    wire: Arc<Wire>,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
            SimPlayerSpec::new("Guest", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let host = sim.player("Alpha").unwrap().ip;
        let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        soap::join_group(&lan, sim.player("Beta").unwrap().ip, &coordinator).unwrap();
        for uri in [DUPLICATE, DUPLICATE, LAST] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, host, &coordinator).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 17).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
        soap::play(&lan, host).unwrap();
        soap::pause(&lan, host).unwrap();
        for (name, volume) in [("Alpha", 80), ("Beta", 70), ("Guest", 60)] {
            soap::set_volume(&lan, sim.player(name).unwrap().ip, volume).unwrap();
        }
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        household.id = Some(HouseholdId(sim.player("Alpha").unwrap().household.clone()));
        let houses = vec![household];
        let known = houses.clone();
        let wire = Arc::new(Wire {
            inner: lan,
            fault: Mutex::new(Fault::None),
            started: AtomicBool::new(false),
            edited: AtomicBool::new(false),
        });
        let surface = Surface::new(
            Box::new(Arc::clone(&wire)),
            Box::new(move |_| Ok(known.clone())),
            Policy::from_toml("[defaults]\nmax_volume = 25\n").unwrap(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "http");
        Self {
            sim,
            surface,
            wire,
            houses,
            coordinator,
            host,
        }
    }

    fn request(zone: &str, album: bool) -> PlayRequest {
        PlayRequest {
            zone: zone.into(),
            source_uri: format!(
                "spotify:{}:0000000000000000000001",
                if album { "album" } else { "playlist" }
            ),
            title: Some("Replacement".into()),
        }
    }

    fn play(&self, source: u8, client: &Client) -> OutcomeDto {
        match source {
            0 | 1 => self.surface.control(client, "play", |rooms| {
                plan_play(rooms, &Self::request("Beta", source == 0))
            }),
            2 => {
                let favorite = favorites::list(&self.sim.lan(), &self.houses, &self.coordinator)
                    .unwrap()
                    .into_iter()
                    .find(|f| f.kind == FavoriteKind::Container)
                    .unwrap();
                self.surface.play_favorite(
                    client,
                    &PlayFavoriteRequest {
                        zone: "Beta".into(),
                        favorite: favorite.id,
                    },
                )
            }
            _ => self.surface.play_sonos_playlist(
                client,
                &PlaySonosPlaylistRequest {
                    zone: "Beta".into(),
                    playlist: "SQ:1".into(),
                },
            ),
        }
        .unwrap()
    }

    fn action(&self) -> LoggedAction {
        self.surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .remove(0)
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn writes_since(&self, marker: usize) -> Vec<String> {
        self.sim.soap_log()[marker..]
            .iter()
            .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
            .map(|call| call.action.clone())
            .collect()
    }

    fn restored(&self) {
        assert_eq!(self.queue(), [DUPLICATE, DUPLICATE, LAST]);
        let position = soap::get_position_info(&self.sim.lan(), self.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(position.uri, DUPLICATE);
        assert_eq!(
            get_play_mode(&self.sim.lan(), self.host).unwrap(),
            PlayMode::RepeatAll
        );
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), self.host)
                .unwrap()
                .state,
            TransportState::Paused
        );
        let volumes = ["Alpha", "Beta", "Guest"].map(|name| {
            soap::get_volume(&self.sim.lan(), self.sim.player(name).unwrap().ip).unwrap()
        });
        assert_eq!(
            volumes,
            [25, 25, 60],
            "Undo must retain the caller's playback caps"
        );
        assert!(
            browse_snapshot(&self.sim.lan(), self.sim.player("Guest").unwrap().ip, QUEUE)
                .unwrap()
                .objects
                .is_empty()
        );
    }

    fn barrier(&self) {
        let action = self.action();
        assert!(!ActionDto::from(&action).undoable);
        assert!(
            action
                .action
                .before_state
                .as_deref()
                .unwrap()
                .contains("queue replacement")
        );
        let marker = self.sim.soap_log().len();
        let refused = self.surface.undo(&Client::Cli, false).unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidArgument);
        assert!(self.writes_since(marker).is_empty());
    }
}

#[test]
fn album_playlist_favorite_and_saved_playlist_offer_reviewed_undo_on_s1_and_s2() {
    for generation in [1, 2] {
        for source in 0..4 {
            let f = Fixture::new(generation);
            let client = if source % 2 == 0 {
                Client::LoopbackHttp
            } else {
                Client::McpStdio
            };
            let outcome = f.play(source, &client);
            assert!(outcome.changed);
            assert_ne!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
            let action = f.action();
            assert!(ActionDto::from(&action).undoable);
            let saved = BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
            assert_eq!(saved.queue_undo.len(), 1);
            assert_eq!(saved.queue_undo[0].coordinator(), &f.coordinator);
            let preview = f.surface.preview_undo(&client).unwrap().unwrap();
            assert_eq!(preview.id, action.id);
            assert!(preview.undoable);
            let marker = f.sim.soap_log().len();
            let restored = f.surface.undo_checked(&client, preview.id).unwrap();
            assert_eq!(restored.undone, Some(action.id));
            assert!(restored.failures.is_empty(), "{restored:?}");
            f.restored();
            assert!(!f.writes_since(marker).iter().any(|action| action == "Play"));
        }
    }
}

#[test]
fn later_queue_or_group_ownership_changes_refuse_even_forced_surface_undo() {
    for change in 0..3 {
        let f = Fixture::new(2);
        f.play(0, &Client::Cli);
        assert!(ActionDto::from(&f.action()).undoable);
        match change {
            0 => {
                soap::add_uri_to_queue(&f.sim.lan(), f.host, EXTERNAL, "", false).unwrap();
            }
            1 => {
                let member = f.sim.player("Beta").unwrap();
                soap::leave_group(&f.sim.lan(), member.ip, &PlayerId(member.uuid.clone())).unwrap();
            }
            _ => f.sim.reboot("Alpha", Duration::ZERO).unwrap(),
        }
        let current = f.queue();
        let marker = f.sim.soap_log().len();
        assert!(f.surface.undo_forced(&Client::Cli, true, true).is_err());
        assert!(f.writes_since(marker).is_empty());
        assert_eq!(f.queue(), current);
    }
}

#[test]
fn uncertain_start_keeps_a_barrier_and_never_retries_the_replacement() {
    let f = Fixture::new(2);
    *f.wire.fault.lock().unwrap() = Fault::LostPlayReply;
    let marker = f.sim.soap_log().len();
    let failed = f
        .surface
        .control(&Client::Cli, "play", |rooms| {
            plan_play(rooms, &Fixture::request("Beta", true))
        })
        .unwrap_err();
    assert_eq!(failed.code, ErrorCode::OutcomeUncertain);
    let writes = f.writes_since(marker);
    assert_eq!(writes.iter().filter(|action| *action == "Play").count(), 1);
    assert_eq!(
        writes
            .iter()
            .filter(|action| *action == "RemoveTrackRangeFromQueue")
            .count(),
        1
    );
    f.barrier();
}

#[test]
fn a_lost_start_reply_confirmed_by_readback_still_records_the_exact_undo() {
    let f = Fixture::new(2);
    *f.wire.fault.lock().unwrap() = Fault::RecoveredPlayReply;
    let marker = f.sim.soap_log().len();
    assert!(f.play(0, &Client::LoopbackHttp).changed);
    let writes = f.writes_since(marker);
    assert_eq!(writes.iter().filter(|action| *action == "Play").count(), 1);
    assert_eq!(
        writes
            .iter()
            .filter(|action| *action == "RemoveTrackRangeFromQueue")
            .count(),
        1
    );
    let preview = f
        .surface
        .preview_undo(&Client::LoopbackHttp)
        .unwrap()
        .unwrap();
    assert!(preview.undoable);
    let restored = f
        .surface
        .undo_checked(&Client::LoopbackHttp, preview.id)
        .unwrap();
    assert!(restored.failures.is_empty(), "{restored:?}");
    f.restored();
}

#[test]
fn successful_play_with_unreadable_result_or_changed_backup_has_no_undo_claim() {
    for fault in [Fault::ReadAfterStart, Fault::EditDuringCap] {
        let f = Fixture::new(2);
        *f.wire.fault.lock().unwrap() = fault;
        assert!(f.play(0, &Client::LoopbackHttp).changed);
        if matches!(fault, Fault::EditDuringCap) {
            assert!(f.wire.edited.load(Ordering::Acquire));
        }
        assert_ne!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
        f.barrier();
    }
}

#[test]
fn an_unselected_or_oversized_old_queue_does_not_block_play_or_claim_restore() {
    for oversized in [false, true] {
        let f = Fixture::new(2);
        if oversized {
            for _ in 3..=fsonos_core::scenes::MAX_QUEUE_BACKUP_ENTRIES {
                soap::add_uri_to_queue(&f.sim.lan(), f.host, LAST, "", false).unwrap();
            }
        } else {
            soap::set_av_transport_uri(
                &f.sim.lan(),
                f.host,
                "https://example.invalid/radio.mp3",
                "",
            )
            .unwrap();
        }
        assert!(f.play(0, &Client::Cli).changed);
        f.barrier();
    }
}

#[test]
fn a_set_with_an_unbacked_replacement_cannot_offer_a_partial_undo() {
    let f = Fixture::new(2);
    let outcome = f
        .surface
        .control_set(&Client::Cli, "play", |rooms| {
            ["Beta", "Guest"]
                .iter()
                .map(|zone| plan_play(rooms, &Fixture::request(zone, true)))
                .collect()
        })
        .unwrap();
    assert!(outcome.changed);
    assert_ne!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
    f.barrier();
}

#[test]
fn disjoint_replacements_restore_both_captured_queues_from_one_reviewed_action() {
    let f = Fixture::new(2);
    let guest = f.sim.player("Guest").unwrap();
    let guest_id = PlayerId(guest.uuid.clone());
    soap::add_uri_to_queue(&f.sim.lan(), guest.ip, EXTERNAL, "", false).unwrap();
    soap::play_from_queue(&f.sim.lan(), guest.ip, &guest_id).unwrap();
    soap::seek_track(&f.sim.lan(), guest.ip, 1).unwrap();
    soap::seek_position(&f.sim.lan(), guest.ip, 9).unwrap();
    soap::play(&f.sim.lan(), guest.ip).unwrap();
    soap::pause(&f.sim.lan(), guest.ip).unwrap();
    let outcome = f
        .surface
        .control_set(&Client::LoopbackHttp, "play", |rooms| {
            ["Beta", "Guest"]
                .iter()
                .map(|zone| plan_play(rooms, &Fixture::request(zone, true)))
                .collect()
        })
        .unwrap();
    assert!(outcome.changed);
    let action = f.action();
    let saved = BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
    assert_eq!(saved.queue_undo.len(), 2);
    let preview = f
        .surface
        .preview_undo(&Client::LoopbackHttp)
        .unwrap()
        .unwrap();
    assert_eq!(preview.id, action.id);
    let restored = f
        .surface
        .undo_checked(&Client::LoopbackHttp, preview.id)
        .unwrap();
    assert!(restored.failures.is_empty(), "{restored:?}");
    assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
    for (host, slot, seconds, uri) in [(f.host, 2, 17, DUPLICATE), (guest.ip, 1, 9, EXTERNAL)] {
        let position = soap::get_position_info(&f.sim.lan(), host).unwrap();
        assert_eq!(
            (position.track, position.position_secs),
            (slot, Some(seconds))
        );
        assert_eq!(position.uri, uri);
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), host).unwrap().state,
            TransportState::Paused
        );
        assert_eq!(soap::get_volume(&f.sim.lan(), host).unwrap(), 25);
    }
}

#[test]
fn two_replacements_of_the_same_queue_do_not_claim_one_coherent_undo() {
    let f = Fixture::new(2);
    let outcome = f
        .surface
        .control_set(&Client::Cli, "play", |rooms| {
            [true, false]
                .into_iter()
                .map(|album| plan_play(rooms, &Fixture::request("Beta", album)))
                .collect()
        })
        .unwrap();
    assert!(outcome.changed);
    f.barrier();
}
