//! Podcast catalogs resolve to episodes before crossing the shared playback
//! boundary. Exercise S1/S2 coordinator targeting, write guards and undo over
//! real simulator sockets with a deterministic read-only catalog.

use fsonos_api::plan::plan_play;
use fsonos_api::source::{SourceSelection, SourceSelectionKind, SpotifySources};
use fsonos_api::surface::queue::QueueAddRequest;
use fsonos_api::{ErrorCode, Failure, PlayRequest, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::Transport;
use fsonos_proto::content::{BrowseFlag, QUEUE, browse, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const SHOW: &str = "spotify:show:2000000000000000000001";
const EPISODES: [&str; 3] = [
    "spotify:episode:1000000000000000000001",
    "spotify:episode:1000000000000000000002",
    "spotify:episode:1000000000000000000003",
];

fn selection() -> SourceSelection {
    SourceSelection {
        tracks: EPISODES
            .iter()
            .enumerate()
            .map(|(index, uri)| ((*uri).into(), format!("Show episode {}", index + 1)))
            .collect(),
        kind: SourceSelectionKind::ShowEpisodes,
    }
}

struct Catalog {
    calls: Arc<AtomicUsize>,
    result: Result<SourceSelection, Failure>,
    during_lookup: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Catalog {
    fn ready(calls: &Arc<AtomicUsize>) -> Self {
        Self {
            calls: Arc::clone(calls),
            result: Ok(selection()),
            during_lookup: None,
        }
    }
}

impl SpotifySources for Catalog {
    fn artist_tracks(&self, _artist_uri: &str) -> Result<SourceSelection, Failure> {
        panic!("a show must resolve through its episode catalog");
    }

    fn show_episodes(&self, show_uri: &str) -> Result<SourceSelection, Failure> {
        // Share links must be canonicalized before the catalog is read.
        assert_eq!(show_uri, SHOW);
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(change) = &self.during_lookup {
            change();
        }
        self.result.clone()
    }
}

struct Rig {
    sim: Arc<SimHandle>,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
    lead: IpAddr,
    member: IpAddr,
    guest: IpAddr,
    generation: u8,
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
            SimPlayerSpec::new("Member", model),
            SimPlayerSpec::new("Guest", model),
        ];
        let builder = SimHousehold::builder();
        let sim = Arc::new(
            if generation == 1 {
                builder.s1(players)
            } else {
                builder.s2(players)
            }
            .spawn()
            .unwrap(),
        );
        let lead = sim.player("Lead").unwrap().ip;
        let member = sim.player("Member").unwrap().ip;
        let guest = sim.player("Guest").unwrap().ip;
        let coordinator = PlayerId(sim.player("Lead").unwrap().uuid.clone());
        soap::join_group(&sim.lan(), member, &coordinator).unwrap();
        let households = inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
        assert_eq!(
            resolve_room(&households, "Member").unwrap().coordinator.id,
            coordinator
        );
        let original = [
            ("spotify:track:0000000000000000000001", "Original one"),
            ("spotify:track:0000000000000000000002", "Original two"),
            ("spotify:track:0000000000000000000003", "Original three"),
        ];
        control::queue_spotify_tracks(&sim.lan(), &households, &coordinator, &original).unwrap();
        control::play_queue_from(&sim.lan(), &households, &coordinator, 2).unwrap();
        set_play_mode(&sim.lan(), lead, PlayMode::ShuffleRepeatOne).unwrap();
        for (host, level) in [(lead, 80), (member, 70), (guest, 60)] {
            soap::set_volume(&sim.lan(), host, level).unwrap();
        }
        Self {
            sim,
            households,
            coordinator,
            lead,
            member,
            guest,
            generation,
        }
    }

    fn surface(&self, policy: Policy) -> Surface {
        self.surface_over(Box::new(self.sim.lan()), policy)
    }

    fn surface_over(&self, transport: Box<dyn Transport + Send + Sync>, policy: Policy) -> Surface {
        let known = self.households.clone();
        Surface::new(
            transport,
            Box::new(move |_| Ok(known.clone())),
            policy,
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "mcp")
    }

    fn with_catalog(&self, catalog: Catalog, policy: Policy) -> Surface {
        self.surface(policy).with_spotify_sources(Box::new(catalog))
    }

    fn queue(&self) -> Vec<(String, String)> {
        browse_all(&self.sim.lan(), self.lead, QUEUE)
            .unwrap()
            .into_iter()
            .map(|item| {
                (
                    spotify_uri_from_renderer_uri(&item.res.unwrap().uri).unwrap(),
                    item.title,
                )
            })
            .collect()
    }

    fn volumes(&self) -> [u8; 3] {
        [self.lead, self.member, self.guest]
            .map(|host| soap::get_volume(&self.sim.lan(), host).unwrap())
    }

    fn writes_since(&self, start: usize) -> Vec<SoapLogEntry> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(start)
            .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
            .collect()
    }
}

