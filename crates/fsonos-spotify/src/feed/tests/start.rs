//! Repeated starts keep the current owned movement and its elapsed time.
//! S1 and S2 use the real simulator; owner interventions bypass the observed
//! transport so the assertions count only the DJ's requested mutations.

use std::cell::RefCell;

use super::*;
use crate::steer::DjConstraints;
use fsonos_proto::control as transport;

fn writes(rig: &Rig, from: usize) -> Vec<String> {
    rig.sim.soap_log()[from..]
        .iter()
        .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
        .map(|call| call.action.clone())
        .collect()
}

fn select(rig: &Rig, position: u32, elapsed: u32, paused: bool) {
    control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, position).unwrap();
    transport::seek_position(&rig.lan, rig.host(), elapsed).unwrap();
    if paused {
        transport::pause(&rig.lan, rig.host()).unwrap();
    }
}

fn assert_selected(rig: &Rig, position: u32, elapsed: u32) {
    let live = control::playback(&rig.lan, &rig.houses, &rig.coordinator).unwrap();
    assert_eq!(live.position.track, position);
    assert_eq!(live.position.position_secs, Some(elapsed));
    assert_eq!(live.transport.state, TransportState::Playing);
}

#[test]
fn a_repeated_start_keeps_a_playing_or_paused_middle_movement() {
    for s2 in [false, true] {
        for paused in [false, true] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let retained = feed.queued().to_vec();
            let current = retained[0].movements[1].position.unwrap();
            select(&rig, current, 37, paused);
            let queue = rig.queue();
            let before = rig.sim.soap_log().len();
            feed.start(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 40),
                &mut store,
            )
            .unwrap();
            assert_selected(&rig, current, 37);
            assert_eq!(rig.queue(), queue);
            assert_eq!(feed.queued(), retained);
            assert_eq!(feed.playing, Some(current));
            assert!(!feed.accepted_playback());
            assert!(
                plays(&store).is_empty(),
                "starting is not a listening event"
            );
            assert_eq!(writes(&rig, before), ["SetPlayMode", "Play"]);
        }
    }
}

#[test]
fn a_restored_feed_can_resume_without_new_music_or_rewinding() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let current = feed.queued()[0].movements[1].position.unwrap();
        select(&rig, current, 29, true);
        let mut feed = QueueFeed::restore(feed.snapshot().unwrap(), DjConfig::default(), 11);
        let empty = WorkPool::default();
        let retained = feed.snapshot();
        let before = rig.sim.soap_log().len();
        feed.start_ready(&rig.speakers(&rig.lan), plan(&empty, MIDNIGHT + 30), &store)
            .unwrap();
        assert_eq!(feed.snapshot(), retained);
        assert!(writes(&rig, before).is_empty());
        let before = rig.sim.soap_log().len();
        feed.start(
            &rig.speakers(&rig.lan),
            plan(&empty, MIDNIGHT + 30),
            &mut store,
        )
        .unwrap();
        assert_selected(&rig, current, 29);
        assert_eq!(writes(&rig, before), ["SetPlayMode", "Play"]);
    }
}

#[test]
fn a_start_with_new_direction_refills_after_the_live_work_without_replaying_old_ones() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.lookahead = 2;
        let retained = feed
            .start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        let current = retained[2].movements[1].position.unwrap();
        // No GENA observation is delivered: start must read the live cursor.
        select(&rig, current, 41, false);
        let queue = rig.queue();
        let composer = pool.works()[0].composer.clone();
        let direction = Steer {
            mood: None,
            constraints: DjConstraints {
                include_composers: vec![composer.clone()],
                strict: true,
                ..DjConstraints::default()
            },
        };
        let directed = Planning {
            steer: Some(&direction),
            ..plan(&pool, MIDNIGHT + 60)
        };
        let before = rig.sim.soap_log().len();
        feed.start(&rig.speakers(&rig.lan), directed, &mut store)
            .unwrap();
        assert_selected(&rig, current, 41);
        assert_eq!(&rig.queue()[..queue.len()], queue);
        assert_eq!(feed.queued()[0], retained[2]);
        assert_eq!(feed.played, retained[..2]);
        assert_eq!(feed.queued().len(), 3);
        assert!(
            feed.queued()[1..]
                .iter()
                .all(|work| work.composer == composer)
        );
        assert!(
            writes(&rig, before)
                .iter()
                .all(|action| matches!(action.as_str(), "AddURIToQueue" | "SetPlayMode" | "Play"))
        );
        assert!(plays(&store).is_empty());
    }
}

