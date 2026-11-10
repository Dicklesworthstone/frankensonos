//! Artist and podcast-show selections are prepared before scene writes and
//! consumed by the final S1/S2 groups. No test uses the live Spotify service.

use chrono::{DateTime, Utc};
use fsonos_api::source::{SourceSelection, SourceSelectionKind, SpotifySources};
use fsonos_api::surface::scenes::SceneDefinition;
use fsonos_api::surface::schedules::{Ran, ScheduleRequest, Scheduler};
use fsonos_api::{ErrorCode, Failure, NoteCode, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{learn_spotify_params, spotify_uri_from_renderer_uri};
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const ORIGINAL: &str = "x-rincon-mp3radio://example.invalid/before-scene.mp3";
const ARTIST: &str = "spotify:artist:2000000000000000000001";
const SHOW: &str = "spotify:show:3000000000000000000001";

fn source_uri(kind: &str) -> &'static str {
    match kind {
        "artist" => ARTIST,
        "show" => SHOW,
        _ => unreachable!(),
    }
}

fn selection(kind: &str) -> SourceSelection {
    let item = if kind == "artist" { "track" } else { "episode" };
    SourceSelection {
        tracks: (1..=3)
            .map(|index| {
                (
                    format!("spotify:{item}:{index:022}"),
                    format!("{kind} selection {index}"),
                )
            })
            .collect(),
        kind: if kind == "artist" {
            SourceSelectionKind::TopTracks
        } else {
            SourceSelectionKind::ShowEpisodes
        },
    }
}

struct Catalog {
    calls: Arc<AtomicUsize>,
    ready: Arc<AtomicBool>,
    invalid: bool,
    during_lookup: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Catalog {
    fn ready() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            ready: Arc::new(AtomicBool::new(true)),
            invalid: false,
            during_lookup: None,
        }
    }

    fn read(&self, uri: &str, kind: &str) -> Result<SourceSelection, Failure> {
        assert_eq!(uri, source_uri(kind));
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(change) = &self.during_lookup {
            change();
        }
        if !self.ready.load(Ordering::SeqCst) {
            return Err(Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "the owner catalog session is temporarily unavailable",
            ));
        }
        let mut selected = selection(kind);
        if self.invalid {
            selected.tracks[2].0 = "spotify:album:0000000000000000000009".into();
        }
        Ok(selected)
    }
}

impl SpotifySources for Catalog {
    fn artist_tracks(&self, uri: &str) -> Result<SourceSelection, Failure> {
        self.read(uri, "artist")
    }

    fn show_episodes(&self, uri: &str) -> Result<SourceSelection, Failure> {
        self.read(uri, "show")
    }
}

struct Rig {
    sim: Arc<SimHandle>,
}

impl Rig {
    fn new() -> Self {
        Self {
            sim: Arc::new(
                SimHousehold::builder()
                    .s1([
                        SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
                        SimPlayerSpec::new("Office", SimModel::Play5Gen1),
                    ])
                    .s2([
                        SimPlayerSpec::new("Den", SimModel::One),
                        SimPlayerSpec::new("Study", SimModel::One),
                    ])
                    .spawn()
                    .unwrap(),
            ),
        }
    }

    fn surface(&self, policy: Policy, catalog: Catalog) -> Surface {
        self.surface_over(policy, catalog, Box::new(self.sim.lan()))
    }

    fn surface_over(
        &self,
        policy: Policy,
        catalog: Catalog,
        transport: Box<dyn Transport + Send + Sync>,
    ) -> Surface {
        let witnesses: Vec<_> = ["Kitchen", "Den"]
            .iter()
            .map(|name| {
                let player = self.sim.player(name).unwrap();
                (player.ip, player.household.clone())
            })
            .collect();
        Surface::new(
            transport,
            Box::new(move |transport| {
                witnesses
                    .iter()
                    .map(|(host, id)| {
                        let mut house = HouseholdState {
                            id: Some(HouseholdId(id.clone())),
                            ..HouseholdState::default()
                        };
                        house.apply_topology(
                            &get_zone_group_state(transport, *host)
                                .map_err(fsonos_core::CoreError::from)?,
                        );
                        Ok(house)
                    })
                    .collect()
            }),
            policy,
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "catalog-scenes-test")
        .with_spotify_sources(Box::new(catalog))
    }

