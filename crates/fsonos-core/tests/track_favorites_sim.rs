//! Track recovery uses the household's next observed account when its majority
//! favorite template is stale. Only Browse FV:2 is substituted; real S1/S2
//! simulator sockets reject the stale account and execute the selected source.

use fsonos_core::control::{Queued, TrackBatchDestination};
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    SpotifyRenderParams, spotify_track_didl, spotify_track_uri, spotify_uri_from_renderer_uri,
    xml_escape,
};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const TRACK: &str = "spotify:track:9000000000000000000001";
const ORIGINAL: [&str; 2] = [
    "x-file-cifs://example.invalid/first.flac",
    "x-file-cifs://example.invalid/second.flac",
];

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    host: IpAddr,
    coordinator: PlayerId,
    params: SpotifyRenderParams,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        }
        .spawn()
        .unwrap();
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let coordinator = PlayerId(player.uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        for uri in ORIGINAL {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        control::play_queue_from(&lan, &houses, &coordinator, 1).unwrap();
        soap::set_volume(&lan, host, 31).unwrap();
        soap::set_mute(&lan, host, true).unwrap();
        let params = sim.render_params(generation).unwrap();
        Self {
            sim,
            houses,
            host,
            coordinator,
            params,
        }
    }

    fn queue(&self) -> Vec<String> {
        browse_all(&self.sim.lan(), self.host, QUEUE)
            .unwrap()
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn controls_unchanged(&self) {
        let lan = self.sim.lan();
        assert_eq!(soap::get_volume(&lan, self.host).unwrap(), 31);
        assert!(soap::get_mute(&lan, self.host).unwrap());
    }
}

#[derive(Debug, Clone, Copy)]
enum Path {
    Play,
    Load,
    Next,
    Append,
}

impl Path {
    const ALL: [Self; 4] = [Self::Play, Self::Load, Self::Next, Self::Append];

    fn run(self, f: &Fixture, transport: &impl Transport) -> Result<Option<Queued>, CoreError> {
        match self {
            Self::Play => control::play_spotify_track(
                transport,
                &f.houses,
                &f.coordinator,
                TRACK,
                "Recovered track",
            )
            .map(|()| None),
            Self::Load => control::load_spotify_track(
                transport,
                &f.houses,
                &f.coordinator,
                TRACK,
                "Recovered track",
            )
            .map(|()| None),
            Self::Next => control::enqueue_spotify(
                transport,
                &f.houses,
                &f.coordinator,
                TRACK,
                "Recovered track",
                true,
            )
            .map(Some),
            Self::Append => {
                let destination = TrackBatchDestination::read(transport, f.host, &f.coordinator)?;
                control::enqueue_spotify_bound(
                    transport,
                    &f.houses,
                    &f.coordinator,
                    TRACK,
                    "Recovered track",
                    false,
                    &destination,
                )
                .map(Some)
            }
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Play | Self::Load => "SetAVTransportURI",
            Self::Next | Self::Append => "AddURIToQueue",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    LostReply,
    PostWrite800,
}

struct Favorites {
    inner: SimLan,
    reply: String,
    fault: Fault,
    applied: Cell<bool>,
    calls: RefCell<Vec<String>>,
}

impl Favorites {
    fn new(f: &Fixture, current: bool, fault: Fault) -> Self {
        let mut stale = f.params.clone();
        stale.sn += 100;
        stale.cdudn.push_str("-stale");
        let mut templates = vec![&stale, &stale];
        if current {
            templates.push(&f.params);
        }
        let items: Vec<_> = templates
            .iter()
            .enumerate()
            .map(|(index, params)| {
                let source = format!("spotify:track:ExampleFavorite{index}");
                format!(
                    "<item id=\"FV:2/{index}\"><res>{}</res><r:resMD>{}</r:resMD></item>",
                    xml_escape(&spotify_track_uri(&source, params)),
                    xml_escape(&spotify_track_didl(&source, "Favorite", params)),
                )
            })
            .collect();
        let didl = format!(
            "<DIDL-Lite xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{}</DIDL-Lite>",
            items.join("")
        );
        let reply = format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
             <s:Body><u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
             <Result>{}</Result><NumberReturned>{}</NumberReturned>\
             <TotalMatches>{}</TotalMatches><UpdateID>1</UpdateID>\
             </u:BrowseResponse></s:Body></s:Envelope>",
            xml_escape(&didl),
            items.len(),
            items.len(),
        );
        Self {
            inner: f.sim.lan(),
            reply,
            fault,
            applied: Cell::new(false),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn count(&self, action: &str) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|call| *call == action)
            .count()
    }
}

impl Transport for Favorites {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action_name = action.trim_matches('"').rsplit('#').next().unwrap();
        let favorites = action_name == "Browse" && body.contains("<ObjectID>FV:2</ObjectID>");
        self.calls.borrow_mut().push(if favorites {
            "Favorites".into()
        } else {
            action_name.into()
        });
        if favorites {
            return Ok(self.reply.clone());
        }
        if self.applied.get()
            && self.fault != Fault::None
            && matches!(action_name, "GetMediaInfo" | "Browse")
        {
            return Err(if self.fault == Fault::PostWrite800 {
                ProtoError::SoapFault {
                    code: 800,
                    reason: "post-write observation failed".into(),
                }
            } else {
                ProtoError::Network {
                    target: host.to_string(),
                    detail: "post-write observation unavailable".into(),
                }
            });
        }
        let reply = self.inner.soap_post(host, path, action, body)?;
        if matches!(action_name, "SetAVTransportURI" | "AddURIToQueue") {
            self.applied.set(true);
            if self.fault == Fault::LostReply {
                return Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "reply lost after the native write was accepted".into(),
                });
            }
        }
        Ok(reply)
    }
}

