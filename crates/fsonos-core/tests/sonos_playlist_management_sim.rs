//! Saved-playlist lifecycle through native S1/S2 SOAP. Catalog management is
//! independent of the live queue, transport and other entries with equal titles.

use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::playlists::{PlaylistChange, PlaylistContents};
use fsonos_core::{CoreError, HouseholdState, control, playlists};
use fsonos_proto::content::{self, BrowseSnapshot, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const SELECTED: &str = "SQ:7";
const RENAMED: &str = r"Dvořák, Bach & <friends> \ Live";

struct Fixture {
    sim: SimHandle,
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
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Alpha").unwrap().ip;
        let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        house.id = Some(HouseholdId(sim.player("Alpha").unwrap().household.clone()));
        let houses = vec![house];
        let selected = playlists::list(&sim.lan(), &houses, &coordinator)
            .unwrap()
            .into_iter()
            .find(|playlist| playlist.id == SELECTED)
            .unwrap();
        favorites::play(&sim.lan(), &houses, &coordinator, &selected).unwrap();
        soap::seek_track(&sim.lan(), host, 2).unwrap();
        soap::seek_position(&sim.lan(), host, 17).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::RepeatAll).unwrap();
        soap::set_volume(&sim.lan(), host, 28).unwrap();
        Self {
            sim,
            houses,
            coordinator,
            host,
        }
    }

    fn observed(&self) -> PlaylistContents {
        playlists::contents(
            &self.sim.lan(),
            &self.houses,
            &self.coordinator,
            SELECTED,
            0,
            100,
            None,
        )
        .unwrap()
    }

    fn change<T: Transport + ?Sized>(
        &self,
        transport: &T,
        remove: bool,
        revision: &str,
    ) -> Result<PlaylistChange, CoreError> {
        if remove {
            playlists::remove(
                transport,
                &self.houses,
                &self.coordinator,
                SELECTED,
                revision,
            )
        } else {
            playlists::rename(
                transport,
                &self.houses,
                &self.coordinator,
                SELECTED,
                revision,
                RENAMED,
            )
        }
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action.clone())
            .collect()
    }

    fn playback(&self) -> (BrowseSnapshot, control::Playback, PlayMode, u8, bool) {
        let lan = self.sim.lan();
        (
            content::browse_snapshot(&lan, self.host, QUEUE).unwrap(),
            control::playback(&lan, &self.houses, &self.coordinator).unwrap(),
            get_play_mode(&lan, self.host).unwrap(),
            soap::get_volume(&lan, self.host).unwrap(),
            soap::get_mute(&lan, self.host).unwrap(),
        )
    }
}

#[test]
fn rename_and_remove_keep_playing_occurrences_and_other_equal_titles_untouched() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let before = f.playback();
        let catalog = playlists::list(&lan, &f.houses, &f.coordinator).unwrap();
        let other = catalog
            .iter()
            .find(|playlist| playlist.id == "SQ:1")
            .unwrap();
        let observed = f.observed();
        let start = f.sim.soap_log().len();
        let renamed = playlists::rename(
            &lan,
            &f.houses,
            &f.coordinator,
            SELECTED,
            &observed.revision,
            &format!("  {}  ", other.title),
        )
        .unwrap();
        assert!(renamed.changed);
        assert_eq!(renamed.playlist.id, SELECTED);
        assert_eq!(renamed.playlist.title, other.title);
        assert_eq!(renamed.playlist.uri, observed.playlist.uri);
        assert_eq!(f.writes_since(start), ["UpdateObject"]);
        assert_eq!(f.playback(), before);

        let after = f.observed();
        assert_eq!(after.playlist, renamed.playlist);
        assert_eq!(after.items, observed.items);
        assert_ne!(after.revision, observed.revision);
        let start = f.sim.soap_log().len();
        let unchanged = playlists::rename(
            &lan,
            &f.houses,
            &f.coordinator,
            SELECTED,
            &after.revision,
            &other.title,
        )
        .unwrap();
        assert!(!unchanged.changed);
        assert_eq!(unchanged.playlist, after.playlist);
        assert!(f.writes_since(start).is_empty());

        let removed =
            playlists::remove(&lan, &f.houses, &f.coordinator, SELECTED, &after.revision).unwrap();
        assert!(removed.changed);
        assert_eq!(removed.playlist, renamed.playlist);
        assert_eq!(f.writes_since(start), ["DestroyObject"]);
        assert_eq!(f.playback(), before);
        let remaining = playlists::list(&lan, &f.houses, &f.coordinator).unwrap();
        assert_eq!(
            remaining,
            catalog
                .into_iter()
                .filter(|playlist| playlist.id != SELECTED)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn managing_retained_playlists_leaves_a_radio_source_and_queue_running() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let radio = favorites::list(&lan, &f.houses, &f.coordinator)
            .unwrap()
            .into_iter()
            .find(|favorite| favorite.kind == FavoriteKind::Stream)
            .unwrap();
        favorites::play(&lan, &f.houses, &f.coordinator, &radio).unwrap();
        let before = f.playback();
        let observed = f.observed();
        let start = f.sim.soap_log().len();
        let renamed = f.change(&lan, false, &observed.revision).unwrap();
        assert_eq!(renamed.playlist.title, RENAMED);
        let after = f.observed();
        f.change(&lan, true, &after.revision).unwrap();
        assert_eq!(f.writes_since(start), ["UpdateObject", "DestroyObject"]);
        assert_eq!(f.playback(), before);
    }
}

