//! Native and Spotify enqueue retain an admitted destination through their
//! internal reads. Real S1/S2 sockets exercise drift, retries and lost replies.

use fsonos_core::control::{Queued, TrackBatchDestination};
use fsonos_core::favorites::{self, Favorite, FavoriteKind};
use fsonos_core::{CoreError, HouseholdState, control, favorite_queue, playlists};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

const ORIGINAL: [&str; 3] = [
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/last.flac",
];
const EXTERNAL: &str = "x-file-cifs://example.invalid/another-controller.flac";
const TRACK: &str = "spotify:track:1000000000000000000001";
const ALBUM: &str = "spotify:album:2000000000000000000001";

struct Rig {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
    peer: IpAddr,
}

impl Rig {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Lead", model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Lead").unwrap().ip;
        let peer = sim.player("Peer").unwrap().ip;
        let coordinator = PlayerId(sim.player("Lead").unwrap().uuid.clone());
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        let households = vec![household];
        for uri in ORIGINAL {
            soap::add_uri_to_queue(&sim.lan(), host, uri, "", false).unwrap();
        }
        control::play_queue_from(&sim.lan(), &households, &coordinator, 1).unwrap();
        soap::seek_position(&sim.lan(), host, 17).unwrap();
        soap::pause(&sim.lan(), host).unwrap();
        soap::set_volume(&sim.lan(), host, 63).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::ShuffleRepeatOne).unwrap();
        Self {
            sim,
            households,
            coordinator,
            host,
            peer,
        }
    }

    fn destination(&self) -> TrackBatchDestination {
        TrackBatchDestination::read(&self.sim.lan(), self.host, &self.coordinator).unwrap()
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), self.host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|object| object.res.unwrap().uri)
            .collect()
    }

    fn native(&self, playlist: bool) -> Favorite {
        let listed = if playlist {
            playlists::list(&self.sim.lan(), &self.households, &self.coordinator)
        } else {
            favorites::list(&self.sim.lan(), &self.households, &self.coordinator)
        }
        .unwrap();
        listed
            .into_iter()
            .find(|favorite| {
                if playlist {
                    favorite.id == "SQ:7"
                } else {
                    favorite.kind == FavoriteKind::Track
                }
            })
            .unwrap()
    }
}

enum Source {
    Spotify(&'static str),
    Native(Favorite),
}

impl Source {
    fn enqueue(
        &self,
        t: &impl Transport,
        rig: &Rig,
        next: bool,
        destination: &TrackBatchDestination,
    ) -> Result<Queued, CoreError> {
        match self {
            Self::Spotify(uri) => control::enqueue_spotify_bound(
                t,
                &rig.households,
                &rig.coordinator,
                uri,
                "Selected source",
                next,
                destination,
            ),
            Self::Native(favorite) => favorite_queue::enqueue_bound(
                t,
                &rig.households,
                &rig.coordinator,
                favorite,
                next,
                destination,
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum Change {
    Queue,
    Source,
    Group,
    Boot,
    Advance,
}

#[derive(Clone, Copy)]
enum Stage {
    Learning,
    QueueRead,
    Relearning,
    FinalHead(u32),
}

#[derive(Clone, Copy)]
enum Reply {
    Ordinary,
    Lost,
    RefuseOnce,
}

#[derive(Clone, Copy)]
enum BadPosition {
    Zero,
    WrongUri,
}

struct Wire<'a> {
    rig: &'a Rig,
    lan: SimLan,
    stage: Option<Stage>,
    change: Change,
    reply: Reply,
    bad_position: Option<BadPosition>,
    edited: Cell<bool>,
    favorites: Cell<u32>,
    positions: Cell<u32>,
    adds: Cell<u32>,
    writes: RefCell<Vec<String>>,
}

impl<'a> Wire<'a> {
    fn new(rig: &'a Rig) -> Self {
        Self {
            rig,
            lan: rig.sim.lan(),
            stage: None,
            change: Change::Queue,
            reply: Reply::Ordinary,
            bad_position: None,
            edited: Cell::new(false),
            favorites: Cell::new(0),
            positions: Cell::new(0),
            adds: Cell::new(0),
            writes: RefCell::new(Vec::new()),
        }
    }

    fn edit(&self) -> Result<(), ProtoError> {
        match self.change {
            Change::Queue => {
                soap::add_uri_to_queue(&self.lan, self.rig.host, EXTERNAL, "", false)?;
            }
            Change::Source => soap::set_av_transport_uri(&self.lan, self.rig.host, EXTERNAL, "")?,
            Change::Group => soap::join_group(&self.lan, self.rig.peer, &self.rig.coordinator)?,
            Change::Boot => self.rig.sim.reboot("Lead", Duration::ZERO).unwrap(),
            Change::Advance => soap::seek_track(&self.lan, self.rig.host, 2)?,
        }
        Ok(())
    }

    fn only_adds(&self, count: usize) {
        assert_eq!(*self.writes.borrow(), vec!["AddURIToQueue"; count]);
    }
}

impl Transport for Wire<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.borrow_mut().push(name.into());
        }
        if name == "AddURIToQueue" {
            self.adds.set(self.adds.get() + 1);
            if matches!(self.reply, Reply::RefuseOnce) && self.adds.get() == 1 {
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "injected renderer refusal".into(),
                });
            }
        }
        let mut response = self.lan.soap_post(host, path, action, body)?;
        if name == "GetPositionInfo" {
            self.positions.set(self.positions.get() + 1);
            if let Some(bad_position) = self.bad_position {
                let altered = match bad_position {
                    BadPosition::Zero => response.replace("<Track>1</Track>", "<Track>0</Track>"),
                    BadPosition::WrongUri => response.replace(
                        &format!("<TrackURI>{}</TrackURI>", ORIGINAL[0]),
                        &format!("<TrackURI>{EXTERNAL}</TrackURI>"),
                    ),
                };
                assert_ne!(altered, response, "the position fault must be injected");
                response = altered;
            }
        }
        let favorite_read = name == "Browse" && body.contains("<ObjectID>FV:2</ObjectID>");
        if favorite_read {
            self.favorites.set(self.favorites.get() + 1);
        }
        let queue_read = name == "Browse" && body.contains("<ObjectID>Q:0</ObjectID>");
        let head = body.contains("<RequestedCount>1</RequestedCount>");
        let changing = match self.stage {
            Some(Stage::Learning) => favorite_read,
            Some(Stage::Relearning) => favorite_read && self.favorites.get() == 2,
            Some(Stage::QueueRead) => queue_read && !head,
            Some(Stage::FinalHead(positions)) => {
                queue_read && head && self.positions.get() == positions
            }
            None => false,
        };
        if changing && !self.edited.replace(true) {
            self.edit()?;
        }
        if name == "AddURIToQueue" && matches!(self.reply, Reply::Lost) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost enqueue reply".into(),
            });
        }
        Ok(response)
    }
}

