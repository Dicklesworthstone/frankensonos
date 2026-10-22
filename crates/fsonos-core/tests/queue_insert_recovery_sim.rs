//! Public enqueue recovery over real localhost S1/S2 SOAP exchanges.
//! Only reply faults and independent controller edits are injected.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const TRACK: &str = "spotify:track:EnqueueRecoveryTest";
const OWNER: &str = "http://example.invalid/owner.mp3";
const ORIGINAL: [&str; 3] = [
    "http://example.invalid/a.mp3",
    "http://example.invalid/b.mp3",
    "http://example.invalid/c.mp3",
];

struct Rig {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    host: IpAddr,
    id: PlayerId,
    peer: PlayerId,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Alpha").unwrap().ip;
        let id = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        let peer = PlayerId(sim.player("Beta").unwrap().uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        for uri in ORIGINAL {
            soap::enqueue_uri(&lan, host, uri, "", 0, false).unwrap();
        }
        control::play_queue_from(&lan, &houses, &id, 1).unwrap();
        Self {
            sim,
            houses,
            host,
            id,
            peer,
        }
    }

    fn enqueue(
        &self,
        t: &impl Transport,
        uri: &str,
        next: bool,
    ) -> Result<control::Queued, CoreError> {
        control::enqueue_spotify(t, &self.houses, &self.id, uri, "Recovery test", next)
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|object| {
                let uri = object.res.unwrap().uri;
                spotify_uri_from_renderer_uri(&uri).unwrap_or(uri)
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Accepted,
    Lost,
    Malformed,
    WrongCounters,
    NotApplied,
    Refused,
    RefusedOnce,
    AppliedThenRefused,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    None,
    Prefix,
    InsertedTrack,
    Source,
    Coordinator,
    ReadFailure,
    AfterSnapshot,
    AdvanceBeforeWrite,
}

struct Intercept<'a> {
    rig: &'a Rig,
    inner: SimLan,
    reply: Reply,
    edit: Edit,
    edited: Cell<bool>,
    adds: Cell<usize>,
    calls: RefCell<Vec<String>>,
}

impl<'a> Intercept<'a> {
    fn new(rig: &'a Rig, reply: Reply, edit: Edit) -> Self {
        Self {
            rig,
            inner: rig.sim.lan(),
            reply,
            edit,
            edited: Cell::new(false),
            adds: Cell::new(0),
            calls: RefCell::default(),
        }
    }

    fn failure(&self) -> ProtoError {
        ProtoError::Network {
            target: self.rig.host.to_string(),
            detail: "injected missing enqueue reply".into(),
        }
    }

    fn refusal() -> ProtoError {
        ProtoError::SoapFault {
            code: 800,
            reason: "injected source refusal".into(),
        }
    }

    fn replace(&self, first: u32) -> Result<(), ProtoError> {
        soap::remove_track_range_from_queue(&self.inner, self.rig.host, 0, first, 1)?;
        soap::enqueue_uri(&self.inner, self.rig.host, OWNER, "", first, false)?;
        Ok(())
    }

    fn only_add_writes(&self, expected: usize) {
        let writes: Vec<_> = self
            .calls
            .borrow()
            .iter()
            .filter(|name| !name.starts_with("Get") && name.as_str() != "Browse")
            .cloned()
            .collect();
        assert_eq!(writes, vec!["AddURIToQueue"; expected]);
    }
}

impl Transport for Intercept<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if name == "Browse" && self.adds.get() > 0 && self.edit == Edit::ReadFailure {
            return Err(self.failure());
        }
        if name == "AddURIToQueue" {
            self.adds.set(self.adds.get() + 1);
            if matches!(self.reply, Reply::Refused)
                || (matches!(self.reply, Reply::RefusedOnce) && self.adds.get() == 1)
            {
                return Err(Self::refusal());
            }
            if matches!(self.reply, Reply::NotApplied) {
                return Err(self.failure());
            }
        }
        let answer = self.inner.soap_post(host, path, action, body)?;
        if name == "GetPositionInfo"
            && self.adds.get() == 0
            && self.edit == Edit::AdvanceBeforeWrite
            && !self.edited.replace(true)
        {
            soap::seek_track(&self.inner, host, 2)?;
        }
        if name == "Browse"
            && self.adds.get() > 0
            && self.edit == Edit::AfterSnapshot
            && !self.edited.replace(true)
        {
            self.replace(4)?;
        }
        if name != "AddURIToQueue" {
            return Ok(answer);
        }
        match self.edit {
            Edit::Prefix => self.replace(1)?,
            Edit::InsertedTrack => self.replace(4)?,
            Edit::Source => soap::set_av_transport_uri(&self.inner, host, OWNER, "")?,
            Edit::Coordinator => soap::join_group(&self.inner, host, &self.rig.peer)?,
            _ => {}
        }
        match self.reply {
            Reply::Lost => Err(self.failure()),
            Reply::Malformed => Ok("<incomplete-enqueue-reply>".into()),
            Reply::WrongCounters => Ok(answer.replace(
                "<NumTracksAdded>1</NumTracksAdded>",
                "<NumTracksAdded>0</NumTracksAdded>",
            )),
            Reply::AppliedThenRefused => Err(Self::refusal()),
            _ => Ok(answer),
        }
    }
}

