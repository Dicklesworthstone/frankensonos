//! Cross-household queue transfer must not erase the destination to find out
//! whether its replacement can be loaded. Real localhost S1/S2 SOAP, with
//! narrowly injected lost replies and independent controller actions.

use fsonos_core::moving::{MoveError, MoveMethod, MoveReport, copy_playback};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;

const OLD: &str = "x-file-cifs://example.invalid/original.flac";
const FIRST: &str = "x-file-cifs://example.invalid/first.flac";
const LAST: &str = "x-file-cifs://example.invalid/last.flac";
const OTHER: &str = "x-file-cifs://example.invalid/owner.flac";
const STREAM: &str = "x-rincon-mp3radio://example.invalid/owner-stream";
const INCOMING: [&str; 3] = [FIRST, FIRST, LAST];

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    source_name: &'static str,
    target_name: &'static str,
    source: IpAddr,
    target: IpAddr,
}

impl Fixture {
    fn new(target_s2: bool) -> Self {
        let sim = SimHousehold::builder()
            .s1([SimPlayerSpec::new("S1", SimModel::Play5Gen1)])
            .s2([SimPlayerSpec::new("S2", SimModel::One)])
            .spawn()
            .unwrap();
        let (source_name, target_name) = if target_s2 {
            ("S1", "S2")
        } else {
            ("S2", "S1")
        };
        let source = sim.player(source_name).unwrap().ip;
        let target = sim.player(target_name).unwrap().ip;
        let lan = sim.lan();
        let houses = [source, target]
            .into_iter()
            .map(|host| {
                let mut house = HouseholdState::default();
                house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
                house
            })
            .collect();
        for (room, host, entries) in [
            (source_name, source, INCOMING.as_slice()),
            (target_name, target, [OLD, OLD].as_slice()),
        ] {
            for uri in entries {
                soap::enqueue_uri(&lan, host, uri, "", 0, false).unwrap();
            }
            let id = PlayerId(sim.player(room).unwrap().uuid.clone());
            soap::play_from_queue(&lan, host, &id).unwrap();
            soap::seek_track(&lan, host, 2).unwrap();
            soap::seek_position(&lan, host, 37).unwrap();
            soap::play(&lan, host).unwrap();
        }
        set_play_mode(&lan, source, PlayMode::RepeatOne).unwrap();
        Self {
            sim,
            houses,
            source_name,
            target_name,
            source,
            target,
        }
    }