fn capped_policy() -> Policy {
    Policy::from_toml("[defaults]\nmax_volume = 25\n").unwrap()
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Play,
    Next,
    Append,
}

impl Operation {
    fn run(self, surface: &Surface) -> Result<(), Failure> {
        match self {
            Self::Play => surface
                .control(&Client::McpStdio, "play", |rooms| {
                    plan_play(rooms, &play_request())
                })
                .map(drop),
            Self::Next | Self::Append => surface
                .queue_add(
                    &Client::McpStdio,
                    &queue_request(matches!(self, Self::Next)),
                )
                .map(drop),
        }
    }
}

fn play_request() -> PlayRequest {
    PlayRequest {
        zone: "Member".into(),
        source_uri: "https://open.spotify.com/show/2000000000000000000001?si=example".into(),
        title: Some("Selected show".into()),
    }
}

fn queue_request(next: bool) -> QueueAddRequest {
    QueueAddRequest {
        zone: "Member".into(),
        source_uri: SHOW.into(),
        title: Some("Selected show".into()),
        next,
    }
}

#[test]
fn show_play_replaces_the_coordinators_queue_with_ordered_episodes_and_caps() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let calls = Arc::new(AtomicUsize::new(0));
        let surface = rig.with_catalog(Catalog::ready(&calls), capped_policy());
        let marker = rig.sim.soap_log().len();
        let result = surface
            .control(&Client::McpStdio, "play", |rooms| {
                plan_play(rooms, &play_request())
            })
            .unwrap();
        assert!(result.changed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(rig.queue(), selection().tracks);
        assert_eq!(rig.volumes(), [25, 25, 60]);
        assert_eq!(
            get_play_mode(&rig.sim.lan(), rig.lead).unwrap(),
            PlayMode::Normal
        );
        let position = soap::get_position_info(&rig.sim.lan(), rig.lead).unwrap();
        assert_eq!(position.track, 1);
        assert_eq!(
            spotify_uri_from_renderer_uri(&position.uri).as_deref(),
            Some(EPISODES[0])
        );
        assert_eq!(
            soap::get_transport_info(&rig.sim.lan(), rig.lead)
                .unwrap()
                .state,
            TransportState::Playing
        );
        let writes = rig.writes_since(marker);
        let adds: Vec<_> = writes
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .collect();
        assert_eq!(adds.len(), EPISODES.len());
        assert!(
            writes
                .iter()
                .filter(|call| call.action != "SetVolume")
                .all(|call| call.player == rig.coordinator.0)
        );
        assert!(adds.iter().all(|call| {
            call.args
                .iter()
                .all(|(_, value)| !value.contains("spotify%3ashow") && !value.contains(SHOW))
        }));
        for host in [rig.member, rig.guest] {
            assert!(browse_all(&rig.sim.lan(), host, QUEUE).unwrap().is_empty());
        }
        let heard = surface
            .recent_plays(&Client::McpStdio, Some("Member"), 10)
            .unwrap();
        assert_eq!(heard.len(), 1);
        assert_eq!(heard[0].source_uri, EPISODES[0]);
        let marker = rig.sim.soap_log().len();
        let refused = surface.undo(&Client::McpStdio, true).unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidArgument);
        assert!(rig.writes_since(marker).is_empty());
        assert_eq!(rig.queue(), selection().tracks);
    }
}