    fn seed(&self) {
        for room in ["Kitchen", "Office", "Den", "Study"] {
            let host = self.sim.player(room).unwrap().ip;
            soap::set_volume(&self.sim.lan(), host, 85).unwrap();
            soap::set_av_transport_uri(&self.sim.lan(), host, ORIGINAL, "").unwrap();
            soap::play(&self.sim.lan(), host).unwrap();
        }
    }

    fn writes(&self, from: usize) -> Vec<SoapLogEntry> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(from)
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .collect()
    }

    fn queue(&self, room: &str) -> Vec<(String, String)> {
        browse_all(&self.sim.lan(), self.sim.player(room).unwrap().ip, QUEUE)
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
}

fn source(kind: &str) -> Value {
    let id = source_uri(kind).rsplit(':').next().unwrap();
    json!({"kind": "uri", "uri": format!("https://open.spotify.com/{kind}/{id}?si=scene")})
}

fn definition(kind: &str, playing: bool) -> SceneDefinition {
    serde_json::from_value(json!({
        "groups": [
            {"coordinator": "Kitchen", "members": ["Office"],
             "source": source(kind), "playing": playing},
            {"coordinator": "Den", "members": ["Study"],
             "source": source(kind), "playing": playing}
        ],
        "volumes": {"Kitchen": 20, "Office": 15, "Den": 20, "Study": 15}
    }))
    .unwrap()
}

fn capped() -> Policy {
    Policy::from_toml("[defaults]\nmax_volume = 25\n").unwrap()
}

#[test]
fn catalog_scenes_resolve_once_and_play_or_load_silently_in_both_households() {
    for kind in ["artist", "show"] {
        for playing in [true, false] {
            let rig = Rig::new();
            rig.seed();
            let catalog = Catalog::ready();
            let calls = Arc::clone(&catalog.calls);
            let surface = rig.surface(capped(), catalog);
            surface
                .save_scene_from(&Client::Cli, "Music", &definition(kind, playing))
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 0, "saving is not playback");
            let stored = surface.scene(&Client::Cli, "Music").unwrap();
            assert_eq!(
                stored.groups[0].source.uri.as_deref(),
                Some(source_uri(kind))
            );
            let marker = rig.sim.soap_log().len();
            let result = surface.apply_scene(&Client::McpStdio, "Music").unwrap();
            assert!(result.complete && result.changed, "{result:?}");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let writes = rig.writes(marker);
            assert_eq!(
                writes
                    .iter()
                    .filter(|entry| entry.action == "AddURIToQueue")
                    .count(),
                6
            );
            assert_eq!(
                writes.iter().filter(|entry| entry.action == "Play").count(),
                if playing { 2 } else { 0 }
            );
            assert!(
                writes
                    .iter()
                    .all(|entry| entry.action != "RemoveAllTracksFromQueue")
            );
            for room in ["Kitchen", "Den"] {
                let player = rig.sim.player(room).unwrap();
                assert_eq!(rig.queue(room), selection(kind).tracks);
                assert_eq!(
                    get_play_mode(&rig.sim.lan(), player.ip).unwrap(),
                    PlayMode::Normal
                );
                let state = soap::get_transport_info(&rig.sim.lan(), player.ip)
                    .unwrap()
                    .state;
                if playing {
                    assert_eq!(state, TransportState::Playing);
                } else {
                    assert_ne!(state, TransportState::Playing);
                }
                let favorites = browse_all(&rig.sim.lan(), player.ip, "FV:2").unwrap();
                let params = learn_spotify_params(&favorites).unwrap();
                let adds: Vec<_> = writes
                    .iter()
                    .filter(|entry| entry.player == player.uuid && entry.action == "AddURIToQueue")
                    .collect();
                assert_eq!(adds.len(), 3);
                assert!(adds.iter().all(|entry| {
                    let uri = &entry
                        .args
                        .iter()
                        .find(|(key, _)| key == "EnqueuedURI")
                        .unwrap()
                        .1;
                    let metadata = &entry
                        .args
                        .iter()
                        .find(|(key, _)| key == "EnqueuedURIMetaData")
                        .unwrap()
                        .1;
                    if kind == "artist" {
                        metadata.contains(&params.cdudn)
                    } else {
                        uri.starts_with("x-sonos-spotify:spotify%3aepisode%3a")
                            && uri.ends_with(&format!("?sid={}", params.sid))
                            && metadata.contains("object.item.audioItem.musicTrack")
                            && metadata.contains("<res>")
                            && metadata.contains(uri.as_str())
                            && !metadata.contains("<desc")
                            && !metadata.contains("flags=")
                            && !metadata.contains("sn=")
                            && !uri.contains("flags=")
                            && !uri.contains("sn=")
                    }
                }));
            }
            assert!(writes.iter().all(|entry| {
                entry
                    .args
                    .iter()
                    .all(|(_, value)| !value.contains(source_uri(kind)))
            }));
            let marker = rig.sim.soap_log().len();
            let refusal = surface.undo(&Client::McpStdio, true).unwrap_err();
            assert_eq!(refusal.code, ErrorCode::InvalidArgument);
            assert!(refusal.detail.contains("catalog playback"));
            assert!(rig.writes(marker).is_empty());
        }
    }
}