#[test]
fn fresh_foreign_and_stopped_starts_still_select_the_first_retained_work() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        let before = rig.sim.soap_log().len();
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert!(writes(&rig, before).iter().any(|action| action == "Seek"));
        let first = feed.queued()[0].first().unwrap();
        let current = feed.queued()[0].movements[1].position.unwrap();
        for foreign in [false, true] {
            select(&rig, current, 19, false);
            if foreign {
                transport::set_av_transport_uri(
                    &rig.lan,
                    rig.host(),
                    "http://example.invalid/radio.mp3",
                    "",
                )
                .unwrap();
                transport::play(&rig.lan, rig.host()).unwrap();
            } else {
                transport::stop(&rig.lan, rig.host()).unwrap();
            }
            let before = rig.sim.soap_log().len();
            feed.start(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 30),
                &mut store,
            )
            .unwrap();
            assert_selected(&rig, first, 0);
            assert!(writes(&rig, before).iter().any(|action| action == "Seek"));
        }
    }
}

#[derive(Clone, Copy)]
enum Interruption {
    Source,
    Track(u32),
    Queue,
    Reboot,
}

struct Interrupted<'a> {
    lan: &'a SimLan,
    sim: &'a SimHandle,
    on: &'static str,
    after_reads: usize,
    seen: Cell<usize>,
    interruption: Interruption,
    writes: RefCell<Vec<String>>,
    edited: Cell<bool>,
}

impl Transport for Interrupted<'_> {
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
        let response = self.lan.soap_post(host, path, action, body)?;
        if name == self.on {
            self.seen.set(self.seen.get() + 1);
        }
        if name == self.on && self.seen.get() == self.after_reads && !self.edited.replace(true) {
            match self.interruption {
                Interruption::Source => {
                    transport::set_av_transport_uri(
                        self.lan,
                        host,
                        "http://example.invalid/announcement.wav",
                        "",
                    )?;
                    transport::play(self.lan, host)?;
                }
                Interruption::Track(position) => {
                    transport::seek_track(self.lan, host, position)?;
                    transport::seek_position(self.lan, host, 13)?;
                }
                Interruption::Queue => {
                    add_uri_to_queue(
                        self.lan,
                        host,
                        "http://example.invalid/owners-track.wav",
                        "",
                        false,
                    )?;
                }
                Interruption::Reboot => {
                    self.sim.reboot(ROOM, Duration::ZERO).unwrap();
                }
            }
        }
        Ok(response)
    }
}

#[test]
fn a_source_or_occurrence_change_during_resume_never_falls_back_to_a_rewind() {
    for s2 in [false, true] {
        for source in [false, true] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let current = feed.queued()[0].movements[1].position.unwrap();
            let other = feed.queued()[1].first().unwrap();
            select(&rig, current, 17, true);
            let queue = rig.queue();
            let interrupted = Interrupted {
                lan: &rig.lan,
                sim: &rig.sim,
                on: "SetPlayMode",
                after_reads: 1,
                seen: Cell::new(0),
                interruption: if source {
                    Interruption::Source
                } else {
                    Interruption::Track(other)
                },
                writes: RefCell::new(Vec::new()),
                edited: Cell::new(false),
            };
            let result = feed.start(
                &rig.speakers(&interrupted),
                plan(&pool, MIDNIGHT + 30),
                &mut store,
            );
            assert!(matches!(
                result,
                Err(FeedError::Core(CoreError::Uncertain(_)))
            ));
            assert!(interrupted.edited.get());
            assert_eq!(*interrupted.writes.borrow(), ["SetPlayMode"]);
            assert_eq!(rig.queue(), queue);
            assert_eq!(feed.queue_len, None);
            assert!(feed.is_active());
            if source {
                assert_eq!(
                    transport::get_media_info(&rig.lan, rig.host()).unwrap().uri,
                    "http://example.invalid/announcement.wav"
                );
            } else {
                let position = transport::get_position_info(&rig.lan, rig.host()).unwrap();
                assert_eq!((position.track, position.position_secs), (other, Some(13)));
                assert_eq!(
                    transport::get_transport_info(&rig.lan, rig.host())
                        .unwrap()
                        .state,
                    TransportState::Paused
                );
            }
        }
    }
}

