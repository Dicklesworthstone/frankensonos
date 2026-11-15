//! An acknowledged Play is not listening evidence when its renderer reports
//! failure. Exercise command history on real S1/S2 simulator sockets, changing
//! only the reported playback observation after the speaker accepted Play.

use fsonos_api::plan::plan_play;
use fsonos_api::{PlayFavoriteRequest, PlayRequest, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const STREAM: &str = "x-rincon-mp3radio://example.invalid/history.mp3";

#[derive(Clone, Copy, Debug)]
struct Report {
    status: Option<&'static str>,
    state: &'static str,
    failed_read: bool,
}

struct ReportedPlayback {
    inner: SimLan,
    report: Report,
    played: AtomicBool,
    reads: AtomicUsize,
}

fn field(response: &mut String, name: &str, value: Option<&str>) {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = response.find(&open).unwrap();
    let content = start + open.len();
    let end = response[content..].find(&close).unwrap() + content;
    if let Some(value) = value {
        response.replace_range(content..end, value);
    } else {
        response.replace_range(start..end + close.len(), "");
    }
}

impl Transport for ReportedPlayback {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let mut response = self.inner.soap_post(host, path, action, body)?;
        match action.trim_matches('"').rsplit('#').next().unwrap() {
            "Play" => self.played.store(true, Ordering::SeqCst),
            "GetTransportInfo" if self.played.load(Ordering::SeqCst) => {
                self.reads.fetch_add(1, Ordering::SeqCst);
                if self.report.failed_read {
                    return Err(ProtoError::Network {
                        target: host.to_string(),
                        detail: "injected history observation failure".into(),
                    });
                }
                field(&mut response, "CurrentTransportStatus", self.report.status);
                field(
                    &mut response,
                    "CurrentTransportState",
                    Some(self.report.state),
                );
            }
            _ => {}
        }
        Ok(response)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Direct,
    Favorite,
}

struct Rig {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    favorite: String,
    favorite_uri: String,
}

impl Rig {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [SimPlayerSpec::new("Room", model)];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let player = sim.player("Room").unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
        let households = vec![house];
        let favorite = favorites::list(
            &sim.lan(),
            &households,
            &PlayerId(player.uuid.clone()),
        )
        .unwrap()
        .into_iter()
        .find(|favorite| favorite.kind == FavoriteKind::Stream)
        .unwrap();
        Self {
            sim,
            households,
            favorite: favorite.title,
            favorite_uri: favorite.uri.unwrap(),
        }
    }

    fn surface(&self, report: Report) -> (Surface, Arc<ReportedPlayback>) {
        let replies = Arc::new(ReportedPlayback {
            inner: self.sim.lan(),
            report,
            played: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
        });
        let households = self.households.clone();
        let surface = Surface::new(
            Box::new(Arc::clone(&replies)),
            Box::new(move |_| Ok(households.clone())),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "history-status");
        (surface, replies)
    }

    fn start(&self, surface: &Surface, kind: Kind) {
        let outcome = match kind {
            Kind::Direct => surface.control(&Client::Cli, "play", |rooms| {
                plan_play(
                    rooms,
                    &PlayRequest {
                        zone: "Room".into(),
                        source_uri: STREAM.into(),
                        title: None,
                    },
                )
            }),
            Kind::Favorite => surface.play_favorite(
                &Client::Cli,
                &PlayFavoriteRequest {
                    zone: "Room".into(),
                    favorite: self.favorite.clone(),
                },
            ),
        }
        .unwrap();
        assert!(outcome.changed, "{kind:?}: {outcome:?}");
    }

    fn assert_one_play(&self, since: usize) {
        assert_eq!(
            self.sim.soap_log()[since..]
                .iter()
                .filter(|entry| entry.action == "Play")
                .count(),
            1,
            "a history observation must never replay the accepted command"
        );
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), self.sim.player("Room").unwrap().ip)
                .unwrap()
                .state,
            TransportState::Playing,
            "only the observed response was changed, not the accepted Play"
        );
    }

    fn uri(&self, kind: Kind) -> &str {
        match kind {
            Kind::Direct => STREAM,
            Kind::Favorite => &self.favorite_uri,
        }
    }
}

#[test]
fn acknowledged_renderer_errors_are_not_counted_as_plays() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        for kind in [Kind::Direct, Kind::Favorite] {
            for status in ["ERROR_OCCURRED", " ERROR_OCCURRED "] {
                let (surface, replies) = rig.surface(Report {
                    status: Some(status),
                    state: "PLAYING",
                    failed_read: false,
                });
                let since = rig.sim.soap_log().len();
                rig.start(&surface, kind);
                assert!(replies.reads.load(Ordering::SeqCst) > 0);
                assert!(
                    surface.recent_plays(&Client::Cli, None, 10).unwrap().is_empty(),
                    "{generation}: {kind:?}: {status}"
                );
                rig.assert_one_play(since);
            }
        }
    }
}

#[test]
fn missing_and_non_error_statuses_preserve_observed_plays() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        for kind in [Kind::Direct, Kind::Favorite] {
            for status in [
                None,
                Some(""),
                Some("OK"),
                Some("vendor-specific"),
                Some("error_occurred"),
            ] {
                let (surface, replies) = rig.surface(Report {
                    status,
                    state: "PLAYING",
                    failed_read: false,
                });
                let since = rig.sim.soap_log().len();
                rig.start(&surface, kind);
                assert!(replies.reads.load(Ordering::SeqCst) > 0);
                let plays = surface.recent_plays(&Client::Cli, None, 10).unwrap();
                assert_eq!(plays.len(), 1, "{generation}: {kind:?}: {status:?}");
                assert_eq!(plays[0].source_uri, rig.uri(kind));
                rig.assert_one_play(since);
            }
        }
    }
}

#[test]
fn failed_reads_and_buffering_do_not_fail_or_replay_accepted_commands() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        for kind in [Kind::Direct, Kind::Favorite] {
            for (state, failed_read) in [
                ("TRANSITIONING", false),
                ("STOPPED", false),
                ("PLAYING", true),
            ] {
                let (surface, replies) = rig.surface(Report {
                    status: Some("OK"),
                    state,
                    failed_read,
                });
                let since = rig.sim.soap_log().len();
                rig.start(&surface, kind);
                assert!(replies.reads.load(Ordering::SeqCst) > 0);
                assert!(
                    surface.recent_plays(&Client::Cli, None, 10).unwrap().is_empty(),
                    "{generation}: {kind:?}: {state}: failed read {failed_read}"
                );
                rig.assert_one_play(since);
            }
        }
    }
}