#[test]
fn bound_spotify_and_native_sources_insert_next_or_append_without_changing_playback() {
    for generation in [1, 2] {
        for kind in 0..4 {
            for next in [false, true] {
                let rig = Rig::new(generation);
                let source = match kind {
                    0 => Source::Spotify(TRACK),
                    1 => Source::Spotify(ALBUM),
                    2 => Source::Native(rig.native(false)),
                    _ => Source::Native(rig.native(true)),
                };
                let destination = rig.destination();
                let before =
                    control::playback(&rig.sim.lan(), &rig.households, &rig.coordinator).unwrap();
                let wire = Wire::new(&rig);
                let added = source.enqueue(&wire, &rig, next, &destination).unwrap();
                assert_eq!(added.first, if next { 2 } else { 4 });
                assert!(added.playing_queue && added.proof.is_some());
                let mut expected: Vec<String> = ORIGINAL.iter().map(|uri| (*uri).into()).collect();
                drop(expected.splice(if next { 1..1 } else { 3..3 }, added.uris));
                assert_eq!(rig.queue(), expected);
                assert_eq!(
                    control::playback(&rig.sim.lan(), &rig.households, &rig.coordinator).unwrap(),
                    before
                );
                assert_eq!(soap::get_volume(&rig.sim.lan(), rig.host).unwrap(), 63);
                assert_eq!(
                    get_play_mode(&rig.sim.lan(), rig.host).unwrap(),
                    PlayMode::ShuffleRepeatOne
                );
                wire.only_adds(1);
            }
        }
    }
}

#[test]
fn a_destination_changed_before_entry_is_not_replaced_with_a_new_baseline() {
    for generation in [1, 2] {
        for change in [Change::Queue, Change::Source, Change::Group, Change::Boot] {
            for native in [false, true] {
                let rig = Rig::new(generation);
                let source = if native {
                    Source::Native(rig.native(false))
                } else {
                    Source::Spotify(TRACK)
                };
                let destination = rig.destination();
                let mut wire = Wire::new(&rig);
                wire.change = change;
                wire.edit().unwrap();
                let error = source
                    .enqueue(&wire, &rig, false, &destination)
                    .unwrap_err();
                assert!(!matches!(error, CoreError::Uncertain(_)));
                wire.only_adds(0);
            }
        }
    }
}