#[test]
fn show_next_and_append_are_silent_ordered_and_undo_preserves_later_queue_edits() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let calls = Arc::new(AtomicUsize::new(0));
        let surface = rig.with_catalog(Catalog::ready(&calls), capped_policy());
        let original = rig.queue();
        let before = control::playback(&rig.sim.lan(), &rig.households, &rig.coordinator).unwrap();
        for (next, first) in [(true, 3), (false, 4)] {
            let marker = rig.sim.soap_log().len();
            let added = surface
                .queue_add(&Client::McpStdio, &queue_request(next))
                .unwrap();
            assert_eq!(
                (added.first, added.count, added.playing_queue),
                (first, 3, true)
            );
            assert_eq!(added.zone, "Lead");
            let mut expected = original.clone();
            let index = usize::try_from(first - 1).unwrap();
            expected.splice(index..index, selection().tracks);
            assert_eq!(rig.queue(), expected);
            assert_eq!(rig.volumes(), [80, 70, 60]);
            assert_eq!(
                control::playback(&rig.sim.lan(), &rig.households, &rig.coordinator).unwrap(),
                before
            );
            let writes = rig.writes_since(marker);
            assert_eq!(writes.len(), 3);
            assert!(
                writes
                    .iter()
                    .all(|call| call.action == "AddURIToQueue" && call.player == rig.coordinator.0)
            );
            let undone = surface.undo(&Client::McpStdio, true).unwrap().unwrap();
            assert_eq!(undone.unqueued, [rig.coordinator.clone()]);
            assert_eq!(rig.queue(), original);
            assert_eq!(
                control::playback(&rig.sim.lan(), &rig.households, &rig.coordinator).unwrap(),
                before
            );
            assert_eq!(
                get_play_mode(&rig.sim.lan(), rig.lead).unwrap(),
                PlayMode::ShuffleRepeatOne
            );
        }
        surface
            .queue_add(&Client::McpStdio, &queue_request(false))
            .unwrap();
        let page = browse(
            &rig.sim.lan(),
            rig.lead,
            QUEUE,
            BrowseFlag::DirectChildren,
            0,
            1,
        )
        .unwrap();
        fsonos_proto::queue::reorder_tracks(&rig.sim.lan(), rig.lead, 4, 3, 1, page.update_id)
            .unwrap();
        let changed = rig.queue();
        let marker = rig.sim.soap_log().len();
        let refused = surface.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert!(refused.unqueued.is_empty());
        assert_eq!(refused.unqueue_failures.len(), 1);
        assert!(rig.writes_since(marker).is_empty());
        assert_eq!(rig.queue(), changed);
        assert_eq!(rig.volumes(), [80, 70, 60]);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(
            surface
                .recent_plays(&Client::McpStdio, None, 10)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn wrong_catalog_kinds_and_missing_auth_cannot_cap_or_mutate_current_music() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let original = rig.queue();
        let before = control::playback(&rig.sim.lan(), &rig.households, &rig.coordinator).unwrap();
        let mut track_in_show = selection();
        track_in_show.tracks[0].0 = "spotify:track:1000000000000000000001".into();
        let mut artist_kind = selection();
        artist_kind.kind = SourceSelectionKind::TopTracks;
        let cases = [
            Ok(track_in_show),
            Ok(artist_kind),
            Ok(SourceSelection {
                tracks: Vec::new(),
                kind: SourceSelectionKind::ShowEpisodes,
            }),
            Err(Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "the local Spotify session needs sign-in",
            )),
        ];
        for result in cases {
            let expected = result
                .as_ref()
                .err()
                .map_or(ErrorCode::NotReady, |error| error.code);
            let calls = Arc::new(AtomicUsize::new(0));
            let surface = rig.with_catalog(
                Catalog {
                    result,
                    ..Catalog::ready(&calls)
                },
                capped_policy(),
            );
            for operation in [Operation::Play, Operation::Next, Operation::Append] {
                let marker = rig.sim.soap_log().len();
                assert_eq!(operation.run(&surface).unwrap_err().code, expected);
                assert!(rig.writes_since(marker).is_empty());
                assert_eq!(rig.queue(), original);
                assert_eq!(rig.volumes(), [80, 70, 60]);
                assert_eq!(
                    control::playback(&rig.sim.lan(), &rig.households, &rig.coordinator).unwrap(),
                    before
                );
            }
            assert_eq!(calls.load(Ordering::SeqCst), 3);
        }
        let surface = rig.surface(capped_policy());
        let marker = rig.sim.soap_log().len();
        for operation in [Operation::Play, Operation::Next, Operation::Append] {
            assert_eq!(
                operation.run(&surface).unwrap_err().code,
                ErrorCode::SpotifyAuthRequired
            );
        }
        assert!(rig.writes_since(marker).is_empty());
        assert_eq!(rig.queue(), original);
        assert_eq!(rig.volumes(), [80, 70, 60]);
    }
}

