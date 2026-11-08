//! Create-only saved queues through real loopback SOAP, including loss of
//! the acknowledgement after the speaker has persisted the new playlist.

use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::{CoreError, HouseholdState, control, playlists, resolve_room};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::DidlObject;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::IpAddr;

fn fixture() -> (SimHandle, Vec<HouseholdState>) {
    let sim = SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap();
    let houses = ["Kitchen", "Den"]
        .iter()
        .map(|room| {
            let mut state = HouseholdState::default();
            state.apply_topology(
                &get_zone_group_state(&sim.lan(), sim.player(room).unwrap().ip).unwrap(),
            );
            state
        })
        .collect();
    (sim, houses)
}

fn queue(lan: &impl Transport, host: IpAddr) -> Vec<DidlObject> {
    content::browse_all(lan, host, QUEUE).unwrap()
}

fn seed(sim: &SimHandle, houses: &[HouseholdState], room: &str) -> Vec<DidlObject> {
    let target = resolve_room(houses, room).unwrap();
    let available = favorites::list(&sim.lan(), houses, &target.coordinator.id).unwrap();
    let tracks: Vec<_> = available
        .iter()
        .filter(|item| item.kind == FavoriteKind::Track)
        .collect();
    assert!(tracks.len() >= 2);
    for index in [0, 1, 0] {
        soap::add_uri_to_queue(
            &sim.lan(),
            target.coordinator.ip,
            tracks[index].uri.as_ref().unwrap(),
            &tracks[index].metadata,
            false,
        )
        .unwrap();
    }
    control::play_queue_from(&sim.lan(), houses, &target.coordinator.id, 2).unwrap();
    soap::seek_position(&sim.lan(), target.coordinator.ip, 37).unwrap();
    queue(&sim.lan(), target.coordinator.ip)
}

fn save_count(sim: &SimHandle) -> usize {
    sim.soap_log()
        .iter()
        .filter(|entry| entry.action == "SaveQueue")
        .count()
}

#[test]
fn a_saved_queue_preserves_playback_and_replays_all_occurrences_with_metadata() {
    let (sim, houses) = fixture();
    for room in ["Kitchen", "Den"] {
        let original = seed(&sim, &houses, room);
        let target = resolve_room(&houses, room).unwrap();
        let coordinator = &target.coordinator.id;
        let lan = sim.lan();
        let before = control::playback(&lan, &houses, coordinator).unwrap();
        let start = sim.soap_log().len();
        let saved =
            playlists::save_queue(&lan, &houses, coordinator, "  Evening & Friends  ").unwrap();
        assert_eq!(
            saved.id, "SQ:8",
            "ids are native and local to each household"
        );
        assert_eq!(saved.title, "Evening & Friends");
        assert_eq!(saved.kind, FavoriteKind::Container);
        assert_eq!(
            control::playback(&lan, &houses, coordinator).unwrap(),
            before
        );
        assert_eq!(queue(&lan, target.coordinator.ip), original);
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|entry| {
                !matches!(
                    entry.action.as_str(),
                    "GetZoneGroupState" | "Browse" | "GetTransportInfo" | "GetPositionInfo"
                )
            })
            .cloned()
            .collect();
        assert_eq!(
            writes.len(),
            1,
            "saving must not pause, seek, switch sources or edit the queue"
        );
        assert_eq!(writes[0].action, "SaveQueue");
        assert!(writes[0].args.contains(&("ObjectID".into(), String::new())));

        // A second save with the same title is another playlist, not overwrite.
        let duplicate_title =
            playlists::save_queue(&lan, &houses, coordinator, &saved.title).unwrap();
        assert_ne!(saved.id, duplicate_title.id);
        let catalog = playlists::list(&lan, &houses, coordinator).unwrap();
        assert_eq!(
            catalog
                .iter()
                .filter(|item| item.title == saved.title)
                .count(),
            2
        );

        // Clearing the live queue does not erase its saved copy. Native
        // container playback must restore order, repeated URIs and metadata.
        soap::remove_all_tracks_from_queue(&lan, target.coordinator.ip).unwrap();
        favorites::play(&lan, &houses, coordinator, &saved).unwrap();
        assert_eq!(queue(&lan, target.coordinator.ip), original);
    }
}

#[test]
fn saving_a_retained_queue_does_not_interrupt_the_current_radio_source() {
    let (sim, houses) = fixture();
    let original = seed(&sim, &houses, "Kitchen");
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let lan = sim.lan();
    let favorites = favorites::list(&lan, &houses, &target.coordinator.id).unwrap();
    let radio = favorites
        .iter()
        .find(|item| item.kind == FavoriteKind::Stream)
        .unwrap();
    favorites::play(&lan, &houses, &target.coordinator.id, radio).unwrap();
    let before = control::playback(&lan, &houses, &target.coordinator.id).unwrap();
    playlists::save_queue(&lan, &houses, &target.coordinator.id, "For later").unwrap();
    assert_eq!(
        control::playback(&lan, &houses, &target.coordinator.id).unwrap(),
        before
    );
    assert_eq!(queue(&lan, target.coordinator.ip), original);
}

struct NoRequests;

impl Transport for NoRequests {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid title must fail before any SOAP request");
    }
}

