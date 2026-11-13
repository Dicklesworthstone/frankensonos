//! Source playback reaches each selected group once, with household-local
//! metadata, all-target preparation and one action/undo boundary. Speaker I/O
//! uses real S1/S2 simulator sockets; only Spotify's catalog is supplied here.

use fsonos_api::plan::plan_play_set;
use fsonos_api::source::{SourceSelection, SourceSelectionKind, SpotifySources};
use fsonos_api::surface::sonos_playlists::PlaySonosPlaylistRequest;
use fsonos_api::{ErrorCode, Failure, NoteCode, PlayFavoriteRequest, PlayRequest, Surface};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::rooms::ControlTarget;
use fsonos_core::store::{ActionFilter, LoggedAction, MemStore};
use fsonos_core::{HouseholdState, favorites, inventory, playlists, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const ORIGINAL: [&str; 3] = [
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/last.flac",
];
const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/selected.mp3";
const EXTERNAL: &str = "x-file-cifs://example.invalid/another-controller.flac";
const ARTIST: &str = "spotify:artist:2000000000000000000001";
const SHOW: &str = "spotify:show:3000000000000000000001";
const TRACK: &str = "spotify:track:1000000000000000000001";
const GROUPS: [&str; 4] = ["Lead@S1", "Guest@S1", "Lead@S2", "Guest@S2"];
const CAP: u8 = 25;

struct House {
    sim: Arc<SimHandle>,
    households: Vec<HouseholdState>,
    directory: tempfile::TempDir,
}

impl House {
    fn new() -> Self {
        let sim = Arc::new(
            SimHousehold::builder()
                .s1(["Lead", "Member", "Guest"]
                    .map(|name| SimPlayerSpec::new(name, SimModel::Play5Gen1)))
                .s2(["Lead", "Member", "Guest"].map(|name| SimPlayerSpec::new(name, SimModel::One)))
                .spawn()
                .unwrap(),
        );
        for generation in [1, 2] {
            let player = |name| {
                sim.players()
                    .iter()
                    .find(|player| player.generation == generation && player.room == name)
                    .unwrap()
            };
            soap::join_group(
                &sim.lan(),
                player("Member").ip,
                &PlayerId(player("Lead").uuid.clone()),
            )
            .unwrap();
        }
        let households = inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("aliases.toml"),
            "[aliases]\nselected = [\"Member@S1\", \"Lead@S1\", \"Member@S2\", \"Lead@S2\"]\n",
        )
        .unwrap();
        let house = Self {
            sim,
            households,
            directory,
        };
        for zone in GROUPS {
            let target = house.target(zone);
            for uri in ORIGINAL {
                soap::add_uri_to_queue(&house.sim.lan(), target.coordinator.ip, uri, "", false)
                    .unwrap();
            }
            soap::play_from_queue(
                &house.sim.lan(),
                target.coordinator.ip,
                &target.coordinator.id,
            )
            .unwrap();
            soap::seek_track(&house.sim.lan(), target.coordinator.ip, 2).unwrap();
            soap::seek_position(&house.sim.lan(), target.coordinator.ip, 17).unwrap();
            soap::play(&house.sim.lan(), target.coordinator.ip).unwrap();
            soap::pause(&house.sim.lan(), target.coordinator.ip).unwrap();
            set_play_mode(
                &house.sim.lan(),
                target.coordinator.ip,
                PlayMode::ShuffleRepeatOne,
            )
            .unwrap();
        }
        for player in house.sim.players() {
            soap::set_volume(&house.sim.lan(), player.ip, 80).unwrap();
        }
        house
    }

    fn target(&self, zone: &str) -> ControlTarget<'_> {
        resolve_room(&self.households, zone).unwrap()
    }

    fn surface(&self) -> Surface {
        self.surface_over(Box::new(self.sim.lan()))
    }

    fn surface_over(&self, transport: Box<dyn Transport + Send + Sync>) -> Surface {
        let known = self.households.clone();
        let mut policy = Policy::default();
        policy.defaults.max_volume = CAP;
        Surface::new(
            transport,
            Box::new(move |_| Ok(known.clone())),
            policy,
            Box::new(SystemClock),
        )
        .with_aliases_file(self.directory.path().join("aliases.toml"))
        .with_action_log(Box::new(MemStore::default()), "http")
    }

    fn queue(&self, zone: &str) -> Vec<String> {
        browse_all(&self.sim.lan(), self.target(zone).coordinator.ip, QUEUE)
            .unwrap()
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn native_source(&self, zone: &str, saved_playlist: bool) -> favorites::Favorite {
        let target = self.target(zone);
        let listed = if saved_playlist {
            playlists::list(&self.sim.lan(), &self.households, &target.coordinator.id)
        } else {
            favorites::list(&self.sim.lan(), &self.households, &target.coordinator.id)
        }
        .unwrap();
        favorites::find(&listed, if saved_playlist { "SQ:7" } else { "Nocturne" })
            .unwrap()
            .clone()
    }

    fn writes_since(&self, start: usize) -> Vec<SoapLogEntry> {
        self.sim
            .soap_log()
            .into_iter()
            .skip(start)
            .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
            .collect()
    }

    fn original_playback(&self, zone: &str) {
        let target = self.target(zone);
        let position = soap::get_position_info(&self.sim.lan(), target.coordinator.ip).unwrap();
        assert_eq!(self.queue(zone), ORIGINAL, "{zone}");
        assert_eq!(
            (position.track, position.position_secs),
            (2, Some(17)),
            "{zone}"
        );
        assert_eq!(position.uri, ORIGINAL[1], "{zone}");
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), target.coordinator.ip)
                .unwrap()
                .state,
            TransportState::Paused,
            "{zone}"
        );
        assert_eq!(
            get_play_mode(&self.sim.lan(), target.coordinator.ip).unwrap(),
            PlayMode::ShuffleRepeatOne,
            "{zone}"
        );
    }

    fn untouched(&self, start: usize) {
        assert!(self.writes_since(start).is_empty());
        for zone in GROUPS {
            self.original_playback(zone);
        }
        for player in self.sim.players() {
            assert_eq!(soap::get_volume(&self.sim.lan(), player.ip).unwrap(), 80);
        }
    }
}

