//! Staged source replacement over real S1/S2 sockets with an independent owner.

use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::SpotifyContainer;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const OLD: [&str; 2] = [
    "x-file-cifs://example.invalid/original-a.flac",
    "x-file-cifs://example.invalid/original-b.flac",
];
const SOURCE: &str = "x-file-cifs://example.invalid/owner-selection.flac";
const ALBUM: &str = "spotify:album:0000000000000000000123";

struct Rig {
    _sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
    peer: PlayerId,
    peer_host: IpAddr,
}

impl Rig {
    fn new(s2: bool, populated: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Target", model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Target").unwrap().ip;
        let id = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let peer_host = sim.player("Peer").unwrap().ip;
        let peer = PlayerId(sim.player("Peer").unwrap().uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        if populated {
            for uri in OLD {
                soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
            }
            control::play_queue_from(&lan, &houses, &id, 1).unwrap();
        }
        Self {
            _sim: sim,
            lan,
            houses,
            id,
            host,
            peer,
            peer_host,
        }
    }

    fn replace<T: Transport + ?Sized>(&self, t: &T, autoplay: bool) -> Result<(), CoreError> {
        if autoplay {
            control::play_spotify_container(
                t,
                &self.houses,
                &self.id,
                SpotifyContainer::Album,
                ALBUM,
                "Replacement",
            )
        } else {
            control::load_spotify_container(
                t,
                &self.houses,
                &self.id,
                SpotifyContainer::Album,
                ALBUM,
                "Replacement",
            )
        }
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.lan, self.host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|object| object.res.unwrap().uri)
            .collect()
    }

    fn change(&self, edit: Edit) -> Result<(), ProtoError> {
        match edit {
            Edit::Coordinator => soap::join_group(&self.lan, self.host, &self.peer),
            Edit::Membership => soap::join_group(&self.lan, self.peer_host, &self.id),
            Edit::Source => soap::set_av_transport_uri(&self.lan, self.host, SOURCE, ""),
            Edit::Metadata => {
                let uri = soap::get_media_info(&self.lan, self.host)?.uri;
                soap::set_av_transport_uri(
                    &self.lan,
                    self.host,
                    &uri,
                    "<DIDL-Lite><item><title>Owner choice</title></item></DIDL-Lite>",
                )
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Edit {
    Coordinator,
    Membership,
    Source,
    Metadata,
}

#[derive(Clone, Copy)]
enum Phase {
    InitialQueue,
    Added,
    StagedGuard,
    Removed,
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    LostRemoval,
    RefusedAfterOwnerRemoval,
    UnreadableAfterAdd,
    UnreadableAfterRemoval,
    MissingSource,
}

struct Replies<'a> {
    rig: &'a Rig,
    edit: Option<(Phase, Edit)>,
    fault: Fault,
    changed: Cell<bool>,
    added: Cell<bool>,
    removed: Cell<bool>,
    media_after_add: Cell<usize>,
    calls: RefCell<Vec<String>>,
}

impl<'a> Replies<'a> {
    fn new(rig: &'a Rig, edit: Option<(Phase, Edit)>, fault: Fault) -> Self {
        Self {
            rig,
            edit,
            fault,
            changed: Cell::new(false),
            added: Cell::new(false),
            removed: Cell::new(false),
            media_after_add: Cell::new(0),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn count(&self, name: &str) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|action| action.as_str() == name)
            .count()
    }

    fn no_selection(&self) {
        for action in [
            "SetAVTransportURI",
            "Seek",
            "Play",
            "RemoveAllTracksFromQueue",
        ] {
            assert_eq!(self.count(action), 0, "unexpected {action}");
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected missing replacement reply".into(),
        }
    }
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if name == "GetMediaInfo"
            && ((self.added.get() && matches!(self.fault, Fault::UnreadableAfterAdd))
                || (self.removed.get() && matches!(self.fault, Fault::UnreadableAfterRemoval)))
        {
            return Err(Self::lost(host));
        }
        if name == "RemoveTrackRangeFromQueue"
            && matches!(self.fault, Fault::RefusedAfterOwnerRemoval)
        {
            // The owner, not this refused request, made the matching deletion.
            soap::remove_track_range_from_queue(&self.rig.lan, host, 0, 1, 2)?;
            return Err(ProtoError::SoapFault {
                code: 800,
                reason: "explicitly refused replacement removal".into(),
            });
        }
        let response = self.rig.lan.soap_post(host, path, action, body)?;
        if name == "AddURIToQueue" {
            self.added.set(true);
        }
        if name == "RemoveTrackRangeFromQueue" {
            self.removed.set(true);
        }
        if name == "GetMediaInfo" && self.added.get() {
            self.media_after_add.set(self.media_after_add.get() + 1);
        }
        if let Some((phase, edit)) = self.edit {
            let due = match phase {
                Phase::InitialQueue => {
                    name == "Browse" && body.contains("<ObjectID>Q:0</ObjectID>")
                }
                Phase::Added => name == "AddURIToQueue",
                Phase::StagedGuard => name == "GetMediaInfo" && self.media_after_add.get() == 1,
                Phase::Removed => name == "RemoveTrackRangeFromQueue",
            };
            if due && !self.changed.replace(true) {
                self.rig.change(edit)?;
            }
        }
        if name == "RemoveTrackRangeFromQueue" && matches!(self.fault, Fault::LostRemoval) {
            return Err(Self::lost(host));
        }
        if name == "GetMediaInfo" && matches!(self.fault, Fault::MissingSource) {
            return Ok(response.replace("CurrentURI", "AbsentURI"));
        }
        Ok(response)
    }
}

#[test]
fn stale_coordinators_cannot_stage_or_detach_themselves_to_play_a_replacement() {
    for s2 in [false, true] {
        for autoplay in [false, true] {
            let rig = Rig::new(s2, true);
            rig.change(Edit::Coordinator).unwrap();
            let t = Replies::new(&rig, None, Fault::None);
            assert!(rig.replace(&t, autoplay).is_err());
            assert_eq!(t.count("AddURIToQueue"), 0);
            assert_eq!(t.count("Pause"), 0);
            t.no_selection();
        }
    }
}

#[test]
fn source_and_group_changes_during_preflight_prevent_the_append() {
    for s2 in [false, true] {
        for autoplay in [false, true] {
            for edit in [
                Edit::Coordinator,
                Edit::Membership,
                Edit::Source,
                Edit::Metadata,
            ] {
                let rig = Rig::new(s2, true);
                let t = Replies::new(&rig, Some((Phase::InitialQueue, edit)), Fault::None);
                assert!(rig.replace(&t, autoplay).is_err(), "{edit:?}");
                assert!(t.changed.get());
                assert_eq!(t.count("AddURIToQueue"), 0);
                assert_eq!(t.count("Pause"), 0, "do not pause the owner's new source");
                if !matches!(edit, Edit::Coordinator) {
                    assert_eq!(rig.queue(), OLD);
                }
                t.no_selection();
            }
        }
    }
}

#[test]
fn a_new_source_after_staging_preserves_the_original_prefix_and_owner_selection() {
    for s2 in [false, true] {
        for autoplay in [false, true] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, Some((Phase::Added, Edit::Source)), Fault::None);
            assert!(rig.replace(&t, autoplay).is_err());
            assert_eq!(t.count("AddURIToQueue"), 1);
            assert_eq!(t.count("RemoveTrackRangeFromQueue"), 0);
            assert!(rig.queue().starts_with(&OLD.map(str::to_string)));
            assert_eq!(
                soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
                SOURCE
            );
            t.no_selection();
        }
    }
}