#[test]
fn lost_track_replies_recover_append_and_next_without_repeating_writes() {
    for s2 in [false, true] {
        for next in [false, true] {
            for reply in [Reply::Accepted, Reply::Lost, Reply::Malformed] {
                let rig = Rig::new(s2);
                let t = Intercept::new(&rig, reply, Edit::None);
                let result = rig.enqueue(&t, TRACK, next).unwrap();
                let first = if next { 2 } else { 4 };
                assert_eq!(result.first, first);
                assert!(result.playing_queue);
                assert_eq!(result.uris.len(), 1);
                assert_eq!(
                    spotify_uri_from_renderer_uri(&result.uris[0]).as_deref(),
                    Some(TRACK)
                );
                let mut expected: Vec<String> = ORIGINAL.iter().map(|uri| (*uri).into()).collect();
                expected.insert(usize::try_from(first - 1).unwrap(), TRACK.into());
                assert_eq!(rig.queue(), expected);
                t.only_add_writes(1);
                assert_eq!(
                    soap::get_position_info(&rig.sim.lan(), rig.host)
                        .unwrap()
                        .track,
                    1
                );
                // The recovered receipt remains usable by the shared undo path.
                assert!(
                    control::unqueue(
                        &rig.sim.lan(),
                        &rig.houses,
                        &rig.id,
                        result.first,
                        &result.uris
                    )
                    .unwrap()
                );
                assert_eq!(rig.queue(), ORIGINAL);
            }
        }
    }
}

#[test]
fn an_existing_duplicate_is_not_a_successful_new_enqueue() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.enqueue(&rig.sim.lan(), TRACK, false).unwrap();
        let before = rig.queue();
        let t = Intercept::new(&rig, Reply::NotApplied, Edit::None);
        let error = rig.enqueue(&t, TRACK, false).unwrap_err();
        assert!(error.to_string().contains("uncertain"), "{error}");
        assert_eq!(rig.queue(), before);
        t.only_add_writes(1);
    }
}

#[test]
fn acknowledged_replies_cannot_hide_a_replacement_track_or_neighbor() {
    for s2 in [false, true] {
        for edit in [Edit::Prefix, Edit::InsertedTrack, Edit::AfterSnapshot] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, Reply::Accepted, edit);
            let error = rig.enqueue(&t, TRACK, false).unwrap_err();
            assert!(error.to_string().contains("uncertain"), "{error}");
            t.only_add_writes(1);
            assert!(rig.queue().iter().any(|uri| uri == OWNER));
        }
    }
}

#[test]
fn missing_readback_and_changed_source_or_group_do_not_replay_an_add() {
    for s2 in [false, true] {
        for edit in [Edit::ReadFailure, Edit::Source, Edit::Coordinator] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, Reply::Lost, edit);
            let error = rig.enqueue(&t, TRACK, false).unwrap_err();
            assert!(error.to_string().contains("uncertain"), "{error}");
            t.only_add_writes(1);
        }
    }
}

#[test]
fn source_relearning_requires_an_unchanged_explicit_refusal() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::RefusedOnce, Edit::None);
        rig.enqueue(&t, TRACK, false).unwrap();
        t.only_add_writes(2);
        assert_eq!(
            rig.queue()
                .iter()
                .filter(|uri| uri.as_str() == TRACK)
                .count(),
            1
        );

        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::Refused, Edit::None);
        assert!(matches!(
            rig.enqueue(&t, TRACK, false),
            Err(CoreError::RenderParamsStale)
        ));
        t.only_add_writes(2);
        assert_eq!(rig.queue(), ORIGINAL);

        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::AppliedThenRefused, Edit::None);
        let error = rig.enqueue(&t, TRACK, false).unwrap_err();
        assert!(error.to_string().contains("uncertain"), "{error}");
        t.only_add_writes(1);
        assert_eq!(
            rig.queue()
                .iter()
                .filter(|uri| uri.as_str() == TRACK)
                .count(),
            1
        );
    }
}