fn request(zone: &str, source: &str) -> PlayRequest {
    PlayRequest {
        zone: zone.into(),
        source_uri: source.into(),
        title: None,
    }
}

fn one_action(surface: &Surface) -> LoggedAction {
    let mut logged = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(logged.len(), 1, "one target set is one action");
    logged.pop().unwrap()
}

fn undo(surface: &Surface) {
    let preview = surface.preview_undo(&Client::McpStdio).unwrap().unwrap();
    let restored = surface.undo_checked(&Client::McpStdio, preview.id).unwrap();
    assert!(restored.failures.is_empty(), "{restored:?}");
}

fn argument<'a>(call: &'a SoapLogEntry, name: &str) -> &'a str {
    &call.args.iter().find(|(key, _)| key == name).unwrap().1
}

#[test]
fn uri_alias_deduplicates_grouped_rooms_caps_only_its_audience_and_undoes_together() {
    let house = House::new();
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let result = surface
        .control_set(&Client::McpStdio, "play", |rooms| {
            plan_play_set(rooms, &request("selected", STREAM))
        })
        .unwrap();
    assert!(result.changed);
    let writes = house.writes_since(start);
    for action in ["SetAVTransportURI", "Play"] {
        let calls: Vec<_> = writes.iter().filter(|call| call.action == action).collect();
        assert_eq!(calls.len(), 2, "one {action} per selected group");
        for zone in ["Lead@S1", "Lead@S2"] {
            assert!(
                calls
                    .iter()
                    .any(|call| call.player == house.target(zone).coordinator.id.0)
            );
        }
    }
    for generation in [1, 2] {
        for name in ["Lead", "Member", "Guest"] {
            let zone = format!("{name}@S{generation}");
            let target = house.target(&zone);
            let expected = if name == "Guest" { 80 } else { CAP };
            assert_eq!(
                soap::get_volume(&house.sim.lan(), target.player.ip).unwrap(),
                expected
            );
            assert_eq!(
                house.queue(&zone),
                ORIGINAL,
                "URI play keeps native queue entries"
            );
        }
        house.original_playback(&format!("Guest@S{generation}"));
    }
    let logged = one_action(&surface);
    let before = BeforeState::parse(logged.action.before_state.as_deref().unwrap()).unwrap();
    assert_eq!(before.zones.len(), 2);
    undo(&surface);
    for zone in GROUPS {
        house.original_playback(zone);
    }
}