#[test]
fn invalid_titles_and_empty_queues_never_send_save_queue() {
    for title in [
        "".into(),
        " \t ".into(),
        "Dinner\nParty".into(),
        "é".repeat(512),
    ] {
        assert!(
            playlists::save_queue(&NoRequests, &[], &PlayerId("unknown".into()), &title).is_err()
        );
    }
    let (sim, houses) = fixture();
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let catalog = playlists::list(&sim.lan(), &houses, &target.coordinator.id).unwrap();
    let error =
        playlists::save_queue(&sim.lan(), &houses, &target.coordinator.id, "Empty").unwrap_err();
    assert!(error.to_string().contains("queue is empty"), "{error}");
    assert_eq!(save_count(&sim), 0);
    assert_eq!(
        playlists::list(&sim.lan(), &houses, &target.coordinator.id).unwrap(),
        catalog
    );
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    LostReply,
    MissingIdentity,
    ExistingIdentity,
    CatalogUnavailable,
}

struct FailedSave {
    lan: SimLan,
    mode: Failure,
    saves: Cell<usize>,
}

impl Transport for FailedSave {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if matches!(self.mode, Failure::CatalogUnavailable)
            && self.saves.get() > 0
            && action.ends_with("#Browse\"")
            && body.contains("<ObjectID>SQ:</ObjectID>")
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "catalog read unavailable after save".into(),
            });
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#SaveQueue\"") {
            self.saves.set(self.saves.get() + 1);
            return match self.mode {
                Failure::LostReply => Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "save reply lost after persisting".into(),
                }),
                Failure::MissingIdentity => {
                    Ok(response.replace("AssignedObjectID", "MissingIdentity"))
                }
                Failure::ExistingIdentity => Ok(response.replace("SQ:8", "SQ:1")),
                Failure::CatalogUnavailable => Ok(response),
            };
        }
        Ok(response)
    }
}

#[test]
fn an_uncertain_save_never_creates_a_second_playlist_or_claims_nothing_changed() {
    for mode in [
        Failure::LostReply,
        Failure::MissingIdentity,
        Failure::ExistingIdentity,
        Failure::CatalogUnavailable,
    ] {
        let (sim, houses) = fixture();
        let original = seed(&sim, &houses, "Kitchen");
        let target = resolve_room(&houses, "Kitchen").unwrap();
        let before = playlists::list(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        let transport = FailedSave {
            lan: sim.lan(),
            mode,
            saves: Cell::new(0),
        };
        let error = playlists::save_queue(&transport, &houses, &target.coordinator.id, "Keep this")
            .unwrap_err();
        assert!(
            matches!(error, CoreError::Uncertain(_)),
            "{mode:?}: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("refresh the saved-playlist catalog")
        );
        assert_eq!(transport.saves.get(), 1);
        assert_eq!(save_count(&sim), 1);
        let after = playlists::list(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        assert_eq!(after.len(), before.len() + 1, "{mode:?}");
        assert!(before.iter().all(|item| after.contains(item)));
        assert_eq!(queue(&sim.lan(), target.coordinator.ip), original);
    }
}

#[test]
fn a_definite_device_refusal_keeps_its_error_and_does_not_retry() {
    let (sim, houses) = fixture();
    seed(&sim, &houses, "Kitchen");
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let before = playlists::list(&sim.lan(), &houses, &target.coordinator.id).unwrap();
    sim.upnp_fault("Kitchen", "SaveQueue", 801).unwrap();
    let error = playlists::save_queue(&sim.lan(), &houses, &target.coordinator.id, "Keep this")
        .unwrap_err();
    assert!(
        matches!(
            error,
            CoreError::Proto(ProtoError::SoapFault { code: 801, .. })
        ),
        "{error}"
    );
    assert_eq!(save_count(&sim), 1);
    assert_eq!(
        playlists::list(&sim.lan(), &houses, &target.coordinator.id).unwrap(),
        before
    );
}

struct RegroupBeforeSave {
    lan: SimLan,
    member_host: IpAddr,
    coordinator: PlayerId,
    reads: Cell<usize>,
}

impl Transport for RegroupBeforeSave {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.ends_with("#GetZoneGroupState\"") {
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() == 2 {
                soap::join_group(&self.lan, self.member_host, &self.coordinator)?;
            }
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn a_group_change_during_preflight_cannot_save_an_unreviewed_groups_queue() {
    let (sim, houses) = fixture();
    let original = seed(&sim, &houses, "Kitchen");
    let target = resolve_room(&houses, "Kitchen").unwrap();
    let transport = RegroupBeforeSave {
        lan: sim.lan(),
        member_host: sim.player("Office").unwrap().ip,
        coordinator: target.coordinator.id.clone(),
        reads: Cell::new(0),
    };
    let error = playlists::save_queue(&transport, &houses, &target.coordinator.id, "Keep this")
        .unwrap_err();
    assert!(
        !matches!(error, CoreError::Uncertain(_)),
        "no save was attempted: {error}"
    );
    assert_eq!(save_count(&sim), 0);
    assert_eq!(queue(&sim.lan(), target.coordinator.ip), original);
}

#[test]
fn a_stale_address_cannot_save_a_queue_in_another_household() {
    let (sim, mut houses) = fixture();
    seed(&sim, &houses, "Den");
    let target = resolve_room(&houses, "Kitchen")
        .unwrap()
        .coordinator
        .id
        .clone();
    houses[0]
        .players
        .iter_mut()
        .find(|player| player.id == target)
        .unwrap()
        .ip = sim.player("Den").unwrap().ip;
    let error = playlists::save_queue(&sim.lan(), &houses, &target, "Wrong house").unwrap_err();
    assert!(!matches!(error, CoreError::Uncertain(_)), "{error}");
    assert_eq!(save_count(&sim), 0);
}