    fn copy(&self, t: &impl Transport) -> Result<MoveReport, MoveError> {
        copy_playback(
            t,
            &self.houses,
            &resolve_room(&self.houses, self.source_name).unwrap(),
            &resolve_room(&self.houses, self.target_name).unwrap(),
        )
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.target, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn source_kept(&self, since: usize) {
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), self.source)
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert!(
            self.sim.soap_log()[since..]
                .iter()
                .all(|entry| entry.room != self.source_name || entry.action != "Stop")
        );
    }

    fn no_clear(&self, since: usize) {
        assert!(
            self.sim.soap_log()[since..]
                .iter()
                .all(|entry| entry.action != "RemoveAllTracksFromQueue")
        );
    }

    fn copied(&self) {
        assert_eq!(self.queue(), INCOMING);
        let lan = self.sim.lan();
        let position = soap::get_position_info(&lan, self.target).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(
            get_play_mode(&lan, self.target).unwrap(),
            PlayMode::RepeatOne
        );
        assert_eq!(
            soap::get_transport_info(&lan, self.target).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(
            soap::get_transport_info(&lan, self.source).unwrap().state,
            TransportState::Stopped
        );
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    RefuseFirst,
    RefuseSecond,
    LoseAppend,
    MalformAppend,
    OmitSecond,
    WrongCounters,
    ReplacePrefix,
    SeekAfterAppend,
    SourceAfterAppend,
    ModeAfterAppend,
    LoseStop,
    OmitStop,
    LoseRemoval,
    IgnoreRemoval,
    RaceRemoval,
    SeekAfterRemoval,
}

struct Replies {
    inner: SimLan,
    target: IpAddr,
    fault: Fault,
    adds: Cell<usize>,
    removes: Cell<usize>,
    stops: Cell<usize>,
    starts: Cell<usize>,
}

impl Replies {
    fn new(f: &Fixture, fault: Fault) -> Self {
        Self {
            inner: f.sim.lan(),
            target: f.target,
            fault,
            adds: Cell::new(0),
            removes: Cell::new(0),
            stops: Cell::new(0),
            starts: Cell::new(0),
        }
    }

    fn lost(&self) -> ProtoError {
        ProtoError::Network {
            target: self.target.to_string(),
            detail: "injected lost transfer reply".into(),
        }
    }

    fn append(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.adds.set(self.adds.get() + 1);
        let n = self.adds.get();
        if (n == 1 && matches!(self.fault, Fault::RefuseFirst))
            || (n == 2 && matches!(self.fault, Fault::RefuseSecond))
        {
            return Err(ProtoError::SoapFault {
                code: 800,
                reason: "injected refusal".into(),
            });
        }
        if n == 2 && matches!(self.fault, Fault::OmitSecond) {
            return Err(self.lost());
        }
        let result = self.inner.soap_post(host, path, action, body)?;
        if n == 2 {
            match self.fault {
                Fault::LoseAppend => return Err(self.lost()),
                Fault::MalformAppend => return Ok("<broken>".into()),
                Fault::WrongCounters => {
                    return Ok(result.replace(
                        "<NumTracksAdded>1</NumTracksAdded>",
                        "<NumTracksAdded>2</NumTracksAdded>",
                    ));
                }
                _ => {}
            }
        }
        if n == 3 {
            match self.fault {
                Fault::ReplacePrefix => {
                    soap::remove_track_range_from_queue(&self.inner, host, 0, 1, 1)?;
                    soap::enqueue_uri(&self.inner, host, OTHER, "", 1, false)?;
                }
                Fault::SeekAfterAppend => soap::seek_position(&self.inner, host, 99)?,
                Fault::SourceAfterAppend => {
                    soap::set_av_transport_uri(&self.inner, host, STREAM, "")?;
                }
                Fault::ModeAfterAppend => {
                    set_play_mode(&self.inner, host, PlayMode::Shuffle)?;
                }
                _ => {}
            }
        }
        Ok(result)
    }

    fn remove(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.removes.set(self.removes.get() + 1);
        if matches!(self.fault, Fault::IgnoreRemoval) {
            return Ok("<RemoveTrackRangeFromQueueResponse/>".into());
        }
        if matches!(self.fault, Fault::RaceRemoval) {
            soap::enqueue_uri(&self.inner, host, OTHER, "", 0, false)?;
        }
        let result = self.inner.soap_post(host, path, action, body)?;
        if matches!(self.fault, Fault::SeekAfterRemoval) {
            soap::seek_position(&self.inner, host, 99)?;
        }
        if matches!(self.fault, Fault::LoseRemoval) {
            Err(self.lost())
        } else {
            Ok(result)
        }
    }
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if host != self.target {
            return self.inner.soap_post(host, path, action, body);
        }
        if name == "Play" {
            self.starts.set(self.starts.get() + 1);
        }
        if name == "Stop" {
            self.stops.set(self.stops.get() + 1);
            if matches!(self.fault, Fault::OmitStop) {
                return Err(self.lost());
            }
            let result = self.inner.soap_post(host, path, action, body)?;
            return if matches!(self.fault, Fault::LoseStop) {
                Err(self.lost())
            } else {
                Ok(result)
            };
        }
        if name == "AddURIToQueue" {
            return self.append(host, path, action, body);
        }
        if name == "RemoveTrackRangeFromQueue" {
            return self.remove(host, path, action, body);
        }
        self.inner.soap_post(host, path, action, body)
    }
}

/// External deletion matches the helper read-back after our own mutation fence blocks.
struct BeforeRemovalFence {
    replies: Replies,
    staged: Cell<bool>,
    topology_reads: Cell<u8>,
    external_removal: Cell<bool>,
    recovery_read: Cell<bool>,
}