#[test]
fn source_learning_and_internal_queue_reads_cannot_replace_an_admitted_destination() {
    for generation in [1, 2] {
        for change in [Change::Queue, Change::Source, Change::Group] {
            for stage in [Stage::Learning, Stage::QueueRead] {
                for native in [false, true] {
                    if native && matches!(stage, Stage::Learning) {
                        continue;
                    }
                    let rig = Rig::new(generation);
                    let source = if native {
                        Source::Native(rig.native(true))
                    } else {
                        Source::Spotify(ALBUM)
                    };
                    let destination = rig.destination();
                    let mut wire = Wire::new(&rig);
                    wire.stage = Some(stage);
                    wire.change = change;
                    let error = source.enqueue(&wire, &rig, true, &destination).unwrap_err();
                    assert!(wire.edited.get());
                    assert!(!matches!(error, CoreError::Uncertain(_)));
                    wire.only_adds(0);
                }
            }
        }
    }
}

#[test]
fn an_explicit_renderer_retry_keeps_the_original_source_group_and_queue() {
    for generation in [1, 2] {
        for change in [
            None,
            Some(Change::Queue),
            Some(Change::Source),
            Some(Change::Group),
        ] {
            let rig = Rig::new(generation);
            let destination = rig.destination();
            let mut wire = Wire::new(&rig);
            wire.reply = Reply::RefuseOnce;
            if let Some(change) = change {
                wire.change = change;
                wire.stage = Some(Stage::Relearning);
            }
            let result = Source::Spotify(TRACK).enqueue(&wire, &rig, false, &destination);
            if change.is_some() {
                let error = result.unwrap_err();
                assert!(wire.edited.get());
                assert!(!matches!(error, CoreError::Uncertain(_)));
                wire.only_adds(1);
            } else {
                assert_eq!(result.unwrap().first, 4);
                wire.only_adds(2);
            }
        }
    }
}

#[test]
fn next_rechecks_the_exact_duplicate_occurrence_after_the_final_destination_reads() {
    for generation in [1, 2] {
        for native in [false, true] {
            let rig = Rig::new(generation);
            let source = if native {
                Source::Native(rig.native(false))
            } else {
                Source::Spotify(TRACK)
            };
            let destination = rig.destination();
            let mut wire = Wire::new(&rig);
            wire.stage = Some(Stage::FinalHead(if native { 1 } else { 2 }));
            wire.change = Change::Advance;
            source.enqueue(&wire, &rig, true, &destination).unwrap_err();
            assert!(wire.edited.get());
            wire.only_adds(0);
            assert_eq!(rig.queue(), ORIGINAL);
            let position = soap::get_position_info(&rig.sim.lan(), rig.host).unwrap();
            assert_eq!((position.track, position.uri.as_str()), (2, ORIGINAL[1]));
        }
    }
}

#[test]
fn native_next_refuses_a_stable_position_that_does_not_identify_a_queue_occurrence() {
    for generation in [1, 2] {
        for bad_position in [BadPosition::Zero, BadPosition::WrongUri] {
            let rig = Rig::new(generation);
            let source = Source::Native(rig.native(false));
            let destination = rig.destination();
            let mut wire = Wire::new(&rig);
            wire.bad_position = Some(bad_position);
            let error = source.enqueue(&wire, &rig, true, &destination).unwrap_err();
            assert!(!matches!(error, CoreError::Uncertain(_)));
            wire.only_adds(0);
            assert_eq!(rig.queue(), ORIGINAL);
            let position = soap::get_position_info(&rig.sim.lan(), rig.host).unwrap();
            assert_eq!((position.track, position.uri.as_str()), (1, ORIGINAL[0]));
        }
    }
}

#[test]
fn lost_replies_recover_only_exact_tracks_and_never_repeat_container_adds() {
    for generation in [1, 2] {
        for native in [false, true] {
            for container in [false, true] {
                let rig = Rig::new(generation);
                let source = if native {
                    Source::Native(rig.native(container))
                } else {
                    Source::Spotify(if container { ALBUM } else { TRACK })
                };
                let destination = rig.destination();
                let mut wire = Wire::new(&rig);
                wire.reply = Reply::Lost;
                let result = source.enqueue(&wire, &rig, false, &destination);
                if container {
                    assert!(matches!(result, Err(CoreError::Uncertain(_))));
                } else {
                    let added = result.unwrap();
                    assert_eq!((added.first, added.uris.len()), (4, 1));
                    assert!(added.proof.is_some());
                }
                wire.only_adds(1);
                assert!(rig.queue().len() > ORIGINAL.len());
            }
        }
    }
}