#[test]
fn unreadable_live_position_cannot_turn_a_resume_into_a_fresh_start() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let current = feed.queued()[0].movements[1].position.unwrap();
        select(&rig, current, 23, true);
        let before = rig.sim.soap_log().len();
        rig.sim.upnp_fault(ROOM, "GetPositionInfo", 701).unwrap();
        assert!(
            feed.start(
                &rig.speakers(&rig.lan),
                plan(&pool, MIDNIGHT + 30),
                &mut store,
            )
            .is_err()
        );
        assert!(writes(&rig, before).is_empty());
        rig.sim.clear_faults(ROOM).unwrap();
        let live = transport::get_position_info(&rig.lan, rig.host()).unwrap();
        assert_eq!((live.track, live.position_secs), (current, Some(23)));
        assert!(feed.is_active());
    }
}

#[test]
fn a_changed_initial_continuation_proof_cannot_authorize_a_fresh_start() {
    for s2 in [false, true] {
        for intervention in 0..4 {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let current = feed.queued()[0].movements[1].position.unwrap();
            let other = feed.queued()[1].first().unwrap();
            select(&rig, current, 17, true);
            let interruption = match intervention {
                0 => Interruption::Track(other),
                1 => Interruption::Source,
                2 => Interruption::Queue,
                _ => Interruption::Reboot,
            };
            let interrupted = Interrupted {
                lan: &rig.lan,
                sim: &rig.sim,
                on: "GetPositionInfo",
                // A queue edit after the first range read must invalidate
                // its revision even when the selected occurrence stays put.
                after_reads: if intervention == 2 { 2 } else { 1 },
                seen: Cell::new(0),
                interruption,
                writes: RefCell::new(Vec::new()),
                edited: Cell::new(false),
            };
            assert!(
                feed.start(
                    &rig.speakers(&interrupted),
                    plan(&pool, MIDNIGHT + 30),
                    &mut store,
                )
                .is_err()
            );
            assert!(interrupted.edited.get());
            assert!(
                interrupted.writes.borrow().is_empty(),
                "an inconclusive proof sends no mode, source, seek, add or Play"
            );
            assert_eq!(feed.queue_len, None);
            assert!(feed.is_active());
            if intervention == 0 {
                let live = transport::get_position_info(&rig.lan, rig.host()).unwrap();
                assert_eq!((live.track, live.position_secs), (other, Some(13)));
            }
        }
    }
}

#[test]
fn a_stable_owner_occurrence_still_allows_an_explicit_start_of_the_retained_dj() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let first = feed.queued()[0].first().unwrap();
        let owner = "spotify:track:OwnerSelectedTest";
        let params = rig.sim.render_params(if s2 { 2 } else { 1 }).unwrap();
        add_uri_to_queue(
            &rig.lan,
            rig.host(),
            &spotify_queue_uri(owner),
            &spotify_track_didl(owner, "Owner's choice", &params),
            false,
        )
        .unwrap();
        let position = u32::try_from(rig.queue().len()).unwrap();
        select(&rig, position, 11, true);
        let before = rig.sim.soap_log().len();
        feed.start(
            &rig.speakers(&rig.lan),
            plan(&pool, MIDNIGHT + 30),
            &mut store,
        )
        .unwrap();
        assert_selected(&rig, first, 0);
        assert!(writes(&rig, before).iter().any(|action| action == "Seek"));
        assert_eq!(rig.queue().last().map(String::as_str), Some(owner));
    }
}

struct ReportedState<'a> {
    lan: &'a SimLan,
    state: &'static str,
}

impl Transport for ReportedState<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.contains("#GetTransportInfo") {
            Ok(response.replace(
                "<CurrentTransportState>PLAYING</CurrentTransportState>",
                &format!(
                    "<CurrentTransportState>{}</CurrentTransportState>",
                    self.state
                ),
            ))
        } else {
            Ok(response)
        }
    }
}

#[test]
fn a_transitioning_or_unknown_owned_transport_waits_without_rewinding() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let current = feed.queued()[0].movements[1].position.unwrap();
        select(&rig, current, 23, false);
        for state in ["TRANSITIONING", "future-state"] {
            let before = rig.sim.soap_log().len();
            let reported = ReportedState {
                lan: &rig.lan,
                state,
            };
            assert!(
                feed.start(
                    &rig.speakers(&reported),
                    plan(&pool, MIDNIGHT + 30),
                    &mut store,
                )
                .is_err()
            );
            assert!(writes(&rig, before).is_empty());
            assert_selected(&rig, current, 23);
            assert_eq!(feed.queue_len, None);
        }
    }
}