#[test]
fn stale_track_majorities_recover_play_load_next_and_append_in_both_households() {
    for generation in [1, 2] {
        for path in Path::ALL {
            let f = Fixture::new(generation);
            let transport = Favorites::new(&f, true, Fault::None);
            let queued = path.run(&f, &transport).unwrap();
            assert_eq!(transport.count(path.action()), 2, "S{generation} {path:?}");
            assert_eq!(transport.count("Favorites"), 2);
            let log = f.sim.soap_log();
            let attempts: Vec<_> = log
                .iter()
                .rev()
                .filter(|entry| entry.action == path.action())
                .take(2)
                .collect();
            assert!(
                attempts[0].result.is_ok(),
                "the current account really rendered"
            );
            assert_eq!(attempts[1].result, Err(800));
            let lan = f.sim.lan();
            let playing = soap::get_transport_info(&lan, f.host).unwrap().state;
            if let Some(queued) = queued {
                let expected = if matches!(path, Path::Next) { 2 } else { 3 };
                assert_eq!(queued.first, expected);
                assert_eq!(queued.uris.len(), 1);
                let mut queue = f.queue();
                let inserted = queue.remove(usize::try_from(expected - 1).unwrap());
                assert_eq!(
                    spotify_uri_from_renderer_uri(&inserted).as_deref(),
                    Some(TRACK)
                );
                assert_eq!(queue, ORIGINAL);
                assert_eq!(
                    soap::get_position_info(&lan, f.host).unwrap().uri,
                    ORIGINAL[0]
                );
                assert_eq!(playing, TransportState::Playing);
                assert_eq!(transport.count("Play"), 0);
            } else {
                assert_eq!(f.queue(), ORIGINAL);
                assert_eq!(
                    soap::get_media_info(&lan, f.host).unwrap().uri,
                    spotify_track_uri(TRACK, &f.params)
                );
                if matches!(path, Path::Play) {
                    assert_eq!(playing, TransportState::Playing);
                    assert_eq!(transport.count("Play"), 1);
                } else {
                    assert!(matches!(
                        playing,
                        TransportState::Paused | TransportState::Stopped
                    ));
                    assert_eq!(transport.count("Play"), 0);
                }
            }
            f.controls_unchanged();
        }
    }
}

#[test]
fn without_an_alternate_track_recovery_stops_after_one_retry() {
    for generation in [1, 2] {
        for path in Path::ALL {
            let f = Fixture::new(generation);
            let transport = Favorites::new(&f, false, Fault::None);
            assert!(matches!(
                path.run(&f, &transport),
                Err(CoreError::RenderParamsStale)
            ));
            assert_eq!(transport.count(path.action()), 2);
            assert_eq!(transport.count("Favorites"), 2);
            assert!(!transport.applied.get());
            assert_eq!(f.queue(), ORIGINAL);
            f.controls_unchanged();
        }
    }
}

#[test]
fn an_accepted_alternate_is_never_replayed_after_lost_replies_or_late_800() {
    for generation in [1, 2] {
        for path in Path::ALL {
            for fault in [Fault::LostReply, Fault::PostWrite800] {
                let f = Fixture::new(generation);
                let transport = Favorites::new(&f, true, fault);
                assert!(
                    path.run(&f, &transport).is_err(),
                    "S{generation} {path:?} {fault:?}"
                );
                assert!(transport.applied.get());
                assert_eq!(transport.count(path.action()), 2);
                assert_eq!(transport.count("Favorites"), 2);
                let queue = f.queue();
                if matches!(path, Path::Next | Path::Append) {
                    assert_eq!(
                        queue.len(),
                        3,
                        "only the single accepted occurrence remains"
                    );
                } else {
                    assert_eq!(queue, ORIGINAL);
                }
                f.controls_unchanged();
            }
        }
    }
}
