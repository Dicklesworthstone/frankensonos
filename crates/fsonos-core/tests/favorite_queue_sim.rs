//! Native favorites queued through real S1/S2 loopback SOAP, never started.

use fsonos_core::favorites::{self, Favorite, FavoriteKind};
use fsonos_core::{CoreError, HouseholdState, control, favorite_queue};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::xml_escape;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

fn fixture(generation: u8) -> (SimHandle, Vec<HouseholdState>, PlayerId) {
    let builder = SimHousehold::builder();
    let builder = if generation == 1 {
        builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
    } else {
        builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
    };
    let sim = builder.spawn().unwrap();
    let player = sim.player("Den").unwrap();
    let coordinator = PlayerId(player.uuid.clone());
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
    (sim, vec![household], coordinator)
}

fn local(name: &str) -> Favorite {
    let uri = format!("x-file-cifs://example.invalid/{name}.flac");
    Favorite {
        id: format!("FV:2/{name}"),
        title: name.into(),
        kind: FavoriteKind::Track,
        metadata: format!(
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
             <item id=\"{name}\"><dc:title>{name}</dc:title>\
             <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
             <res>{}</res></item></DIDL-Lite>",
            xml_escape(&uri)
        ),
        uri: Some(uri),
        description: None,
        art_uri: None,
    }
}