#[test]
fn a_catalog_scene_can_make_a_current_group_member_its_new_coordinator() {
    for kind in ["artist", "show"] {
        let rig = Rig::new();
        rig.seed();
        let old = rig.sim.player("Den").unwrap();
        let future = rig.sim.player("Study").unwrap();
        soap::join_group(&rig.sim.lan(), future.ip, &PlayerId(old.uuid.clone())).unwrap();
        let surface = rig.surface(capped(), Catalog::ready());
        let scene: SceneDefinition = serde_json::from_value(json!({
            "groups": [{"coordinator": "Study", "members": ["Den"],
                        "source": source(kind), "playing": false}],
            "volumes": {"Study": 20, "Den": 15}
        }))
        .unwrap();
        surface
            .save_scene_from(&Client::Cli, "Swap", &scene)
            .unwrap();
        let marker = rig.sim.soap_log().len();
        let result = surface.apply_scene(&Client::McpStdio, "Swap").unwrap();
        assert!(result.complete && result.changed, "{result:?}");
        assert_eq!(rig.queue("Study"), selection(kind).tracks);
        let topology = get_zone_group_state(&rig.sim.lan(), future.ip).unwrap();
        let group = topology
            .groups
            .iter()
            .find(|group| group.coordinator.0 == future.uuid)
            .unwrap();
        assert!(group.members.iter().any(|member| member.uuid.0 == old.uuid));
        assert!(
            rig.writes(marker)
                .iter()
                .all(|entry| entry.action != "Play")
        );
        assert_ne!(
            soap::get_transport_info(&rig.sim.lan(), future.ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }
}

#[test]
fn invalid_catalog_items_and_missing_household_linkage_leave_all_groups_untouched() {
    for kind in ["artist", "show"] {
        for missing_renderer in [false, true] {
            let rig = Rig::new();
            rig.seed();
            let catalog = Catalog {
                invalid: !missing_renderer,
                ..Catalog::ready()
            };
            let surface = rig.surface(capped(), catalog);
            surface
                .save_scene_from(&Client::Cli, "Music", &definition(kind, true))
                .unwrap();
            if missing_renderer {
                rig.sim
                    .retain_favorites(2, |uri| !uri.contains("spotify"))
                    .unwrap();
            }
            let marker = rig.sim.soap_log().len();
            let error = surface.apply_scene(&Client::McpStdio, "Music").unwrap_err();
            assert_eq!(
                error.code,
                if missing_renderer {
                    ErrorCode::RenderParamsMissing
                } else {
                    ErrorCode::NotReady
                }
            );
            assert!(rig.writes(marker).is_empty());
            for room in ["Kitchen", "Office", "Den", "Study"] {
                let host = rig.sim.player(room).unwrap().ip;
                assert_eq!(soap::get_volume(&rig.sim.lan(), host).unwrap(), 85);
                assert_eq!(
                    soap::get_media_info(&rig.sim.lan(), host).unwrap().uri,
                    ORIGINAL
                );
            }
        }
    }
}

#[test]
fn a_queue_edit_during_lookup_preserves_the_edit_and_prevents_caps_or_regrouping() {
    for destination in ["Kitchen", "Office"] {
        let rig = Rig::new();
        rig.seed();
        let kitchen = rig.sim.player("Kitchen").unwrap();
        let office = rig.sim.player("Office").unwrap();
        soap::join_group(&rig.sim.lan(), office.ip, &PlayerId(kitchen.uuid.clone())).unwrap();
        let during = Arc::clone(&rig.sim);
        let target = rig.sim.player(destination).unwrap().ip;
        let catalog = Catalog {
            during_lookup: Some(Box::new(move || {
                soap::add_uri_to_queue(
                    &during.lan(),
                    target,
                    "http://example.invalid/newer.mp3",
                    "",
                    false,
                )
                .unwrap();
            })),
            ..Catalog::ready()
        };
        let surface = rig.surface(capped(), catalog);
        let scene: SceneDefinition = serde_json::from_value(json!({
            "groups": [{"coordinator": destination, "source": source("artist")}],
            "volumes": {(destination): 20},
            "mutes": {(destination): true}
        }))
        .unwrap();
        surface
            .save_scene_from(&Client::Cli, "Music", &scene)
            .unwrap();
        let marker = rig.sim.soap_log().len();
        let error = surface.apply_scene(&Client::McpStdio, "Music").unwrap_err();
        assert!(error.detail.contains("changed"), "{error:?}");
        let writes = rig.writes(marker);
        assert_eq!(
            writes.len(),
            1,
            "only the newer user's edit may write: {writes:?}"
        );
        assert_eq!(writes[0].action, "AddURIToQueue");
        assert_eq!(soap::get_volume(&rig.sim.lan(), target).unwrap(), 85);
        assert!(!soap::get_mute(&rig.sim.lan(), target).unwrap());
        let queue = browse_all(&rig.sim.lan(), target, QUEUE).unwrap();
        assert_eq!(
            queue.last().unwrap().res.as_ref().unwrap().uri,
            "http://example.invalid/newer.mp3"
        );
    }
}

#[test]
fn either_writer_or_caller_can_forbid_catalog_playback_without_blocking_room_levels() {
    for kind in ["artist", "show"] {
        for denied_writer in [true, false] {
            let rig = Rig::new();
            rig.seed();
            let catalog = Catalog::ready();
            catalog.ready.store(false, Ordering::SeqCst);
            let calls = Arc::clone(&catalog.calls);
            let policy = Policy::from_toml("[clients.\"tag:blocked\"]\ndeny = ['play']\n").unwrap();
            let surface = rig.surface(policy, catalog);
            let blocked = Client::Tailnet("tag:blocked".into());
            let writer = if denied_writer {
                &blocked
            } else {
                &Client::Cli
            };
            let caller = if denied_writer {
                &Client::Cli
            } else {
                &blocked
            };
            surface
                .save_scene_from(writer, "Quiet", &definition(kind, false))
                .unwrap();
            let marker = rig.sim.soap_log().len();
            let result = surface.apply_scene(caller, "Quiet").unwrap();
            assert!(result.complete && result.changed, "{result:?}");
            assert!(
                result
                    .notes
                    .iter()
                    .any(|note| note.code == NoteCode::Skipped)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                soap::get_volume(&rig.sim.lan(), rig.sim.player("Den").unwrap().ip).unwrap(),
                20
            );
            assert!(rig.writes(marker).iter().all(|entry| {
                !matches!(
                    entry.action.as_str(),
                    "Play" | "Pause" | "Stop" | "AddURIToQueue"
                )
            }));
        }
    }
}

#[test]
fn a_catalog_scene_uses_policy_caps_reloaded_during_lookup() {
    let rig = Rig::new();
    rig.seed();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("policy.toml");
    let catalog = Catalog {
        during_lookup: Some(Box::new(move || {
            std::fs::write(&path, "[defaults]\nmax_volume = 12\n").unwrap();
        })),
        ..Catalog::ready()
    };
    let surface = rig
        .surface(capped(), catalog)
        .with_policy_file(directory.path().to_path_buf());
    surface
        .save_scene_from(&Client::McpStdio, "Quiet", &definition("artist", true))
        .unwrap();
    let result = surface.apply_scene(&Client::Cli, "Quiet").unwrap();
    assert!(result.complete && result.changed, "{result:?}");
    for room in ["Kitchen", "Office", "Den", "Study"] {
        assert_eq!(
            soap::get_volume(&rig.sim.lan(), rig.sim.player(room).unwrap().ip).unwrap(),
            12
        );
    }
}

#[test]
fn scheduled_catalog_scenes_retry_readiness_before_claiming_and_load_only_once() {
    for kind in ["artist", "show"] {
        let rig = Rig::new();
        rig.seed();
        let catalog = Catalog::ready();
        let ready = Arc::clone(&catalog.ready);
        let surface = Arc::new(rig.surface(capped(), catalog));
        surface
            .save_scene_from(&Client::Cli, "Morning", &definition(kind, false))
            .unwrap();
        let saved = surface
            .add_schedule(
                &Client::Cli,
                &ScheduleRequest {
                    when: "in 1m".into(),
                    action: "apply_scene".into(),
                    scene: Some("Morning".into()),
                    ..ScheduleRequest::default()
                },
            )
            .unwrap();
        let at = DateTime::parse_from_rfc3339(saved.next.as_deref().unwrap())
            .unwrap()
            .to_utc();
        let scheduler = Scheduler::new(&surface);
        ready.store(false, Ordering::SeqCst);
        let marker = rig.sim.soap_log().len();
        assert_eq!(
            scheduler.tick_at(at, &Utc),
            [Ran::Fired { id: saved.id, at }]
        );
        assert!(scheduler.wait(Duration::from_secs(10)));
        assert!(
            surface.schedules(&Client::Cli).unwrap()[0]
                .last_fired
                .is_none()
        );
        assert!(rig.writes(marker).is_empty());
        ready.store(true, Ordering::SeqCst);
        assert_eq!(
            scheduler.tick_at(at + chrono::TimeDelta::minutes(1), &Utc),
            [Ran::Fired { id: saved.id, at }]
        );
        assert!(scheduler.wait(Duration::from_secs(10)));
        assert!(
            surface.schedules(&Client::Cli).unwrap()[0]
                .last_fired
                .is_some()
        );
        assert!(
            scheduler
                .tick_at(at + chrono::TimeDelta::minutes(2), &Utc)
                .is_empty()
        );
        for room in ["Kitchen", "Den"] {
            assert_eq!(rig.queue(room), selection(kind).tracks);
        }
        let writes = rig.writes(marker);
        assert_eq!(
            writes
                .iter()
                .filter(|entry| entry.action == "AddURIToQueue")
                .count(),
            6
        );
        assert!(writes.iter().all(|entry| entry.action != "Play"));
    }
}

struct ChangeDuringVolume {
    lan: SimLan,
    once: AtomicBool,
    target: IpAddr,
    change: &'static str,
}

impl Transport for ChangeDuringVolume {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#SetVolume\"") && !self.once.swap(true, Ordering::SeqCst) {
            if self.change == "queue" {
                soap::add_uri_to_queue(
                    &self.lan,
                    self.target,
                    "http://example.invalid/after-cap.mp3",
                    "",
                    false,
                )?;
            } else {
                soap::set_av_transport_uri(
                    &self.lan,
                    self.target,
                    "http://example.invalid/after-cap.mp3",
                    "",
                )?;
            }
        }
        Ok(answer)
    }
}