#[test]
fn unknown_container_expansions_require_an_acknowledged_range() {
    for s2 in [false, true] {
        for uri in [
            "spotify:album:RecoveryAlbum",
            "spotify:playlist:RecoveryList",
        ] {
            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, Reply::Accepted, Edit::None);
            let result = rig.enqueue(&t, uri, false).unwrap();
            assert_eq!(result.first, 4);
            assert!(!result.uris.is_empty());
            assert_eq!(rig.queue().len(), 3 + result.uris.len());
            t.only_add_writes(1);

            let rig = Rig::new(s2);
            let t = Intercept::new(&rig, Reply::Lost, Edit::None);
            assert!(
                rig.enqueue(&t, uri, false)
                    .unwrap_err()
                    .to_string()
                    .contains("uncertain")
            );
            t.only_add_writes(1);
            assert!(rig.queue().len() > 3);
        }
    }
}

#[test]
fn next_on_an_external_source_appends_without_starting_the_queue() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        soap::set_av_transport_uri(&rig.sim.lan(), rig.host, OWNER, "").unwrap();
        let t = Intercept::new(&rig, Reply::Lost, Edit::None);
        let result = rig.enqueue(&t, TRACK, true).unwrap();
        assert_eq!(result.first, 4);
        assert!(!result.playing_queue);
        assert_eq!(
            soap::get_media_info(&rig.sim.lan(), rig.host).unwrap().uri,
            OWNER
        );
        t.only_add_writes(1);
    }
}

#[test]
fn stale_coordinators_and_changed_current_tracks_fail_before_enqueue() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::Accepted, Edit::AdvanceBeforeWrite);
        assert!(rig.enqueue(&t, TRACK, true).is_err());
        t.only_add_writes(0);
        assert_eq!(rig.queue(), ORIGINAL);

        let rig = Rig::new(s2);
        soap::join_group(&rig.sim.lan(), rig.host, &rig.peer).unwrap();
        let t = Intercept::new(&rig, Reply::Accepted, Edit::None);
        assert!(rig.enqueue(&t, TRACK, false).is_err());
        t.only_add_writes(0);
    }
}

#[test]
fn invalid_acknowledgement_counters_do_not_create_an_undo_receipt() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Intercept::new(&rig, Reply::WrongCounters, Edit::None);
        assert!(
            rig.enqueue(&t, TRACK, false)
                .unwrap_err()
                .to_string()
                .contains("uncertain")
        );
        t.only_add_writes(1);
        assert_eq!(rig.queue().last().map(String::as_str), Some(TRACK));
    }
}

#[test]
fn recovery_preserves_a_queue_spanning_multiple_browse_pages() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        for index in 3..103 {
            let uri = format!("http://example.invalid/queue/{index}.mp3");
            soap::enqueue_uri(&rig.sim.lan(), rig.host, &uri, "", 0, false).unwrap();
        }
        let before = rig.queue();
        let t = Intercept::new(&rig, Reply::Lost, Edit::None);
        let result = rig.enqueue(&t, TRACK, false).unwrap();
        assert_eq!(result.first, 104);
        assert_eq!(&rig.queue()[..103], before);
        t.only_add_writes(1);
    }
}

#[test]
fn an_empty_active_queue_accepts_append_and_next_with_a_lost_reply() {
    for s2 in [false, true] {
        for next in [false, true] {
            let rig = Rig::new(s2);
            soap::remove_all_tracks_from_queue(&rig.sim.lan(), rig.host).unwrap();
            let t = Intercept::new(&rig, Reply::Lost, Edit::None);
            let result = rig.enqueue(&t, TRACK, next).unwrap();
            assert_eq!(result.first, 1);
            assert!(result.playing_queue);
            assert_eq!(rig.queue(), [TRACK]);
            t.only_add_writes(1);
        }
    }
}

#[test]
fn failed_preflight_and_unidentifiable_next_tracks_send_no_enqueue() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.sim.upnp_fault("Alpha", "Browse", 701).unwrap();
        let t = Intercept::new(&rig, Reply::Accepted, Edit::None);
        assert!(rig.enqueue(&t, TRACK, false).is_err());
        t.only_add_writes(0);
        rig.sim.clear_faults("Alpha").unwrap();
        rig.sim.upnp_fault("Alpha", "GetPositionInfo", 701).unwrap();
        let t = Intercept::new(&rig, Reply::Accepted, Edit::None);
        assert!(rig.enqueue(&t, TRACK, true).is_err());
        t.only_add_writes(0);
        rig.sim.clear_faults("Alpha").unwrap();
        assert_eq!(rig.queue(), ORIGINAL);
    }
}