fn queue(t: &impl Transport, host: IpAddr) -> Vec<String> {
    browse_snapshot(t, host, QUEUE)
        .unwrap()
        .objects
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn seed(sim: &SimHandle, households: &[HouseholdState], coordinator: &PlayerId) -> Vec<String> {
    let host = sim.player("Den").unwrap().ip;
    for name in ["owner-a", "owner-b"] {
        let favorite = local(name);
        soap::add_uri_to_queue(
            &sim.lan(),
            host,
            favorite.uri.as_deref().unwrap(),
            &favorite.metadata,
            false,
        )
        .unwrap();
    }
    control::play_queue_from(&sim.lan(), households, coordinator, 1).unwrap();
    queue(&sim.lan(), host)
}

#[derive(Clone, Copy)]
enum Fault {
    Reject,
    BeforeAdd,
    LostReply,
    MalformedReply,
    WrongCount,
    ConcurrentAdd,
    ReadAfterAdd,
}

struct Tape {
    inner: SimLan,
    fault: Cell<Option<Fault>>,
    adds: Cell<usize>,
    unreadable: Cell<bool>,
    calls: RefCell<Vec<(String, String)>>,
}

impl Tape {
    fn new(sim: &SimHandle, fault: Option<Fault>) -> Self {
        Self {
            inner: sim.lan(),
            fault: Cell::new(fault),
            adds: Cell::new(0),
            unreadable: Cell::new(false),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn assert_only_adds(&self, expected: usize) {
        assert_eq!(self.adds.get(), expected);
        for (action, _) in self.calls.borrow().iter() {
            assert!(
                matches!(
                    action.as_str(),
                    "GetMediaInfo" | "GetPositionInfo" | "Browse" | "AddURIToQueue"
                ),
                "unexpected playback mutation: {action}"
            );
        }
    }
}

impl Transport for Tape {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push((name.into(), body.into()));
        let lost = || ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost reply".into(),
        };
        if name == "Browse" && self.unreadable.get() {
            return Err(lost());
        }
        if name != "AddURIToQueue" {
            return self.inner.soap_post(host, path, action, body);
        }
        self.adds.set(self.adds.get() + 1);
        let fault = self.fault.take();
        if matches!(fault, Some(Fault::Reject)) {
            return Err(ProtoError::SoapFault {
                code: 800,
                reason: "injected rejection".into(),
            });
        }
        if matches!(fault, Some(Fault::BeforeAdd)) {
            return Err(lost());
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        match fault {
            Some(Fault::LostReply) => Err(lost()),
            Some(Fault::MalformedReply) => Ok("<broken>".into()),
            Some(Fault::WrongCount) => Ok(reply.replace(
                "<NumTracksAdded>1</NumTracksAdded>",
                "<NumTracksAdded>2</NumTracksAdded>",
            )),
            Some(Fault::ConcurrentAdd) => {
                let other = local("other-client");
                soap::add_uri_to_queue(
                    &self.inner,
                    host,
                    other.uri.as_deref().unwrap(),
                    &other.metadata,
                    false,
                )?;
                Ok(reply)
            }
            Some(Fault::ReadAfterAdd) => {
                self.unreadable.set(true);
                Ok(reply)
            }
            None => Ok(reply),
            Some(Fault::Reject | Fault::BeforeAdd) => unreachable!("returned before sending"),
        }
    }
}

#[test]
fn native_tracks_append_or_play_next_without_interrupting_the_owner() {
    for generation in [1, 2] {
        for next in [false, true] {
            let (sim, households, coordinator) = fixture(generation);
            let host = sim.player("Den").unwrap().ip;
            let original = seed(&sim, &households, &coordinator);
            sim.retain_favorites(generation, |_| false).unwrap();
            let favorite = local("native-favorite");
            let t = Tape::new(&sim, None);
            let added =
                favorite_queue::enqueue(&t, &households, &coordinator, &favorite, next).unwrap();
            assert_eq!(added.first, if next { 2 } else { 3 });
            assert_eq!(added.uris, [favorite.uri.clone().unwrap()]);
            assert!(added.playing_queue);
            let mut expected = original.clone();
            expected.insert(if next { 1 } else { 2 }, favorite.uri.clone().unwrap());
            assert_eq!(queue(&sim.lan(), host), expected);
            let now = control::playback(&sim.lan(), &households, &coordinator).unwrap();
            assert_eq!(now.transport.state, TransportState::Playing);
            assert_eq!(now.position.uri, original[0]);
            t.assert_only_adds(1);
            assert!(t.calls.borrow().iter().any(|(action, body)| {
                action == "AddURIToQueue" && body.contains(&xml_escape(&favorite.metadata))
            }));
            assert!(
                control::unqueue(
                    &sim.lan(),
                    &households,
                    &coordinator,
                    added.first,
                    &added.uris
                )
                .unwrap()
            );
            assert_eq!(queue(&sim.lan(), host), original);
        }
    }
}

#[test]
fn native_album_and_playlist_favorites_need_no_track_render_template() {
    for generation in [1, 2] {
        for kind in ["album", "playlist"] {
            let (sim, households, coordinator) = fixture(generation);
            let host = sim.player("Den").unwrap().ip;
            let original = seed(&sim, &households, &coordinator);
            let marker = format!("spotify%3a{kind}%3a");
            sim.retain_favorites(generation, |uri| uri.contains(&marker))
                .unwrap();
            assert!(
                control::spotify_params(&sim.lan(), &households, &coordinator)
                    .unwrap()
                    .is_none()
            );
            let favorite = favorites::list(&sim.lan(), &households, &coordinator)
                .unwrap()
                .into_iter()
                .find(|favorite| favorite.kind == FavoriteKind::Container)
                .unwrap();
            let t = Tape::new(&sim, None);
            let added =
                favorite_queue::enqueue(&t, &households, &coordinator, &favorite, true).unwrap();
            assert_eq!(added.first, 2);
            assert!(added.uris.len() > 1);
            let mut expected = vec![original[0].clone()];
            expected.extend(added.uris.clone());
            expected.push(original[1].clone());
            assert_eq!(queue(&sim.lan(), host), expected);
            t.assert_only_adds(1);
            assert!(
                control::unqueue(
                    &sim.lan(),
                    &households,
                    &coordinator,
                    added.first,
                    &added.uris
                )
                .unwrap()
            );
            assert_eq!(queue(&sim.lan(), host), original);
        }
    }
}

#[test]
fn lost_native_track_replies_are_read_back_without_duplicate_mutations() {
    for generation in [1, 2] {
        for fault in [Fault::LostReply, Fault::MalformedReply] {
            let (sim, households, coordinator) = fixture(generation);
            let host = sim.player("Den").unwrap().ip;
            let favorite = local("duplicate");
            soap::add_uri_to_queue(
                &sim.lan(),
                host,
                favorite.uri.as_deref().unwrap(),
                &favorite.metadata,
                false,
            )
            .unwrap();
            let t = Tape::new(&sim, Some(fault));
            let added =
                favorite_queue::enqueue(&t, &households, &coordinator, &favorite, false).unwrap();
            assert_eq!(added.first, 2);
            assert_eq!(
                queue(&sim.lan(), host),
                vec![favorite.uri.clone().unwrap(); 2]
            );
            t.assert_only_adds(1);
        }
    }
}

#[test]
fn unconfirmed_writes_never_replay_or_change_transport() {
    for fault in [
        Fault::BeforeAdd,
        Fault::WrongCount,
        Fault::ConcurrentAdd,
        Fault::ReadAfterAdd,
    ] {
        let (sim, households, coordinator) = fixture(2);
        let t = Tape::new(&sim, Some(fault));
        let error = favorite_queue::enqueue(&t, &households, &coordinator, &local("track"), false)
            .unwrap_err();
        assert!(error.to_string().contains("uncertain"), "{error}");
        t.assert_only_adds(1);
    }
}

#[test]
fn a_lost_container_reply_is_not_inferred_from_an_unattributable_expansion() {
    for generation in [1, 2] {
        let (sim, households, coordinator) = fixture(generation);
        let favorite = favorites::list(&sim.lan(), &households, &coordinator)
            .unwrap()
            .into_iter()
            .find(|favorite| favorite.kind == FavoriteKind::Container)
            .unwrap();
        let t = Tape::new(&sim, Some(Fault::LostReply));
        let error =
            favorite_queue::enqueue(&t, &households, &coordinator, &favorite, false).unwrap_err();
        assert!(error.to_string().contains("uncertain"));
        assert!(!queue(&sim.lan(), sim.player("Den").unwrap().ip).is_empty());
        t.assert_only_adds(1);
    }
}

#[test]
fn explicit_rejection_and_unqueueable_sources_do_not_touch_the_queue() {
    let (sim, households, coordinator) = fixture(2);
    let t = Tape::new(&sim, Some(Fault::Reject));
    assert!(matches!(
        favorite_queue::enqueue(&t, &households, &coordinator, &local("track"), false),
        Err(CoreError::Proto(ProtoError::SoapFault { code: 800, .. }))
    ));
    assert!(queue(&sim.lan(), sim.player("Den").unwrap().ip).is_empty());
    t.assert_only_adds(1);
    for kind in [FavoriteKind::Stream, FavoriteKind::Unplayable] {
        let t = Tape::new(&sim, None);
        let mut favorite = local("not-queueable");
        favorite.kind = kind;
        assert!(favorite_queue::enqueue(&t, &households, &coordinator, &favorite, true).is_err());
        assert!(t.calls.borrow().is_empty());
    }
    let t = Tape::new(&sim, None);
    let mut favorite = local("missing-uri");
    favorite.uri = None;
    assert!(favorite_queue::enqueue(&t, &households, &coordinator, &favorite, true).is_err());
    assert!(t.calls.borrow().is_empty());
}

#[test]
fn play_next_while_a_station_is_active_appends_without_switching_sources() {
    for generation in [1, 2] {
        let (sim, households, coordinator) = fixture(generation);
        let host = sim.player("Den").unwrap().ip;
        let original = seed(&sim, &households, &coordinator);
        let station = "x-rincon-mp3radio://example.invalid/radio";
        control::play_uri(&sim.lan(), &households, &coordinator, station, "").unwrap();
        let t = Tape::new(&sim, None);
        let added =
            favorite_queue::enqueue(&t, &households, &coordinator, &local("later"), true).unwrap();
        assert_eq!(added.first, 3);
        assert!(!added.playing_queue);
        assert_eq!(soap::get_media_info(&sim.lan(), host).unwrap().uri, station);
        assert_eq!(&queue(&sim.lan(), host)[..2], &original);
        t.assert_only_adds(1);
        assert!(t.calls.borrow().iter().any(|(action, body)| {
            action == "AddURIToQueue" && body.contains("<EnqueueAsNext>0</EnqueueAsNext>")
        }));
    }
}