#[test]
fn membership_change_with_the_same_coordinator_is_not_the_original_destination() {
    for s2 in [false, true] {
        let rig = Rig::new(s2, true);
        let t = Replies::new(&rig, Some((Phase::Added, Edit::Membership)), Fault::None);
        assert!(rig.replace(&t, true).is_err());
        assert!(t.changed.get());
        assert_eq!(t.count("RemoveTrackRangeFromQueue"), 0);
        assert!(rig.queue().starts_with(&OLD.map(str::to_string)));
        t.no_selection();
    }
}

#[test]
fn acknowledged_and_lost_removal_replies_cannot_restart_a_handed_off_group() {
    for s2 in [false, true] {
        for fault in [Fault::None, Fault::LostRemoval] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, Some((Phase::Removed, Edit::Coordinator)), fault);
            assert!(rig.replace(&t, true).is_err());
            assert!(t.changed.get());
            assert_eq!(t.count("AddURIToQueue"), 1);
            assert_eq!(t.count("RemoveTrackRangeFromQueue"), 1);
            t.no_selection();
        }
    }
}

#[test]
fn final_selection_rechecks_membership_source_and_metadata_after_removal() {
    for s2 in [false, true] {
        for edit in [Edit::Membership, Edit::Source, Edit::Metadata] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, Some((Phase::Removed, edit)), Fault::None);
            assert!(rig.replace(&t, true).is_err(), "{edit:?}");
            assert!(t.changed.get());
            assert_eq!(t.count("RemoveTrackRangeFromQueue"), 1);
            t.no_selection();
        }
    }
}