impl Transport for BeforeRemovalFence {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if host == self.replies.target && self.replies.adds.get() == INCOMING.len() {
            if name == "Browse"
                && body.contains("<StartingIndex>0</StartingIndex>")
                && body.contains("<RequestedCount>100</RequestedCount>")
            {
                if self.external_removal.get() {
                    self.recovery_read.set(true);
                } else {
                    self.staged.set(true);
                    self.topology_reads.set(0);
                }
            }
            if name == "GetZoneGroupState" && self.staged.get() && !self.external_removal.get() {
                let reads = self.topology_reads.get() + 1;
                self.topology_reads.set(reads);
                // Two Owner::confirm reads, then the removal helper preflight.
                if reads == 3 {
                    soap::remove_track_range_from_queue(&self.replies.inner, host, 0, 1, 2)?;
                    self.external_removal.set(true);
                }
            }
        }
        self.replies.soap_post(host, path, action, body)
    }
}

#[test]
fn transfer_verifies_the_entire_suffix_before_removing_the_original_prefix() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let t = Replies::new(&f, Fault::None);
        let since = f.sim.soap_log().len();
        assert_eq!(f.copy(&t).unwrap().method, MoveMethod::Copied);
        f.copied();
        f.no_clear(since);
        assert_eq!((t.adds.get(), t.removes.get(), t.starts.get()), (3, 1, 1));
        let log = f.sim.soap_log();
        let writes: Vec<_> = log[since..]
            .iter()
            .filter(|entry| {
                entry.room == f.target_name
                    && matches!(
                        entry.action.as_str(),
                        "AddURIToQueue" | "RemoveTrackRangeFromQueue"
                    )
            })
            .map(|entry| entry.action.as_str())
            .collect();
        assert_eq!(
            writes,
            [
                "AddURIToQueue",
                "AddURIToQueue",
                "AddURIToQueue",
                "RemoveTrackRangeFromQueue"
            ]
        );
    }
}

#[test]
fn an_external_deletion_cannot_be_adopted_after_our_removal_fence_blocked_the_write() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let t = BeforeRemovalFence {
            replies: Replies::new(&f, Fault::None),
            staged: Cell::new(false),
            topology_reads: Cell::new(0),
            external_removal: Cell::new(false),
            recovery_read: Cell::new(false),
        };
        let since = f.sim.soap_log().len();
        assert!(
            f.copy(&t).is_err(),
            "a blocked write cannot be recovered as ours"
        );
        assert!(
            t.external_removal.get(),
            "the external deletion was injected"
        );
        assert!(
            t.recovery_read.get(),
            "the removal helper read back the matching external deletion"
        );
        assert_eq!(f.queue(), INCOMING);
        assert_eq!(
            (
                t.replies.adds.get(),
                t.replies.removes.get(),
                t.replies.starts.get()
            ),
            (3, 0, 0),
            "our removal and playback writes were never sent"
        );
        let log = f.sim.soap_log();
        assert_eq!(
            log[since..]
                .iter()
                .filter(|entry| {
                    entry.room == f.target_name && entry.action == "RemoveTrackRangeFromQueue"
                })
                .count(),
            1,
            "only the independent controller removed the old prefix"
        );
        assert!(
            log[since..].iter().all(|entry| {
                entry.room != f.target_name || entry.action != "SetAVTransportURI"
            })
        );
        f.no_clear(since);
        f.source_kept(since);
    }
}

#[test]
fn refused_unapplied_or_wrongly_acknowledged_appends_preserve_the_old_queue() {
    for s2 in [false, true] {
        for (fault, appended) in [
            (Fault::RefuseFirst, 0),
            (Fault::RefuseSecond, 1),
            (Fault::OmitSecond, 1),
            (Fault::WrongCounters, 2),
        ] {
            let f = Fixture::new(s2);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            assert!(f.copy(&t).is_err());
            let expected: Vec<_> = [OLD, OLD]
                .into_iter()
                .chain(INCOMING[..appended].iter().copied())
                .collect();
            assert_eq!(f.queue(), expected);
            assert_eq!((t.removes.get(), t.starts.get()), (0, 0));
            f.no_clear(since);
            f.source_kept(since);
        }
    }
}

#[test]
fn lost_append_stop_and_removal_replies_are_proven_without_replaying_writes() {
    for s2 in [false, true] {
        for fault in [
            Fault::LoseAppend,
            Fault::MalformAppend,
            Fault::LoseStop,
            Fault::LoseRemoval,
        ] {
            let f = Fixture::new(s2);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            f.copy(&t).unwrap();
            f.copied();
            f.no_clear(since);
            assert_eq!((t.adds.get(), t.removes.get(), t.stops.get()), (3, 1, 1));
        }
    }
}