#[test]
fn catalog_recall_keeps_its_queue_and_source_proof_through_cap_writes_and_regrouping() {
    for kind in ["artist", "show"] {
        for layout in ["unchanged", "join", "future"] {
            for change in ["queue", "source"] {
                // A future coordinator's source is deliberately changed by
                // its self-join; its independent queue must still be retained.
                if layout == "future" && change == "source" {
                    continue;
                }
                let rig = Rig::new();
                rig.seed();
                let kitchen = rig.sim.player("Kitchen").unwrap();
                let office = rig.sim.player("Office").unwrap();
                let coordinator = if layout == "future" {
                    soap::join_group(&rig.sim.lan(), office.ip, &PlayerId(kitchen.uuid.clone()))
                        .unwrap();
                    "Office"
                } else {
                    "Kitchen"
                };
                let target = rig.sim.player(coordinator).unwrap().ip;
                let members = match layout {
                    "join" => vec!["Office"],
                    "future" => vec!["Kitchen"],
                    _ => Vec::new(),
                };
                let surface = rig.surface_over(
                    capped(),
                    Catalog::ready(),
                    Box::new(ChangeDuringVolume {
                        lan: rig.sim.lan(),
                        once: AtomicBool::new(false),
                        target,
                        change,
                    }),
                );
                let scene: SceneDefinition = serde_json::from_value(json!({
                    "groups": [{"coordinator": coordinator, "members": members,
                                "source": source(kind), "playing": true}],
                    "volumes": {(coordinator): 20}
                }))
                .unwrap();
                surface
                    .save_scene_from(&Client::Cli, "Music", &scene)
                    .unwrap();
                let marker = rig.sim.soap_log().len();
                let result = surface.apply_scene(&Client::McpStdio, "Music").unwrap();
                assert!(!result.complete, "{kind} {layout} {change}: {result:?}");
                assert!(
                    result
                        .failed
                        .iter()
                        .any(|failure| failure.error.contains("changed")),
                    "{result:?}"
                );
                let writes = rig.writes(marker);
                assert!(writes.iter().any(|entry| entry.action == "SetVolume"));
                assert!(
                    writes.iter().all(|entry| {
                        !matches!(entry.action.as_str(), "Play" | "Pause" | "Stop")
                    })
                );
                assert_eq!(
                    writes
                        .iter()
                        .filter(|entry| entry.action == "AddURIToQueue")
                        .count(),
                    usize::from(change == "queue"),
                    "the catalog must not adopt the newer state as its replacement baseline"
                );
                if change == "queue" {
                    let queue = browse_all(&rig.sim.lan(), target, QUEUE).unwrap();
                    assert_eq!(
                        queue.last().unwrap().res.as_ref().unwrap().uri,
                        "http://example.invalid/after-cap.mp3"
                    );
                } else {
                    assert_eq!(
                        soap::get_media_info(&rig.sim.lan(), target).unwrap().uri,
                        "http://example.invalid/after-cap.mp3"
                    );
                }
            }
        }
    }
}