struct NoRequests;

impl Transport for NoRequests {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid playlist management must fail before I/O");
    }
}

#[test]
fn ids_revisions_and_titles_are_validated_before_any_speaker_request() {
    let player = PlayerId("unknown".into());
    for id in [
        "",
        "SQ:",
        "SQ:7/1",
        "Q:0",
        "Evening",
        "file:///jffs/settings/savedqueues.rsq",
    ] {
        assert!(playlists::remove(&NoRequests, &[], &player, id, "revision").is_err());
        assert!(playlists::rename(&NoRequests, &[], &player, id, "revision", RENAMED).is_err());
    }
    for revision in [
        "".into(),
        "  ".into(),
        "bad\nrevision".into(),
        "x".repeat(4097),
    ] {
        assert!(playlists::remove(&NoRequests, &[], &player, SELECTED, &revision).is_err());
        assert!(
            playlists::rename(&NoRequests, &[], &player, SELECTED, &revision, RENAMED).is_err()
        );
    }
    for title in [
        "".into(),
        " \t ".into(),
        "Party\nMix".into(),
        "é".repeat(512),
    ] {
        assert!(
            playlists::rename(&NoRequests, &[], &player, SELECTED, "revision", &title).is_err()
        );
    }
}

#[test]
fn a_changed_playlist_revision_refuses_rename_remove_and_an_apparent_noop() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let observed = f.observed();
        content::rename_object(&lan, f.host, SELECTED, &observed.playlist.title, RENAMED).unwrap();
        let start = f.sim.soap_log().len();
        for remove in [false, true] {
            assert!(matches!(
                f.change(&lan, remove, &observed.revision),
                Err(CoreError::PlaylistChanged(_))
            ));
        }
        assert!(f.writes_since(start).is_empty());
        assert_eq!(f.observed().playlist.title, RENAMED);
    }
}

#[derive(Clone, Copy, Debug)]
enum Failure {
    LostReply,
    MalformedReply,
    CatalogUnavailable,
    Unconfirmed,
}

struct FailedChange {
    lan: SimLan,
    mode: Failure,
    writes: Cell<usize>,
}

impl Transport for FailedChange {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if matches!(self.mode, Failure::CatalogUnavailable)
            && self.writes.get() > 0
            && name == "Browse"
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "catalog read unavailable after mutation".into(),
            });
        }
        if !matches!(name, "UpdateObject" | "DestroyObject") {
            return self.lan.soap_post(host, path, action, body);
        }
        self.writes.set(self.writes.get() + 1);
        if matches!(self.mode, Failure::Unconfirmed) {
            return Ok(fsonos_proto::soap::envelope(
                &fsonos_proto::soap::CONTENT_DIRECTORY,
                &format!("{name}Response"),
                "",
            ));
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        match self.mode {
            Failure::LostReply => Err(ProtoError::Network {
                target: host.to_string(),
                detail: "native mutation applied but acknowledgement lost".into(),
            }),
            Failure::MalformedReply => Ok("not a SOAP acknowledgement".into()),
            _ => Ok(response),
        }
    }
}