#[test]
fn an_unapplied_stop_cannot_append_to_playing_destination_music() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let t = Replies::new(&f, Fault::OmitStop);
        let since = f.sim.soap_log().len();
        assert!(f.copy(&t).is_err());
        assert_eq!(f.queue(), [OLD, OLD]);
        assert_eq!((t.adds.get(), t.removes.get(), t.starts.get()), (0, 0, 0));
        f.source_kept(since);
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.target)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn changing_the_prefix_even_at_the_same_length_prevents_its_deletion() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let t = Replies::new(&f, Fault::ReplacePrefix);
        let since = f.sim.soap_log().len();
        assert!(f.copy(&t).is_err());
        assert_eq!(f.queue(), [OTHER, OLD, FIRST, FIRST, LAST]);
        assert_eq!((t.removes.get(), t.starts.get()), (0, 0));
        f.no_clear(since);
        f.source_kept(since);
    }
}

#[test]
fn newer_destination_seek_source_or_mode_wins_over_staged_replacement() {
    for s2 in [false, true] {
        for fault in [
            Fault::SeekAfterAppend,
            Fault::SourceAfterAppend,
            Fault::ModeAfterAppend,
        ] {
            let f = Fixture::new(s2);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            assert!(f.copy(&t).is_err());
            assert_eq!(f.queue(), [OLD, OLD, FIRST, FIRST, LAST]);
            assert_eq!((t.removes.get(), t.starts.get()), (0, 0));
            f.source_kept(since);
            let lan = f.sim.lan();
            match fault {
                Fault::SeekAfterAppend => assert_eq!(
                    soap::get_position_info(&lan, f.target)
                        .unwrap()
                        .position_secs,
                    Some(99)
                ),
                Fault::SourceAfterAppend => {
                    assert_eq!(soap::get_media_info(&lan, f.target).unwrap().uri, STREAM);
                }
                Fault::ModeAfterAppend => {
                    assert_eq!(get_play_mode(&lan, f.target).unwrap(), PlayMode::Shuffle);
                }
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn ignored_removal_and_a_native_revision_race_do_not_start_or_release_the_source() {
    for s2 in [false, true] {
        for fault in [Fault::IgnoreRemoval, Fault::RaceRemoval] {
            let f = Fixture::new(s2);
            let t = Replies::new(&f, fault);
            let since = f.sim.soap_log().len();
            assert!(f.copy(&t).is_err());
            let mut expected = vec![OLD, OLD, FIRST, FIRST, LAST];
            if matches!(fault, Fault::RaceRemoval) {
                expected.push(OTHER);
            }
            assert_eq!(f.queue(), expected);
            assert_eq!((t.adds.get(), t.removes.get(), t.starts.get()), (3, 1, 0));
            f.no_clear(since);
            f.source_kept(since);
        }
    }
}

#[test]
fn a_seek_after_commit_is_not_overwritten_by_loading_the_queue() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let t = Replies::new(&f, Fault::SeekAfterRemoval);
        let since = f.sim.soap_log().len();
        assert!(f.copy(&t).is_err());
        assert_eq!(f.queue(), INCOMING);
        assert_eq!(t.starts.get(), 0);
        assert_eq!(
            soap::get_position_info(&f.sim.lan(), f.target)
                .unwrap()
                .position_secs,
            Some(99)
        );
        assert!(
            f.sim.soap_log()[since..].iter().all(|entry| {
                entry.room != f.target_name || entry.action != "SetAVTransportURI"
            })
        );
        f.source_kept(since);
    }
}

#[test]
fn a_paused_source_stays_silent_and_an_inactive_destination_queue_is_supported() {
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let lan = f.sim.lan();
        soap::pause(&lan, f.source).unwrap();
        soap::set_av_transport_uri(&lan, f.target, STREAM, "").unwrap();
        soap::play(&lan, f.target).unwrap();
        let t = Replies::new(&f, Fault::None);
        let since = f.sim.soap_log().len();
        f.copy(&t).unwrap();
        assert_eq!(f.queue(), INCOMING);
        assert_eq!(t.starts.get(), 0);
        let position = soap::get_position_info(&lan, f.target).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(
            soap::get_transport_info(&lan, f.target).unwrap().state,
            TransportState::Stopped
        );
        f.no_clear(since);
    }
}