#[test]
fn missing_renderer_parameters_stop_show_resolution_before_caps_or_writes() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let original = rig.queue();
        rig.sim.retain_favorites(rig.generation, |_| false).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let surface = rig.with_catalog(Catalog::ready(&calls), capped_policy());
        for operation in [Operation::Play, Operation::Next, Operation::Append] {
            let marker = rig.sim.soap_log().len();
            assert_eq!(
                operation.run(&surface).unwrap_err().code,
                ErrorCode::RenderParamsMissing
            );
            assert!(rig.writes_since(marker).is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(rig.queue(), original);
        assert_eq!(rig.volumes(), [80, 70, 60]);
    }
}

#[test]
fn source_or_group_changes_during_show_lookup_prevent_every_surface_write() {
    for generation in [1, 2] {
        for operation in [Operation::Play, Operation::Next, Operation::Append] {
            for group_changed in [false, true] {
                let rig = Rig::new(generation);
                let original = rig.queue();
                let calls = Arc::new(AtomicUsize::new(0));
                let changed_at = Arc::new(AtomicUsize::new(usize::MAX));
                let sim = Arc::clone(&rig.sim);
                let after = Arc::clone(&changed_at);
                let (lead, guest, coordinator) = (rig.lead, rig.guest, rig.coordinator.clone());
                let catalog = Catalog {
                    during_lookup: Some(Box::new(move || {
                        if group_changed {
                            soap::join_group(&sim.lan(), guest, &coordinator).unwrap();
                        } else {
                            soap::set_av_transport_uri(
                                &sim.lan(),
                                lead,
                                "http://example.invalid/manual-podcast.mp3",
                                "",
                            )
                            .unwrap();
                            soap::set_volume(&sim.lan(), lead, 37).unwrap();
                        }
                        after.store(sim.soap_log().len(), Ordering::SeqCst);
                    })),
                    ..Catalog::ready(&calls)
                };
                let surface = rig.with_catalog(catalog, capped_policy());
                let failure = operation.run(&surface).unwrap_err();
                assert!(
                    matches!(
                        failure.code,
                        ErrorCode::UpnpFault | ErrorCode::QueueChanged | ErrorCode::NotCoordinator
                    ),
                    "{generation} {operation:?} group={group_changed}: {failure:?}"
                );
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                let marker = changed_at.load(Ordering::SeqCst);
                assert_ne!(marker, usize::MAX);
                assert!(rig.writes_since(marker).is_empty());
                assert_eq!(rig.queue(), original);
                assert_eq!(
                    rig.volumes(),
                    if group_changed {
                        [80, 70, 60]
                    } else {
                        [37, 70, 60]
                    }
                );
                if !group_changed {
                    assert_eq!(
                        soap::get_media_info(&rig.sim.lan(), rig.lead).unwrap().uri,
                        "http://example.invalid/manual-podcast.mp3"
                    );
                }
            }
        }
    }
}

#[test]
fn denied_show_actions_never_resolve_the_catalog_or_contact_speakers() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let calls = Arc::new(AtomicUsize::new(0));
        let policy =
            Policy::from_toml("[clients.\"mcp-stdio\"]\ndeny = ['play', 'add_to_queue']\n")
                .unwrap();
        let surface = rig.with_catalog(Catalog::ready(&calls), policy);
        let marker = rig.sim.soap_log().len();
        for operation in [Operation::Play, Operation::Next, Operation::Append] {
            assert_eq!(
                operation.run(&surface).unwrap_err().code,
                ErrorCode::PolicyDenied
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(rig.sim.soap_log().len(), marker);
    }
}