#[test]
fn ambiguous_mutations_are_uncertain_and_never_replayed() {
    for generation in [1, 2] {
        for remove in [false, true] {
            for mode in [
                Failure::LostReply,
                Failure::MalformedReply,
                Failure::CatalogUnavailable,
                Failure::Unconfirmed,
            ] {
                let f = Fixture::new(generation);
                let observed = f.observed();
                let before = f.playback();
                let transport = FailedChange {
                    lan: f.sim.lan(),
                    mode,
                    writes: Cell::new(0),
                };
                let error = f
                    .change(&transport, remove, &observed.revision)
                    .unwrap_err();
                assert!(
                    matches!(error, CoreError::Uncertain(_)),
                    "{generation}/{remove}/{mode:?}: {error}"
                );
                assert!(
                    error
                        .to_string()
                        .contains("refresh the saved-playlist catalog")
                );
                assert_eq!(transport.writes.get(), 1);
                assert_eq!(f.playback(), before);
                let selected = playlists::list(&f.sim.lan(), &f.houses, &f.coordinator)
                    .unwrap()
                    .into_iter()
                    .find(|playlist| playlist.id == SELECTED);
                if matches!(mode, Failure::Unconfirmed) {
                    assert_eq!(selected, Some(observed.playlist));
                } else if remove {
                    assert!(selected.is_none());
                } else {
                    assert_eq!(selected.unwrap().title, RENAMED);
                }
            }
        }
    }
}

#[test]
fn an_explicit_native_refusal_keeps_its_error_and_does_not_retry() {
    for generation in [1, 2] {
        for remove in [false, true] {
            let f = Fixture::new(generation);
            let observed = f.observed();
            let before = f.playback();
            let action = if remove {
                "DestroyObject"
            } else {
                "UpdateObject"
            };
            f.sim.upnp_fault("Alpha", action, 800).unwrap();
            let start = f.sim.soap_log().len();
            let error = f
                .change(&f.sim.lan(), remove, &observed.revision)
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    CoreError::Proto(ProtoError::SoapFault { code: 800, .. })
                ),
                "{error}"
            );
            assert_eq!(f.writes_since(start), [action]);
            assert_eq!(f.observed(), observed);
            assert_eq!(f.playback(), before);
        }
    }
}

struct DriftBeforeWrite<'a> {
    f: &'a Fixture,
    reboot: bool,
    topology_reads: Cell<usize>,
    mutations: Cell<usize>,
}

impl Transport for DriftBeforeWrite<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if matches!(name, "UpdateObject" | "DestroyObject") {
            self.mutations.set(self.mutations.get() + 1);
        }
        let response = self.f.sim.lan().soap_post(host, path, action, body)?;
        if name == "GetZoneGroupState" {
            self.topology_reads.set(self.topology_reads.get() + 1);
            // Return the old final contents observation, then change the
            // player before the mutation's last ownership confirmation.
            if self.topology_reads.get() == 3 {
                if self.reboot {
                    self.f.sim.reboot("Alpha", Duration::ZERO).unwrap();
                } else {
                    soap::join_group(
                        &self.f.sim.lan(),
                        self.f.sim.player("Beta").unwrap().ip,
                        &self.f.coordinator,
                    )
                    .unwrap();
                }
            }
        }
        Ok(response)
    }
}

#[test]
fn a_group_or_boot_change_after_the_contents_read_prevents_catalog_writes() {
    for remove in [false, true] {
        for reboot in [false, true] {
            let f = Fixture::new(2);
            let observed = f.observed();
            let transport = DriftBeforeWrite {
                f: &f,
                reboot,
                topology_reads: Cell::new(0),
                mutations: Cell::new(0),
            };
            assert!(f.change(&transport, remove, &observed.revision).is_err());
            assert_eq!(transport.mutations.get(), 0);
            assert!(
                playlists::list(&f.sim.lan(), &f.houses, &f.coordinator)
                    .unwrap()
                    .contains(&observed.playlist)
            );
        }
    }
}