#[test]
fn favorites_and_native_playlist_ids_use_each_households_metadata_once_per_group() {
    for saved_playlist in [false, true] {
        let house = House::new();
        let surface = house.surface();
        let selected: Vec<_> = GROUPS
            .iter()
            .map(|zone| house.native_source(zone, saved_playlist))
            .collect();
        assert_ne!(selected[0].metadata, selected[2].metadata);
        let start = house.sim.soap_log().len();
        let result = if saved_playlist {
            surface.play_sonos_playlist(
                &Client::McpStdio,
                &PlaySonosPlaylistRequest {
                    zone: "everywhere".into(),
                    playlist: "SQ:7".into(),
                },
            )
        } else {
            surface.play_favorite(
                &Client::McpStdio,
                &PlayFavoriteRequest {
                    zone: "everywhere".into(),
                    favorite: "Nocturne".into(),
                },
            )
        }
        .unwrap();
        assert!(result.changed);
        let action = if saved_playlist {
            "AddURIToQueue"
        } else {
            "SetAVTransportURI"
        };
        let writes = house.writes_since(start);
        for (zone, favorite) in GROUPS.iter().zip(&selected) {
            let target = house.target(zone);
            let calls: Vec<_> = writes
                .iter()
                .filter(|call| call.player == target.coordinator.id.0 && call.action == action)
                .collect();
            assert_eq!(
                calls.len(),
                1,
                "{zone}: {action} duplicated for group members"
            );
            let (uri, metadata) = if saved_playlist {
                ("EnqueuedURI", "EnqueuedURIMetaData")
            } else {
                ("CurrentURI", "CurrentURIMetaData")
            };
            assert_eq!(argument(calls[0], uri), favorite.uri.as_deref().unwrap());
            assert_eq!(argument(calls[0], metadata), favorite.metadata);
            assert_eq!(
                soap::get_transport_info(&house.sim.lan(), target.coordinator.ip)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
            if saved_playlist {
                let generation = if zone.ends_with("S1") { "S1" } else { "S2" };
                let queue = house.queue(zone);
                assert_eq!(queue.len(), 3);
                assert!(
                    queue
                        .iter()
                        .all(|uri| uri.contains(&format!("0Sim{generation}")))
                );
                assert_eq!(
                    queue[0], queue[2],
                    "native repeated occurrences remain ordered"
                );
            } else {
                assert_eq!(house.queue(zone), ORIGINAL);
            }
        }
        let catalog = if saved_playlist { "SQ:" } else { "FV:2" };
        assert_eq!(
            house.sim.soap_log()[start..]
                .iter()
                .filter(|call| { call.action == "Browse" && argument(call, "ObjectID") == catalog })
                .count(),
            2,
            "one native catalog observation per household, not per room/group"
        );
        let logged = one_action(&surface);
        let before = BeforeState::parse(logged.action.before_state.as_deref().unwrap()).unwrap();
        assert_eq!(before.zones.len(), 4);
        assert_eq!(before.queue_undo.len(), if saved_playlist { 4 } else { 0 });
        undo(&surface);
        for zone in GROUPS {
            house.original_playback(zone);
        }
    }
}

#[test]
fn a_native_selection_missing_in_one_household_leaves_every_group_untouched() {
    for saved_playlist in [false, true] {
        let house = House::new();
        if !saved_playlist {
            house
                .sim
                .retain_favorites(2, |uri| !uri.contains("TrackA"))
                .unwrap();
        }
        let surface = house.surface();
        let start = house.sim.soap_log().len();
        let failure = if saved_playlist {
            surface.play_sonos_playlist(
                &Client::McpStdio,
                &PlaySonosPlaylistRequest {
                    zone: "everywhere".into(),
                    playlist: "Saved Mix S1".into(),
                },
            )
        } else {
            surface.play_favorite(
                &Client::McpStdio,
                &PlayFavoriteRequest {
                    zone: "everywhere".into(),
                    favorite: "Nocturne".into(),
                },
            )
        }
        .unwrap_err();
        assert_eq!(
            failure.code,
            if saved_playlist {
                ErrorCode::NoMatch
            } else {
                ErrorCode::UnknownFavorite
            }
        );
        house.untouched(start);
        assert!(
            surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn missing_spotify_linkage_in_one_household_prevents_caps_and_play_everywhere() {
    let house = House::new();
    house
        .sim
        .retain_favorites(2, |uri| uri.starts_with("x-rincon-mp3radio:"))
        .unwrap();
    let surface = house.surface();
    let start = house.sim.soap_log().len();
    let failure = surface
        .control_set(&Client::McpStdio, "play", |rooms| {
            plan_play_set(rooms, &request("everywhere", TRACK))
        })
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::RenderParamsMissing);
    house.untouched(start);
}

struct RegroupDuringCatalog {
    lan: SimLan,
    household: Vec<IpAddr>,
    guest: IpAddr,
    lead: PlayerId,
    catalog: &'static str,
    changed: AtomicBool,
}

impl Transport for RegroupDuringCatalog {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if self.household.contains(&host)
            && action.ends_with("#Browse\"")
            && body.contains(&format!("<ObjectID>{}</ObjectID>", self.catalog))
            && !self.changed.swap(true, Ordering::SeqCst)
        {
            soap::join_group(&self.lan, self.guest, &self.lead)?;
        }
        Ok(response)
    }
}

#[test]
fn a_group_merge_during_native_catalog_reads_cannot_duplicate_play_or_use_stale_caps() {
    for saved_playlist in [false, true] {
        let house = House::new();
        let wire = RegroupDuringCatalog {
            lan: house.sim.lan(),
            household: house
                .sim
                .players()
                .iter()
                .filter(|player| player.generation == 2)
                .map(|player| player.ip)
                .collect(),
            guest: house.target("Guest@S2").player.ip,
            lead: house.target("Lead@S2").coordinator.id.clone(),
            catalog: if saved_playlist { "SQ:" } else { "FV:2" },
            changed: AtomicBool::new(false),
        };
        let surface = house.surface_over(Box::new(wire));
        let start = house.sim.soap_log().len();
        let failure = if saved_playlist {
            surface.play_sonos_playlist(
                &Client::McpStdio,
                &PlaySonosPlaylistRequest {
                    zone: "everywhere".into(),
                    playlist: "SQ:7".into(),
                },
            )
        } else {
            surface.play_favorite(
                &Client::McpStdio,
                &PlayFavoriteRequest {
                    zone: "everywhere".into(),
                    favorite: "Nocturne".into(),
                },
            )
        }
        .unwrap_err();
        assert_eq!(failure.code, ErrorCode::QueueChanged);
        let writes = house.writes_since(start);
        assert_eq!(
            writes.len(),
            1,
            "only the external grouping may write: {writes:?}"
        );
        assert_eq!(writes[0].action, "SetAVTransportURI");
        assert_eq!(writes[0].player, house.target("Guest@S2").player.id.0);
        assert_eq!(
            argument(&writes[0], "CurrentURI"),
            format!("x-rincon:{}", house.target("Lead@S2").coordinator.id.0)
        );
        for player in house.sim.players() {
            assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 80);
        }
        for zone in ["Lead@S1", "Guest@S1", "Lead@S2"] {
            house.original_playback(zone);
        }
        assert!(
            surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap()
                .is_empty()
        );
    }
}

struct Catalog {
    calls: Arc<AtomicUsize>,
    during_lookup: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Catalog {
    fn read(&self, kind: SourceSelectionKind) -> SourceSelection {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(change) = &self.during_lookup {
            change();
        }
        let source = if kind == SourceSelectionKind::ShowEpisodes {
            "episode"
        } else {
            "track"
        };
        SourceSelection {
            tracks: (1..=3)
                .map(|index| {
                    (
                        format!("spotify:{source}:100000000000000000000{index}"),
                        format!("Selection {index}"),
                    )
                })
                .collect(),
            kind,
        }
    }
}

impl SpotifySources for Catalog {
    fn artist_tracks(&self, uri: &str) -> Result<SourceSelection, Failure> {
        assert_eq!(uri, ARTIST);
        Ok(self.read(SourceSelectionKind::TopTracks))
    }

    fn show_episodes(&self, uri: &str) -> Result<SourceSelection, Failure> {
        assert_eq!(uri, SHOW);
        Ok(self.read(SourceSelectionKind::ShowEpisodes))
    }
}

#[test]
fn artist_and_show_playback_share_one_catalog_observation_and_restore_all_original_queues() {
    for source in [ARTIST, SHOW] {
        let house = House::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let surface = house.surface().with_spotify_sources(Box::new(Catalog {
            calls: Arc::clone(&calls),
            during_lookup: None,
        }));
        let result = surface
            .control_set(&Client::McpStdio, "play", |rooms| {
                plan_play_set(rooms, &request("everywhere", source))
            })
            .unwrap();
        assert!(result.changed);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the selected music must be one catalog observation"
        );
        let kind = if source == SHOW { "episode" } else { "track" };
        let expected: Vec<_> = (1..=3)
            .map(|index| format!("spotify:{kind}:100000000000000000000{index}"))
            .collect();
        for zone in GROUPS {
            let target = house.target(zone);
            let selected: Vec<_> = house
                .queue(zone)
                .iter()
                .map(|uri| spotify_uri_from_renderer_uri(uri).unwrap())
                .collect();
            assert_eq!(selected, expected, "{zone}");
            assert_eq!(
                get_play_mode(&house.sim.lan(), target.coordinator.ip).unwrap(),
                PlayMode::Normal
            );
            let position =
                soap::get_position_info(&house.sim.lan(), target.coordinator.ip).unwrap();
            assert_eq!(position.track, 1);
            assert_eq!(
                soap::get_transport_info(&house.sim.lan(), target.coordinator.ip)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
        }
        let logged = one_action(&surface);
        let before = BeforeState::parse(logged.action.before_state.as_deref().unwrap()).unwrap();
        assert_eq!(before.queue_undo.len(), 4);
        undo(&surface);
        for zone in GROUPS {
            house.original_playback(zone);
        }
    }
}

#[test]
fn every_catalog_destination_is_bound_before_lookup_and_any_changed_target_stops_all_writes() {
    for change in ["queue", "source", "group"] {
        let house = House::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let sim = Arc::clone(&house.sim);
        let guest = house.target("Guest@S2").player.ip;
        let lead = house.target("Lead@S2").coordinator.id.clone();
        let surface = house.surface().with_spotify_sources(Box::new(Catalog {
            calls: Arc::clone(&calls),
            during_lookup: Some(Box::new(move || match change {
                "queue" => {
                    soap::add_uri_to_queue(&sim.lan(), guest, EXTERNAL, "", false).unwrap();
                }
                "source" => {
                    soap::set_av_transport_uri(&sim.lan(), guest, EXTERNAL, "").unwrap();
                }
                "group" => {
                    soap::join_group(&sim.lan(), guest, &lead).unwrap();
                }
                _ => unreachable!(),
            })),
        }));
        let start = house.sim.soap_log().len();
        surface
            .control_set(&Client::McpStdio, "play", |rooms| {
                plan_play_set(rooms, &request("everywhere", ARTIST))
            })
            .expect_err("a changed target cannot become a new replacement baseline");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let writes = house.writes_since(start);
        assert_eq!(
            writes.len(),
            1,
            "only the external {change} change may have written: {writes:?}"
        );
        for player in house.sim.players() {
            assert_eq!(soap::get_volume(&house.sim.lan(), player.ip).unwrap(), 80);
        }
        for zone in ["Lead@S1", "Guest@S1"] {
            house.original_playback(zone);
        }
        assert!(
            surface
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn a_late_speaker_refusal_reports_partial_delivery_and_keeps_one_reversible_action() {
    let house = House::new();
    let surface = house.surface();
    let refused = house.target("Guest@S2").coordinator.id.clone();
    house
        .sim
        .upnp_fault(&refused.0, "SetAVTransportURI", 701)
        .unwrap();
    let start = house.sim.soap_log().len();
    let result = surface
        .control_set(&Client::McpStdio, "play", |rooms| {
            plan_play_set(rooms, &request("everywhere", STREAM))
        })
        .unwrap();
    assert!(result.changed);
    assert_eq!(
        result
            .notes
            .iter()
            .filter(|note| note.code == NoteCode::Skipped)
            .count(),
        1
    );
    let writes = house.writes_since(start);
    assert_eq!(
        writes
            .iter()
            .filter(|call| call.action == "SetAVTransportURI")
            .count(),
        4
    );
    assert_eq!(
        writes.iter().filter(|call| call.action == "Play").count(),
        3
    );
    for zone in GROUPS {
        let target = house.target(zone);
        let media = soap::get_media_info(&house.sim.lan(), target.coordinator.ip).unwrap();
        if target.coordinator.id == refused {
            house.original_playback(zone);
        } else {
            assert_eq!(media.uri, STREAM);
        }
    }
    one_action(&surface);
    house.sim.clear_faults(&refused.0).unwrap();
    undo(&surface);
    for zone in GROUPS {
        house.original_playback(zone);
    }
}

#[test]
fn a_partial_container_replacement_does_not_offer_an_undo_that_omits_a_group() {
    let house = House::new();
    let surface = house.surface();
    let refused = house.target("Guest@S2").coordinator.id.clone();
    house
        .sim
        .upnp_fault(&refused.0, "AddURIToQueue", 701)
        .unwrap();
    let result = surface
        .play_sonos_playlist(
            &Client::McpStdio,
            &PlaySonosPlaylistRequest {
                zone: "everywhere".into(),
                playlist: "SQ:7".into(),
            },
        )
        .unwrap();
    assert!(result.changed);
    assert_eq!(
        result
            .notes
            .iter()
            .filter(|note| note.code == NoteCode::Skipped)
            .count(),
        1
    );
    let logged = one_action(&surface);
    let before = BeforeState::parse(logged.action.before_state.as_deref().unwrap()).unwrap();
    assert!(
        before.not_undoable.is_some(),
        "partial replacement cannot claim complete restoration"
    );
    assert!(surface.preview_undo(&Client::McpStdio).is_err());
    house.original_playback("Guest@S2");
    for zone in ["Lead@S1", "Guest@S1", "Lead@S2"] {
        assert_ne!(house.queue(zone), ORIGINAL);
    }
}
