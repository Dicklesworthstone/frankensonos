//! Explicit selections use the ordinary playback boundary and the existing
//! staged replacement engine over real S1/S2 simulator sockets.

use fsonos_api::surface::queue::{PlayTracksRequest, QueueTrackInput};
use fsonos_api::{ErrorCode, NoteCode, Surface};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIRST: &str = "spotify:track:0000000000000000000001";
const LAST: &str = "spotify:track:0000000000000000000002";
const SELECTED: &str = "spotify:track:1000000000000000000001";
const EPISODE: &str = "spotify:episode:1000000000000000000002";
const EXTERNAL: &str = "x-file-cifs://example.invalid/external.flac";
const RADIO: &str = "x-rincon-mp3radio://example.invalid/radio.mp3";

struct Rig {
    sim: Arc<SimHandle>,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
    lead: IpAddr,
    member: IpAddr,
    guest: IpAddr,
}

impl Rig {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = ["Lead", "Member", "Guest"].map(|name| SimPlayerSpec::new(name, model));
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
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), lead).unwrap());
        let households = vec![house];
        control::queue_spotify_tracks(
            &sim.lan(),
            &households,
            &coordinator,
            &[
                (FIRST, "Original first"),
                (FIRST, "Original repeated occurrence"),
                (LAST, "Original tail"),
            ],
        )
        .unwrap()
        .unwrap();
        control::play_queue_from(&sim.lan(), &households, &coordinator, 2).unwrap();
        soap::seek_position(&sim.lan(), lead, 17).unwrap();
        soap::pause(&sim.lan(), lead).unwrap();
        set_play_mode(&sim.lan(), lead, PlayMode::ShuffleRepeatOne).unwrap();
        for (host, volume) in [(lead, 80), (member, 70), (guest, 60)] {
            soap::set_volume(&sim.lan(), host, volume).unwrap();
        }
        Self {
            sim,
            households,
            coordinator,
            lead,
            member,
            guest,
        }
    }

    fn surface(&self, policy: Policy) -> Surface {
        self.surface_over(Box::new(self.sim.lan()), policy)
    }

    fn surface_over(&self, transport: Box<dyn Transport + Send + Sync>, policy: Policy) -> Surface {
        let households = self.households.clone();
        Surface::new(
            transport,
            Box::new(move |_| Ok(households.clone())),
            policy,
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "play-selection")
    }

    fn queue(&self) -> Vec<(String, String)> {
        browse_all(&self.sim.lan(), self.lead, QUEUE)
            .unwrap()
            .into_iter()
            .map(|item| {
                let uri = item.res.unwrap().uri;
                (
                    spotify_uri_from_renderer_uri(&uri).unwrap_or(uri),
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
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .collect()
    }

    fn assert_original_playback(&self) {
        let position = soap::get_position_info(&self.sim.lan(), self.lead).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(
            spotify_uri_from_renderer_uri(&position.uri).as_deref(),
            Some(FIRST)
        );
        assert_eq!(
            soap::get_transport_info(&self.sim.lan(), self.lead)
                .unwrap()
                .state,
            TransportState::Paused
        );
        assert_eq!(
            get_play_mode(&self.sim.lan(), self.lead).unwrap(),
            PlayMode::ShuffleRepeatOne
        );
    }
}

fn policy() -> Policy {
    Policy::from_toml("[defaults]\nmax_volume = 25\n").unwrap()
}

fn request() -> PlayTracksRequest {
    PlayTracksRequest {
        zone: "Member".into(),
        tracks: vec![
            QueueTrackInput {
                source_uri: SELECTED.into(),
                title: Some("First selected occurrence".into()),
            },
            QueueTrackInput {
                source_uri: "https://open.spotify.com/episode/1000000000000000000002?si=fixture"
                    .into(),
                title: Some("Selected episode".into()),
            },
            QueueTrackInput {
                source_uri: SELECTED.into(),
                title: Some("Repeated selected occurrence".into()),
            },
        ],
        expected_revision: None,
    }
}

fn expected() -> Vec<(String, String)> {
    [
        (SELECTED, "First selected occurrence"),
        (EPISODE, "Selected episode"),
        (SELECTED, "Repeated selected occurrence"),
    ]
    .map(|(uri, title)| (uri.into(), title.into()))
    .to_vec()
}

#[test]
fn selected_mixed_occurrences_start_in_order_with_caps_and_complete_queue_undo() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let original = rig.queue();
        // No Spotify catalog provider or Web API grant is configured.
        let surface = rig.surface(policy());
        let mut asked = request();
        asked.expected_revision = surface
            .queue(&Client::Cli, "Member", 0, 1)
            .unwrap()
            .revision;
        let start = rig.sim.soap_log().len();
        let outcome = surface.play_tracks(&Client::McpStdio, &asked).unwrap();
        assert!(outcome.changed && outcome.done.contains("3 selected tracks"));
        assert_eq!(
            outcome
                .notes
                .iter()
                .filter(|note| note.code == NoteCode::VolumeClamped)
                .count(),
            2
        );
        assert_eq!(rig.queue(), expected());
        assert_eq!(rig.volumes(), [25, 25, 60]);
        assert_eq!(
            get_play_mode(&rig.sim.lan(), rig.lead).unwrap(),
            PlayMode::Normal
        );
        let position = soap::get_position_info(&rig.sim.lan(), rig.lead).unwrap();
        assert_eq!(position.track, 1);
        assert_eq!(
            spotify_uri_from_renderer_uri(&position.uri).as_deref(),
            Some(SELECTED)
        );
        assert_eq!(
            soap::get_transport_info(&rig.sim.lan(), rig.lead)
                .unwrap()
                .state,
            TransportState::Playing
        );
        let writes = rig.writes_since(start);
        assert!(
            writes
                .iter()
                .all(|entry| entry.action != "RemoveAllTracksFromQueue")
        );
        let removed = writes
            .iter()
            .position(|entry| entry.action == "RemoveTrackRangeFromQueue")
            .unwrap();
        assert_eq!(
            writes[..removed]
                .iter()
                .filter(|entry| entry.action == "AddURIToQueue")
                .count(),
            3
        );
        assert!(
            writes
                .iter()
                .filter(|entry| entry.action != "SetVolume")
                .all(|entry| entry.player == rig.coordinator.0)
        );
        assert!(
            browse_all(&rig.sim.lan(), rig.member, QUEUE)
                .unwrap()
                .is_empty()
        );
        assert!(
            browse_all(&rig.sim.lan(), rig.guest, QUEUE)
                .unwrap()
                .is_empty()
        );
        let heard = surface
            .recent_plays(&Client::McpStdio, Some("Member"), 10)
            .unwrap();
        assert_eq!(heard.len(), 1);
        assert_eq!(heard[0].source_uri, SELECTED);
        let actions = surface
            .recent_actions(&Client::McpStdio, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(
            actions[0]
                .action
                .intent
                .starts_with("play: play 3 selected tracks")
        );
        let saved = BeforeState::parse(actions[0].action.before_state.as_deref().unwrap()).unwrap();
        assert_eq!(saved.queue_undo.len(), 1);
        let preview = surface.preview_undo(&Client::McpStdio).unwrap().unwrap();
        assert!(preview.undoable);
        let undone = surface.undo_checked(&Client::McpStdio, preview.id).unwrap();
        assert_eq!(undone.undone, Some(preview.id));
        assert!(undone.failures.is_empty(), "{undone:?}");
        assert_eq!(rig.queue(), original);
        rig.assert_original_playback();
        assert_eq!(
            rig.volumes(),
            [25, 25, 60],
            "undo still respects current policy"
        );
    }
}

#[test]
fn direct_sources_and_empty_queues_start_without_claiming_unavailable_queue_undo() {
    for generation in [1, 2] {
        for radio in [false, true] {
            let rig = Rig::new(generation);
            if radio {
                control::play_uri(&rig.sim.lan(), &rig.households, &rig.coordinator, RADIO, "")
                    .unwrap();
            } else {
                soap::remove_all_tracks_from_queue(&rig.sim.lan(), rig.lead).unwrap();
            }
            let surface = rig.surface(policy());
            assert!(
                surface
                    .play_tracks(&Client::McpStdio, &request())
                    .unwrap()
                    .changed
            );
            assert_eq!(rig.queue(), expected());
            assert_eq!(rig.volumes(), [25, 25, 60]);
            assert!(
                !surface
                    .preview_undo(&Client::McpStdio)
                    .unwrap()
                    .unwrap()
                    .undoable
            );
            assert!(
                surface
                    .undo(&Client::McpStdio, false)
                    .unwrap_err()
                    .detail
                    .contains("selected tracks queue replacement")
            );
        }
    }
}

#[test]
fn playback_permission_is_required_and_invalid_selections_never_contact_speakers() {
    let rig = Rig::new(2);
    let surface = rig.surface(
        Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = ['add_to_queue', 'play_tracks']\n")
            .unwrap(),
    );
    let marker = rig.sim.soap_log().len();
    assert_eq!(
        surface
            .play_tracks(&Client::McpStdio, &request())
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    for invalid in [
        "spotify:track:short",
        "spotify:album:1000000000000000000001",
        "https://example.invalid/stream",
    ] {
        let mut asked = request();
        asked.tracks[1].source_uri = invalid.into();
        assert_eq!(
            surface.play_tracks(&Client::Cli, &asked).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(rig.sim.soap_log().len(), marker);
    let allowed =
        rig.surface(Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = ['play']\n").unwrap());
    assert!(
        allowed
            .play_tracks(&Client::McpStdio, &request())
            .unwrap()
            .changed
    );
    assert_eq!(rig.queue(), expected());
}

#[test]
fn stale_queue_revisions_and_missing_household_renderer_leave_music_and_levels_intact() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let surface = rig.surface(policy());
        let mut asked = request();
        asked.expected_revision = surface
            .queue(&Client::Cli, "Member", 0, 1)
            .unwrap()
            .revision;
        soap::add_uri_to_queue(&rig.sim.lan(), rig.lead, EXTERNAL, "", false).unwrap();
        let original = rig.queue();
        let marker = rig.sim.soap_log().len();
        assert_eq!(
            surface
                .play_tracks(&Client::McpStdio, &asked)
                .unwrap_err()
                .code,
            ErrorCode::QueueChanged
        );
        assert!(rig.writes_since(marker).is_empty());
        rig.sim
            .retain_favorites(generation, |uri| uri.starts_with("x-rincon-mp3radio:"))
            .unwrap();
        let marker = rig.sim.soap_log().len();
        assert_eq!(
            surface
                .play_tracks(&Client::McpStdio, &request())
                .unwrap_err()
                .code,
            ErrorCode::RenderParamsMissing
        );
        assert!(rig.writes_since(marker).is_empty());
        assert_eq!(rig.queue(), original);
        assert_eq!(rig.volumes(), [80, 70, 60]);
        rig.assert_original_playback();
    }
}

#[derive(Clone, Copy)]
enum Drift {
    Queue,
    Source,
    Group,
    Boot,
}

struct Wire {
    sim: Arc<SimHandle>,
    lan: SimLan,
    drift: Option<Drift>,
    edited: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    refuse_second: bool,
    adds: Arc<AtomicUsize>,
    writes: Arc<Mutex<Vec<String>>>,
}

impl Wire {
    fn new(rig: &Rig) -> Self {
        Self {
            sim: Arc::clone(&rig.sim),
            lan: rig.sim.lan(),
            drift: None,
            edited: Arc::new(AtomicBool::new(false)),
            lost: Arc::new(AtomicBool::new(false)),
            refuse_second: false,
            adds: Arc::new(AtomicUsize::new(0)),
            writes: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Transport for Wire {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.lock().unwrap().push(name.into());
        }
        if name == "AddURIToQueue" {
            let add = self.adds.fetch_add(1, Ordering::SeqCst);
            if self.refuse_second && add == 1 {
                return Err(ProtoError::SoapFault {
                    code: 701,
                    reason: "injected second-item refusal".into(),
                });
            }
        }
        let answer = self.lan.soap_post(host, path, action, body)?;
        if name == "Browse"
            && body.contains("<ObjectID>FV:2</ObjectID>")
            && let Some(drift) = self.drift
            && !self.edited.swap(true, Ordering::SeqCst)
        {
            match drift {
                Drift::Queue => {
                    soap::add_uri_to_queue(&self.lan, host, EXTERNAL, "", false)?;
                }
                Drift::Source => soap::set_av_transport_uri(&self.lan, host, RADIO, "")?,
                Drift::Group => soap::join_group(
                    &self.lan,
                    self.sim.player("Guest").unwrap().ip,
                    &PlayerId(self.sim.player("Lead").unwrap().uuid.clone()),
                )?,
                Drift::Boot => self.sim.reboot("Lead", Duration::ZERO).unwrap(),
            }
        }
        if name == "AddURIToQueue" && self.lost.load(Ordering::SeqCst) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost acknowledgement".into(),
            });
        }
        Ok(answer)
    }
}

#[test]
fn renderer_learning_cannot_replace_the_first_queue_source_group_or_boot_proof() {
    for generation in [1, 2] {
        for drift in [Drift::Queue, Drift::Source, Drift::Group, Drift::Boot] {
            let rig = Rig::new(generation);
            let mut wire = Wire::new(&rig);
            wire.drift = Some(drift);
            let edited = Arc::clone(&wire.edited);
            let writes = Arc::clone(&wire.writes);
            let surface = rig.surface_over(Box::new(wire), policy());
            let error = surface
                .play_tracks(&Client::McpStdio, &request())
                .unwrap_err();
            assert_ne!(error.code, ErrorCode::OutcomeUncertain);
            assert!(edited.load(Ordering::SeqCst));
            assert!(
                writes.lock().unwrap().is_empty(),
                "no cap or playback mutation may follow drift"
            );
            assert_eq!(rig.volumes(), [80, 70, 60]);
        }
    }
}

#[test]
fn lost_add_replies_are_observed_once_and_partial_selections_never_replace_the_old_queue() {
    for generation in [1, 2] {
        for refuse_second in [false, true] {
            let rig = Rig::new(generation);
            let original = rig.queue();
            let mut wire = Wire::new(&rig);
            wire.lost.store(true, Ordering::SeqCst);
            wire.refuse_second = refuse_second;
            let lost = Arc::clone(&wire.lost);
            let adds = Arc::clone(&wire.adds);
            let writes = Arc::clone(&wire.writes);
            let surface = rig.surface_over(Box::new(wire), policy());
            let result = surface.play_tracks(&Client::McpStdio, &request());
            if refuse_second {
                assert_eq!(result.unwrap_err().code, ErrorCode::OutcomeUncertain);
                assert_eq!(adds.load(Ordering::SeqCst), 2);
                let mut partial = original.clone();
                partial.push(expected()[0].clone());
                assert_eq!(rig.queue(), partial);
                assert!(
                    writes
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|name| name == "SetVolume" || name == "AddURIToQueue")
                );
                rig.assert_original_playback();
                assert!(
                    !surface
                        .preview_undo(&Client::McpStdio)
                        .unwrap()
                        .unwrap()
                        .undoable
                );
            } else {
                assert!(result.unwrap().changed);
                assert_eq!(adds.load(Ordering::SeqCst), 3);
                assert_eq!(rig.queue(), expected());
                lost.store(false, Ordering::SeqCst);
                let preview = surface.preview_undo(&Client::McpStdio).unwrap().unwrap();
                assert!(preview.undoable);
                assert!(
                    surface
                        .undo_checked(&Client::McpStdio, preview.id)
                        .unwrap()
                        .failures
                        .is_empty()
                );
                assert_eq!(rig.queue(), original);
                rig.assert_original_playback();
            }
        }
    }
}