struct MissingMediaUri<'a> {
    lan: &'a SimLan,
    missing_on: usize,
    seen: Cell<usize>,
}

impl Transport for MissingMediaUri<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.contains("#GetMediaInfo") {
            self.seen.set(self.seen.get() + 1);
            if self.seen.get() == self.missing_on {
                return Ok(response
                    .replace("<CurrentURI>", "<OmittedCurrentURI>")
                    .replace("</CurrentURI>", "</OmittedCurrentURI>"));
            }
        }
        Ok(response)
    }
}

#[test]
fn a_missing_live_source_field_never_authorizes_a_fresh_start() {
    for s2 in [false, true] {
        for stopped in [false, true] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let current = feed.queued()[0].movements[1].position.unwrap();
            select(&rig, current, 23, true);
            if stopped {
                transport::stop(&rig.lan, rig.host()).unwrap();
            }
            let selected_before = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            let before = rig.sim.soap_log().len();
            let incomplete = MissingMediaUri {
                lan: &rig.lan,
                // Exercise the initial live-source read and the stopped
                // queue's second read before authorizing an ordinary start.
                missing_on: if stopped { 2 } else { 1 },
                seen: Cell::new(0),
            };
            assert!(
                feed.start(
                    &rig.speakers(&incomplete),
                    plan(&pool, MIDNIGHT + 30),
                    &mut store,
                )
                .is_err()
            );
            assert_eq!(incomplete.seen.get(), incomplete.missing_on);
            assert!(writes(&rig, before).is_empty());
            let live = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            assert_eq!(
                (live.track, live.position_secs),
                (selected_before.track, selected_before.position_secs)
            );
            assert_eq!(feed.queue_len, None);
            assert!(feed.is_active());
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResumeReply {
    Refused,
    Lost,
    Malformed,
}

struct ResumeReplyFault<'a> {
    lan: &'a SimLan,
    action: &'static str,
    reply: ResumeReply,
    writes: RefCell<Vec<String>>,
}

impl Transport for ResumeReplyFault<'_> {
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
        if name == self.action && self.reply == ResumeReply::Refused {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "resume mode refused by simulator test".into(),
            });
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if name == self.action {
            return Err(match self.reply {
                ResumeReply::Lost => ProtoError::Network {
                    target: "simulated resume reply".into(),
                    detail: "the command succeeded but its reply was lost".into(),
                },
                ResumeReply::Malformed => {
                    ProtoError::Malformed("the applied resume command returned invalid XML".into())
                }
                ResumeReply::Refused => unreachable!("the refusal happens before the write"),
            });
        }
        Ok(response)
    }
}

#[test]
fn uncertain_resume_writes_are_not_replayed_or_reported_as_read_failures() {
    for s2 in [false, true] {
        for (action, reply) in [
            ("SetPlayMode", ResumeReply::Refused),
            ("SetPlayMode", ResumeReply::Lost),
            ("SetPlayMode", ResumeReply::Malformed),
            ("Play", ResumeReply::Lost),
        ] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
            let current = feed.queued()[0].movements[1].position.unwrap();
            select(&rig, current, 31, true);
            let observed = ResumeReplyFault {
                lan: &rig.lan,
                action,
                reply,
                writes: RefCell::new(Vec::new()),
            };
            let result = feed.start(
                &rig.speakers(&observed),
                plan(&pool, MIDNIGHT + 30),
                &mut store,
            );
            if reply == ResumeReply::Refused {
                assert!(matches!(
                    result,
                    Err(FeedError::Proto(ProtoError::SoapFault { .. }))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(FeedError::Core(CoreError::Uncertain(_)))
                ));
            }
            let expected = if action == "Play" {
                vec!["SetPlayMode", "Play"]
            } else {
                vec!["SetPlayMode"]
            };
            assert_eq!(*observed.writes.borrow(), expected);
            let live = transport::get_position_info(&rig.lan, rig.host()).unwrap();
            assert_eq!((live.track, live.position_secs), (current, Some(31)));
            assert_eq!(
                transport::get_transport_info(&rig.lan, rig.host())
                    .unwrap()
                    .state,
                if action == "Play" {
                    TransportState::Playing
                } else {
                    TransportState::Paused
                }
            );
            assert_eq!(feed.queue_len, None);
            assert!(feed.is_active());
        }
    }
}