#[test]
fn an_initially_empty_queue_does_not_skip_the_final_destination_check() {
    for s2 in [false, true] {
        let rig = Rig::new(s2, false);
        let t = Replies::new(&rig, Some((Phase::StagedGuard, Edit::Source)), Fault::None);
        assert!(rig.replace(&t, true).is_err());
        assert!(t.changed.get());
        assert_eq!(t.count("AddURIToQueue"), 1);
        assert_eq!(t.count("RemoveTrackRangeFromQueue"), 0);
        t.no_selection();
    }
}

#[test]
fn a_definite_removal_refusal_is_not_overridden_by_an_owners_matching_deletion() {
    for s2 in [false, true] {
        let rig = Rig::new(s2, true);
        let t = Replies::new(&rig, None, Fault::RefusedAfterOwnerRemoval);
        let error = rig.replace(&t, true).unwrap_err();
        assert!(matches!(error, CoreError::Proto(ProtoError::Malformed(_))));
        assert_eq!(t.count("AddURIToQueue"), 1, "no render-parameter retry");
        assert_eq!(t.count("RemoveTrackRangeFromQueue"), 1);
        assert!(!rig.queue().is_empty());
        t.no_selection();
    }
}

#[test]
fn unavailable_destination_after_append_or_removal_never_authorizes_selection() {
    for s2 in [false, true] {
        for fault in [Fault::UnreadableAfterAdd, Fault::UnreadableAfterRemoval] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, None, fault);
            assert!(rig.replace(&t, true).is_err());
            assert_eq!(t.count("AddURIToQueue"), 1);
            t.no_selection();
        }
    }
}

#[test]
fn missing_source_identity_is_not_an_idle_empty_source() {
    for s2 in [false, true] {
        let rig = Rig::new(s2, false);
        let t = Replies::new(&rig, None, Fault::MissingSource);
        assert!(rig.replace(&t, true).is_err());
        assert_eq!(t.count("AddURIToQueue"), 0);
        t.no_selection();
    }
}

#[test]
fn a_confirmed_lost_removal_completes_playback_or_silent_preparation_once() {
    for s2 in [false, true] {
        for autoplay in [false, true] {
            let rig = Rig::new(s2, true);
            let t = Replies::new(&rig, None, Fault::LostRemoval);
            rig.replace(&t, autoplay).unwrap();
            assert_eq!(t.count("AddURIToQueue"), 1);
            assert_eq!(t.count("RemoveTrackRangeFromQueue"), 1);
            assert_eq!(t.count("SetAVTransportURI"), 1);
            assert_eq!(t.count("Seek"), 1);
            assert_eq!(t.count("Play"), usize::from(autoplay));
            let now = soap::get_position_info(&rig.lan, rig.host).unwrap();
            assert_eq!(now.track, 1);
            assert_eq!(now.uri, rig.queue()[0]);
            let state = soap::get_transport_info(&rig.lan, rig.host).unwrap().state;
            if autoplay {
                assert_eq!(state, TransportState::Playing);
            } else {
                assert!(matches!(
                    state,
                    TransportState::Paused | TransportState::Stopped
                ));
            }
        }
    }
}

#[test]
fn playlist_replacement_obeys_the_same_staging_destination_boundary() {
    for s2 in [false, true] {
        let rig = Rig::new(s2, true);
        let t = Replies::new(&rig, Some((Phase::Added, Edit::Membership)), Fault::None);
        let result = control::play_spotify_container(
            &t,
            &rig.houses,
            &rig.id,
            SpotifyContainer::Playlist,
            "spotify:playlist:0000000000000000000456",
            "Replacement playlist",
        );
        assert!(result.is_err());
        assert!(t.changed.get());
        assert_eq!(t.count("AddURIToQueue"), 1);
        assert_eq!(t.count("RemoveTrackRangeFromQueue"), 0);
        assert!(rig.queue().starts_with(&OLD.map(str::to_string)));
        t.no_selection();
    }
}
