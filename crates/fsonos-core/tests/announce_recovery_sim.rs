//! Durable announcement recovery across a real serialization/process boundary.
//! The speakers keep running while the announcing worker disappears; recovery
//! must prove ownership, claim cleanup once, and never deliver the clip again.

use fsonos_core::announce::{
    AnnouncementCallbacks, AnnouncementCheckpoint, AnnouncementControl, AnnouncementRecovery,
    Announcer, Clip, HouseholdAnnouncement,
};
use fsonos_core::{CoreError, HouseholdState, resolve_room};
use fsonos_proto::content;
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const CLIP: &str = "http://192.0.2.250:3400/media/durable.wav";
const NEW: &str = "http://example.invalid/newer.mp3";

fn sim(s2: bool, pair: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        if pair {
            SimPlayerSpec::pair("Alpha", model)
        } else {
            SimPlayerSpec::new("Alpha", model)
        },
        SimPlayerSpec::new("Beta", model),
        SimPlayerSpec::new("Gamma", model),
    ];
    let builder = SimHousehold::builder();
    if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    }
    .spawn()
    .unwrap()
}

fn host(sim: &SimHandle, room: &str) -> IpAddr {
    sim.player(room).unwrap().ip
}
fn id(sim: &SimHandle, room: &str) -> PlayerId {
    PlayerId(sim.player(room).unwrap().uuid.clone())
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    let mut h = HouseholdState::default();
    h.apply_topology(&get_zone_group_state(lan, host(sim, "Alpha")).unwrap());
    vec![h]
}

fn metadata(uri: &str) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"file\" parentID=\"-1\" restricted=\"true\"><dc:title>Queue item</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo=\"http-get:*:audio/mpeg:*\" duration=\"0:10:00\">{uri}</res></item></DIDL-Lite>"
    )
}

fn queue(sim: &SimHandle, lan: &SimLan, room: &str) {
    let address = host(sim, room);
    for uri in [
        "http://example.invalid/a.mp3",
        "http://example.invalid/b.mp3",
    ] {
        soap::add_uri_to_queue(lan, address, uri, &metadata(uri), false).unwrap();
    }
    soap::play_from_queue(lan, address, &id(sim, room)).unwrap();
    soap::seek_track(lan, address, 2).unwrap();
    soap::seek_position(lan, address, 40).unwrap();
    set_play_mode(lan, address, PlayMode::ShuffleNoRepeat).unwrap();
    soap::play(lan, address).unwrap();
}

fn join(sim: &SimHandle, lan: &SimLan, room: &str, coordinator: &str) {
    soap::join_group(lan, host(sim, room), &id(sim, coordinator)).unwrap();
}

fn mutations(log: &[SoapLogEntry]) -> Vec<&SoapLogEntry> {
    log.iter()
        .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending,
    Claimed,
    Completed,
}

struct Saved {
    record: AnnouncementRecovery,
    phase: Phase,
    report: Option<HouseholdAnnouncement>,
}

#[derive(Default)]
struct Journal {
    saved: Mutex<Option<Saved>>,
    crash_ready: AtomicBool,
    fail_save: AtomicBool,
}

impl Journal {
    fn crashing() -> Self {
        Self {
            crash_ready: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn record(&self) -> AnnouncementRecovery {
        self.saved.lock().unwrap().as_ref().unwrap().record.clone()
    }

    /// The old worker and its ownership objects are gone. Only JSON is carried
    /// into the new process, including an already-claimed fence if present.
    fn reopened(&self) -> Self {
        let saved = self.saved.lock().unwrap();
        let saved = saved.as_ref().unwrap();
        let bytes = serde_json::to_vec(&saved.record).unwrap();
        let record: AnnouncementRecovery = serde_json::from_slice(&bytes).unwrap();
        assert!(record.valid());
        assert_eq!(record, saved.record);
        Self {
            saved: Mutex::new(Some(Saved {
                record,
                phase: saved.phase,
                report: None,
            })),
            ..Self::default()
        }
    }

    fn completed(&self) -> bool {
        self.saved.lock().unwrap().as_ref().unwrap().phase == Phase::Completed
    }
}

impl AnnouncementCheckpoint for Journal {
    fn save(&self, record: &AnnouncementRecovery) -> Result<(), CoreError> {
        if self.fail_save.load(Ordering::SeqCst) {
            return Err(CoreError::Store("synthetic durable-save failure".into()));
        }
        assert!(record.valid(), "{record:#?}");
        {
            let mut saved = self.saved.lock().unwrap();
            if saved
                .as_ref()
                .is_some_and(|old| old.phase != Phase::Pending || old.record.id() != record.id())
            {
                return Err(CoreError::Store("the record is no longer pending".into()));
            }
            *saved = Some(Saved {
                record: record.clone(),
                phase: Phase::Pending,
                report: None,
            });
        }
        if record.recoverable() && self.crash_ready.swap(false, Ordering::SeqCst) {
            panic!("synthetic process death after publishing established ownership");
        }
        Ok(())
    }

    fn claim(&self, record: &AnnouncementRecovery) -> Result<bool, CoreError> {
        let mut saved = self.saved.lock().unwrap();
        let Some(saved) = saved.as_mut() else {
            return Ok(false);
        };
        if saved.phase != Phase::Pending || saved.record != *record {
            return Ok(false);
        }
        saved.phase = Phase::Claimed;
        Ok(true)
    }

    fn complete(
        &self,
        record: &AnnouncementRecovery,
        report: &HouseholdAnnouncement,
    ) -> Result<(), CoreError> {
        let mut saved = self.saved.lock().unwrap();
        let saved = saved.as_mut().unwrap();
        assert_eq!(saved.record, *record);
        assert!(saved.phase != Phase::Completed);
        saved.phase = Phase::Completed;
        saved.report = Some(report.clone());
        Ok(())
    }

    fn current(&self, record: &AnnouncementRecovery) -> bool {
        self.saved
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|saved| saved.record == *record && saved.phase != Phase::Completed)
    }
}

fn fast() -> Announcer {
    Announcer::with_timing(Duration::from_millis(5), Duration::from_millis(20))
        .with_patience(Duration::from_secs(2))
}

fn announce(
    sim: &SimHandle,
    lan: &SimLan,
    transport: &(dyn Transport + Sync),
    rooms: &[&str],
    journal: &Journal,
) {
    let houses = houses(sim, lan);
    let targets: Vec<_> = rooms
        .iter()
        .map(|room| resolve_room(&houses, room).unwrap())
        .collect();
    let _ = fast()
        .announce_controlled_with_checkpoint(
            transport,
            &targets,
            &Clip {
                url: CLIP.into(),
                title: "durable".into(),
                duration: Duration::from_secs(10),
            },
            35,
            None,
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Ok(()),
                cap: &|_| 100,
                restore_cap: None,
                completed: &|_, _| {},
            },
            &AnnouncementControl::default(),
            journal,
        )
        .unwrap();
}

fn interrupted(sim: &SimHandle, lan: &SimLan, rooms: &[&str]) -> Journal {
    let journal = Journal::crashing();
    assert!(
        catch_unwind(AssertUnwindSafe(|| announce(
            sim, lan, lan, rooms, &journal
        )))
        .is_err()
    );
    let journal = journal.reopened();
    assert!(journal.record().recoverable());
    journal
}

fn no_clip_replayed(log: &[SoapLogEntry]) {
    assert!(
        !log.iter().any(|entry| entry.action == "SetAVTransportURI"
            && entry
                .args
                .iter()
                .any(|(key, value)| key == "CurrentURI" && value == CLIP)),
        "{log:#?}"
    );
}

#[test]
fn restart_restores_whole_groups_sources_levels_and_bonded_rooms_without_replaying_clip() {
    for s2 in [false, true] {
        let sim = sim(s2, true);
        let lan = sim.lan();
        queue(&sim, &lan, "Alpha");
        join(&sim, &lan, "Beta", "Alpha");
        soap::set_av_transport_uri(&lan, host(&sim, "Gamma"), NEW, &metadata(NEW)).unwrap();
        soap::play(&lan, host(&sim, "Gamma")).unwrap();
        for (room, level) in [("Alpha", 18), ("Beta", 24), ("Gamma", 9)] {
            soap::set_volume(&lan, host(&sim, room), level).unwrap();
        }
        soap::set_mute(&lan, host(&sim, "Gamma"), true).unwrap();
        let journal = interrupted(&sim, &lan, &["Alpha", "Beta", "Gamma"]);
        let from = sim.soap_log().len();
        let report = fast().recover(&lan, &journal.record(), &journal, None);
        assert!(
            report.outcome.is_err(),
            "delivery must remain interrupted: {report:#?}"
        );
        assert!(report.restore_failed.is_empty(), "{report:#?}");
        assert!(
            report
                .restored
                .iter()
                .all(|(_, restored)| restored.skipped.is_empty()),
            "{report:#?}"
        );
        assert!(journal.completed());
        let current = houses(&sim, &lan);
        assert_eq!(
            resolve_room(&current, "Beta").unwrap().coordinator.id,
            id(&sim, "Alpha")
        );
        assert_eq!(
            resolve_room(&current, "Gamma").unwrap().coordinator.id,
            id(&sim, "Gamma")
        );
        let position = soap::get_position_info(&lan, host(&sim, "Alpha")).unwrap();
        assert_eq!(position.track, 2);
        assert!(position.position_secs.unwrap() >= 40);
        assert_eq!(
            soap::get_media_info(&lan, host(&sim, "Gamma")).unwrap().uri,
            NEW
        );
        for (room, level) in [("Alpha", 18), ("Beta", 24), ("Gamma", 9)] {
            assert_eq!(soap::get_volume(&lan, host(&sim, room)).unwrap(), level);
        }
        assert!(soap::get_mute(&lan, host(&sim, "Gamma")).unwrap());
        no_clip_replayed(&sim.soap_log()[from..]);
    }
}

#[test]
fn restart_returns_a_partial_groups_live_survivor_with_new_edits_and_pause() {
    for s2 in [false, true] {
        let sim = sim(s2, true);
        let lan = sim.lan();
        queue(&sim, &lan, "Alpha");
        join(&sim, &lan, "Beta", "Alpha");
        join(&sim, &lan, "Gamma", "Alpha");
        let journal = interrupted(&sim, &lan, &["Alpha"]);
        let survivor = host(&sim, "Beta");
        soap::add_uri_to_queue(&lan, survivor, NEW, &metadata(NEW), false).unwrap();
        soap::seek_position(&lan, survivor, 75).unwrap();
        set_play_mode(&lan, survivor, PlayMode::RepeatOne).unwrap();
        soap::pause(&lan, survivor).unwrap();
        let from = sim.soap_log().len();
        let report = fast().recover(&lan, &journal.record(), &journal, None);
        assert!(report.restore_failed.is_empty(), "{report:#?}");
        assert!(
            report
                .restored
                .iter()
                .all(|(_, restored)| restored.skipped.is_empty()),
            "{report:#?}"
        );
        let current = houses(&sim, &lan);
        for room in ["Alpha", "Beta", "Gamma"] {
            assert_eq!(
                resolve_room(&current, room).unwrap().coordinator.id,
                id(&sim, "Alpha")
            );
        }
        let lead = host(&sim, "Alpha");
        assert_eq!(
            soap::get_transport_info(&lan, lead).unwrap().state,
            TransportState::Paused
        );
        assert_eq!(
            soap::get_position_info(&lan, lead).unwrap().position_secs,
            Some(75)
        );
        assert_eq!(get_play_mode(&lan, lead).unwrap(), PlayMode::RepeatOne);
        assert!(
            content::browse_all(&lan, lead, content::QUEUE)
                .unwrap()
                .iter()
                .any(|item| item
                    .res
                    .as_ref()
                    .is_some_and(|resource| resource.uri == NEW))
        );
        no_clip_replayed(&sim.soap_log()[from..]);
    }
}

#[test]
fn a_temporary_member_lead_follows_native_handback_to_its_original_coordinator() {
    for s2 in [false, true] {
        let sim = sim(s2, true);
        let lan = sim.lan();
        queue(&sim, &lan, "Alpha");
        join(&sim, &lan, "Beta", "Alpha");
        join(&sim, &lan, "Gamma", "Alpha");
        soap::set_volume(&lan, host(&sim, "Alpha"), 11).unwrap();
        soap::set_volume(&lan, host(&sim, "Beta"), 19).unwrap();
        let journal = interrupted(&sim, &lan, &["Alpha", "Beta"]);
        assert_eq!(*journal.record().lead(), id(&sim, "Beta"));
        let survivor = host(&sim, "Gamma");
        soap::seek_position(&lan, survivor, 75).unwrap();
        soap::pause(&lan, survivor).unwrap();
        let from = sim.soap_log().len();
        let report = fast().recover(&lan, &journal.record(), &journal, None);
        assert!(report.restore_failed.is_empty(), "{report:#?}");
        assert!(
            report
                .restored
                .iter()
                .all(|(_, restored)| restored.skipped.is_empty()),
            "{report:#?}"
        );
        let current = houses(&sim, &lan);
        for room in ["Alpha", "Beta", "Gamma"] {
            assert_eq!(
                resolve_room(&current, room).unwrap().coordinator.id,
                id(&sim, "Alpha")
            );
        }
        assert_eq!(
            soap::get_transport_info(&lan, host(&sim, "Alpha"))
                .unwrap()
                .state,
            TransportState::Paused
        );
        assert_eq!(
            soap::get_position_info(&lan, host(&sim, "Alpha"))
                .unwrap()
                .position_secs,
            Some(75)
        );
        assert_eq!(soap::get_volume(&lan, host(&sim, "Alpha")).unwrap(), 11);
        assert_eq!(soap::get_volume(&lan, host(&sim, "Beta")).unwrap(), 19);
        no_clip_replayed(&sim.soap_log()[from..]);
    }
}

#[test]
fn an_originally_idle_room_is_restored_by_one_confirmed_stop() {
    for s2 in [false, true] {
        let sim = sim(s2, false);
        let lan = sim.lan();
        soap::set_volume(&lan, host(&sim, "Alpha"), 12).unwrap();
        let journal = interrupted(&sim, &lan, &["Alpha"]);
        let from = sim.soap_log().len();
        let report = fast().recover(&lan, &journal.record(), &journal, None);
        assert!(report.restore_failed.is_empty(), "{report:#?}");
        assert!(
            report
                .restored
                .iter()
                .all(|(_, restored)| restored.skipped.is_empty()),
            "{report:#?}"
        );
        let log = sim.soap_log();
        assert_eq!(
            log[from..]
                .iter()
                .filter(|entry| entry.action == "Stop")
                .count(),
            1,
            "{report:#?}"
        );
        assert_eq!(
            soap::get_transport_info(&lan, host(&sim, "Alpha"))
                .unwrap()
                .state,
            TransportState::Stopped
        );
        assert_eq!(soap::get_volume(&lan, host(&sim, "Alpha")).unwrap(), 12);
        no_clip_replayed(&log[from..]);
    }
}

#[test]
fn newer_source_group_pause_or_boot_identity_prevents_restart_writes() {
    for s2 in [false, true] {
        for change in ["source", "group", "pause", "boot"] {
            let sim = sim(s2, false);
            let lan = sim.lan();
            queue(&sim, &lan, "Alpha");
            let journal = interrupted(&sim, &lan, &["Alpha"]);
            let lead = host(&sim, "Alpha");
            match change {
                "source" => {
                    soap::set_av_transport_uri(&lan, lead, NEW, &metadata(NEW)).unwrap();
                    soap::play(&lan, lead).unwrap();
                }
                "group" => join(&sim, &lan, "Beta", "Alpha"),
                "pause" => soap::pause(&lan, lead).unwrap(),
                "boot" => sim.reboot("Alpha", Duration::ZERO).unwrap(),
                _ => unreachable!(),
            }
            let from = sim.soap_log().len();
            let report = fast().recover(&lan, &journal.record(), &journal, None);
            assert!(
                report
                    .restored
                    .iter()
                    .all(|(_, restored)| restored.restored.is_empty()),
                "{change}: {report:#?}"
            );
            assert!(
                mutations(&sim.soap_log()[from..]).is_empty(),
                "{change}: {report:#?}"
            );
            assert!(journal.completed());
        }
    }
}

#[test]
fn newer_volume_is_preserved_while_owned_playback_returns_under_current_cap() {
    let sim = sim(true, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha");
    join(&sim, &lan, "Beta", "Alpha");
    soap::set_volume(&lan, host(&sim, "Alpha"), 50).unwrap();
    soap::set_volume(&lan, host(&sim, "Beta"), 40).unwrap();
    let journal = interrupted(&sim, &lan, &["Alpha", "Beta"]);
    soap::set_volume(&lan, host(&sim, "Beta"), 17).unwrap();
    let report = fast().recover(&lan, &journal.record(), &journal, Some(&|_| 25));
    assert!(report.restore_failed.is_empty(), "{report:#?}");
    assert_eq!(soap::get_volume(&lan, host(&sim, "Alpha")).unwrap(), 25);
    assert_eq!(soap::get_volume(&lan, host(&sim, "Beta")).unwrap(), 17);
    assert_eq!(
        soap::get_position_info(&lan, host(&sim, "Alpha"))
            .unwrap()
            .track,
        2
    );
}

#[test]
fn failed_initial_checkpoint_precedes_sleep_callbacks_and_all_speaker_mutations() {
    let sim = sim(false, false);
    let lan = sim.lan();
    let households = houses(&sim, &lan);
    let journal = Journal {
        fail_save: AtomicBool::new(true),
        ..Journal::default()
    };
    let caps = AtomicUsize::new(0);
    let from = sim.soap_log().len();
    let report = fast()
        .announce_controlled_with_checkpoint(
            &lan,
            &[resolve_room(&households, "Alpha").unwrap()],
            &Clip {
                url: CLIP.into(),
                title: "durable".into(),
                duration: Duration::from_secs(1),
            },
            35,
            None,
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Ok(()),
                cap: &|_| {
                    caps.fetch_add(1, Ordering::SeqCst);
                    100
                },
                restore_cap: None,
                completed: &|_, _| {},
            },
            &AnnouncementControl::default(),
            &journal,
        )
        .unwrap();
    assert!(
        report.households[0]
            .outcome
            .as_ref()
            .unwrap_err()
            .contains("checkpoint")
    );
    assert_eq!(caps.load(Ordering::SeqCst), 0);
    assert!(mutations(&sim.soap_log()[from..]).is_empty());
}

struct DieAfter<'a> {
    lan: &'a SimLan,
    action: &'static str,
}

impl Transport for DieAfter<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.lan.soap_post(host, path, action, body);
        assert!(
            !action
                .trim_matches('"')
                .ends_with(&format!("#{}", self.action)),
            "synthetic process death after attempted speaker effect"
        );
        result
    }
}

#[test]
fn interrupted_preparation_is_durable_but_is_never_falsely_recovered() {
    for action in ["SetVolume", "SetMute"] {
        let sim = sim(false, false);
        let lan = sim.lan();
        queue(&sim, &lan, "Alpha");
        let journal = Journal::default();
        let dying = DieAfter { lan: &lan, action };
        assert!(
            catch_unwind(AssertUnwindSafe(|| announce(
                &sim,
                &lan,
                &dying,
                &["Alpha"],
                &journal
            )))
            .is_err()
        );
        let journal = journal.reopened();
        assert!(!journal.record().recoverable());
        let from = sim.soap_log().len();
        let report = fast().recover(&lan, &journal.record(), &journal, None);
        assert!(
            report
                .outcome
                .as_ref()
                .unwrap_err()
                .contains("incomplete preparation")
        );
        assert!(mutations(&sim.soap_log()[from..]).is_empty());
        assert!(journal.completed());
    }
}

#[test]
fn accepted_clip_writes_recover_after_a_crash_before_the_established_checkpoint() {
    for s2 in [false, true] {
        for action in ["SetAVTransportURI", "Play"] {
            let sim = sim(s2, true);
            let lan = sim.lan();
            queue(&sim, &lan, "Alpha");
            // The Play boundary also exercises a completed partial-group
            // handoff whose survivor keeps the original live queue.
            if action == "Play" {
                join(&sim, &lan, "Beta", "Alpha");
                join(&sim, &lan, "Gamma", "Alpha");
            }
            soap::set_volume(&lan, host(&sim, "Alpha"), 18).unwrap();
            let journal = Journal::default();
            let dying = DieAfter { lan: &lan, action };
            assert!(
                catch_unwind(AssertUnwindSafe(|| announce(
                    &sim,
                    &lan,
                    &dying,
                    &["Alpha"],
                    &journal
                )))
                .is_err()
            );
            let journal = journal.reopened();
            assert!(
                !journal.record().recoverable(),
                "the worker never saved an established marker"
            );
            assert_eq!(
                soap::get_media_info(&lan, host(&sim, "Alpha"))
                    .unwrap()
                    .uri,
                CLIP
            );
            let from = sim.soap_log().len();
            let report = fast().recover(&lan, &journal.record(), &journal, None);
            assert!(report.restore_failed.is_empty(), "{action}: {report:#?}");
            assert!(
                report
                    .restored
                    .iter()
                    .all(|(_, restored)| restored.skipped.is_empty()),
                "{action}: {report:#?}"
            );
            assert_eq!(soap::get_volume(&lan, host(&sim, "Alpha")).unwrap(), 18);
            assert_eq!(
                soap::get_position_info(&lan, host(&sim, "Alpha"))
                    .unwrap()
                    .track,
                2
            );
            assert_eq!(
                soap::get_transport_info(&lan, host(&sim, "Alpha"))
                    .unwrap()
                    .state,
                TransportState::Playing
            );
            if action == "Play" {
                let current = houses(&sim, &lan);
                for room in ["Alpha", "Beta", "Gamma"] {
                    assert_eq!(
                        resolve_room(&current, room).unwrap().coordinator.id,
                        id(&sim, "Alpha")
                    );
                }
            }
            no_clip_replayed(&sim.soap_log()[from..]);
            assert!(journal.completed());
        }
    }
}

struct BeforeClip<'a>(&'a SimLan);

impl Transport for BeforeClip<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        assert!(
            !(action.contains("#SetAVTransportURI") && body.contains(CLIP)),
            "synthetic process death before clip dispatch"
        );
        self.0.soap_post(host, path, action, body)
    }
}

#[test]
fn an_attempted_clip_record_cannot_restore_without_an_actual_owned_clip() {
    let sim = sim(false, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha");
    let journal = Journal::default();
    assert!(
        catch_unwind(AssertUnwindSafe(|| announce(
            &sim,
            &lan,
            &BeforeClip(&lan),
            &["Alpha"],
            &journal
        )))
        .is_err()
    );
    let journal = journal.reopened();
    assert!(!journal.record().recoverable());
    let from = sim.soap_log().len();
    let report = fast().recover(&lan, &journal.record(), &journal, None);
    assert!(
        report
            .restored
            .iter()
            .all(|(_, restored)| restored.restored.is_empty()),
        "{report:#?}"
    );
    assert!(mutations(&sim.soap_log()[from..]).is_empty());
    assert_eq!(
        soap::get_position_info(&lan, host(&sim, "Alpha"))
            .unwrap()
            .track,
        2
    );
    assert!(journal.completed());
}

#[test]
fn another_process_claim_and_second_crash_cannot_replay_cleanup() {
    let sim = sim(true, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha");
    let journal = interrupted(&sim, &lan, &["Alpha"]);
    let record = journal.record();
    assert!(journal.claim(&record).unwrap());
    let journal = journal.reopened();
    let from = sim.soap_log().len();
    let report = fast().recover(&lan, &record, &journal, None);
    assert!(
        report
            .outcome
            .as_ref()
            .unwrap_err()
            .contains("already claimed")
    );
    assert!(mutations(&sim.soap_log()[from..]).is_empty());
    assert!(
        !journal.completed(),
        "a losing worker cannot retire another worker's live claim"
    );
}

struct Offline;

impl Transport for Offline {
    fn soap_post(&self, host: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        Err(ProtoError::Network {
            target: host.to_string(),
            detail: "offline during startup".into(),
        })
    }
}

#[test]
fn unavailable_initial_evidence_stays_pending_and_can_recover_when_reads_return() {
    let sim = sim(false, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha");
    let journal = interrupted(&sim, &lan, &["Alpha"]);
    let record = journal.record();
    let report = fast().recover(&Offline, &record, &journal, None);
    assert!(!report.restore_failed.is_empty());
    assert!(!journal.completed());
    assert!(journal.current(&record));
    let report = fast().recover(&lan, &record, &journal, None);
    assert!(report.restore_failed.is_empty(), "{report:#?}");
    assert!(journal.completed());
    assert_eq!(
        soap::get_position_info(&lan, host(&sim, "Alpha"))
            .unwrap()
            .track,
        2
    );
}

#[test]
fn missing_physical_player_waits_for_reappearance_then_checks_its_new_boot() {
    let sim = sim(true, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha");
    join(&sim, &lan, "Beta", "Alpha");
    let journal = interrupted(&sim, &lan, &["Alpha", "Beta"]);
    let record = journal.record();
    sim.set_offline("Beta", true).unwrap();
    let from = sim.soap_log().len();
    let report = fast().recover(&lan, &record, &journal, None);
    assert!(!report.restore_failed.is_empty(), "{report:#?}");
    assert!(!journal.completed());
    assert!(mutations(&sim.soap_log()[from..]).is_empty());
    sim.set_offline("Beta", false).unwrap();
    join(&sim, &lan, "Beta", "Alpha");
    let from = sim.soap_log().len();
    let report = fast().recover(&lan, &record, &journal, None);
    assert!(journal.completed());
    assert!(mutations(&sim.soap_log()[from..]).is_empty(), "{report:#?}");
}

struct LostRestore<'a> {
    lan: &'a SimLan,
    attempts: AtomicUsize,
}

impl Transport for LostRestore<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#SetAVTransportURI") && body.contains("<CurrentURI>x-rincon-queue:") {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "source write lost; application is unknown".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn a_lost_restart_mutation_is_never_replayed_by_the_live_cleanup_retry_loop() {
    let sim = sim(false, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha");
    let journal = interrupted(&sim, &lan, &["Alpha"]);
    let record = journal.record();
    let lost = LostRestore {
        lan: &lan,
        attempts: AtomicUsize::new(0),
    };
    let report = fast().recover(&lost, &record, &journal, None);
    assert_eq!(lost.attempts.load(Ordering::SeqCst), 1, "{report:#?}");
    assert!(
        report.restored.iter().any(|(_, restored)| {
            restored.skipped.iter().any(|(aspect, why)| {
                *aspect == fsonos_core::snapshot::Aspect::Source
                    && why.contains("source write lost; application is unknown")
            })
        }),
        "{report:#?}"
    );
    assert!(journal.completed());
    let from = sim.soap_log().len();
    let report = fast().recover(&lan, &record, &journal, None);
    assert!(
        report
            .outcome
            .as_ref()
            .unwrap_err()
            .contains("already claimed")
    );
    assert!(mutations(&sim.soap_log()[from..]).is_empty());
}

#[test]
fn forged_duplicate_level_evidence_is_invalid_and_missing_boot_is_not_recoverable() {
    let sim = sim(true, false);
    let lan = sim.lan();
    queue(&sim, &lan, "Alpha");
    join(&sim, &lan, "Beta", "Alpha");
    let journal = interrupted(&sim, &lan, &["Alpha", "Beta"]);
    let record = journal.record();
    let mut duplicate = serde_json::to_value(&record).unwrap();
    duplicate["snapshots"][0]["levels"][1] = duplicate["snapshots"][0]["levels"][0].clone();
    assert!(
        !serde_json::from_value::<AnnouncementRecovery>(duplicate)
            .unwrap()
            .valid()
    );
    let mut absent = serde_json::to_value(&record).unwrap();
    absent["proof"]["identities"][0]["boot"] = serde_json::Value::Null;
    let absent: AnnouncementRecovery = serde_json::from_value(absent).unwrap();
    assert!(
        absent.valid(),
        "structural validity permits terminal receipts without positive boot evidence"
    );
    assert!(!absent.recoverable());
}

#[derive(Clone, Copy, Debug)]
enum Intervene {
    SourceBeforeRestore,
    PauseBeforeRestore,
    PauseAfterSelection,
}

struct DuringRecovery<'a> {
    sim: &'a SimHandle,
    lan: &'a SimLan,
    lead: IpAddr,
    change: Intervene,
    stopped: AtomicBool,
    changed: AtomicBool,
    after: AtomicUsize,
}

impl Transport for DuringRecovery<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.lan.soap_post(host, path, action, body);
        if result.is_ok() && host == self.lead && action.contains("#Stop\"") {
            self.stopped.store(true, Ordering::SeqCst);
        }
        let boundary = match self.change {
            Intervene::SourceBeforeRestore | Intervene::PauseBeforeRestore => {
                self.stopped.load(Ordering::SeqCst) && action.contains("#Browse\"")
            }
            Intervene::PauseAfterSelection => {
                action.contains("#SetAVTransportURI")
                    && body.contains("<CurrentURI>x-rincon-queue:")
            }
        };
        if result.is_ok() && boundary && !self.changed.swap(true, Ordering::SeqCst) {
            match self.change {
                Intervene::SourceBeforeRestore => {
                    soap::set_av_transport_uri(self.lan, self.lead, NEW, &metadata(NEW))?;
                    soap::play(self.lan, self.lead)?;
                }
                Intervene::PauseBeforeRestore | Intervene::PauseAfterSelection => {
                    // Both boundaries follow a confirmed Stop/SetURI. A real
                    // listener resumes then pauses that loaded source.
                    soap::play(self.lan, self.lead)?;
                    soap::pause(self.lan, self.lead)?;
                }
            }
            self.after
                .store(self.sim.soap_log().len(), Ordering::SeqCst);
        }
        result
    }
}

#[test]
fn source_replacement_or_pause_during_cleanup_revokes_every_remaining_write() {
    for s2 in [false, true] {
        for change in [
            Intervene::SourceBeforeRestore,
            Intervene::PauseBeforeRestore,
            Intervene::PauseAfterSelection,
        ] {
            let sim = sim(s2, false);
            let lan = sim.lan();
            queue(&sim, &lan, "Alpha");
            let journal = interrupted(&sim, &lan, &["Alpha"]);
            let lead = host(&sim, "Alpha");
            let during = DuringRecovery {
                sim: &sim,
                lan: &lan,
                lead,
                change,
                stopped: AtomicBool::new(false),
                changed: AtomicBool::new(false),
                after: AtomicUsize::new(0),
            };
            let report = fast().recover(&during, &journal.record(), &journal, None);
            assert!(
                during.changed.load(Ordering::SeqCst),
                "{change:?}: {report:#?}"
            );
            assert!(
                mutations(&sim.soap_log()[during.after.load(Ordering::SeqCst)..]).is_empty(),
                "{change:?}: {report:#?}"
            );
            match change {
                Intervene::SourceBeforeRestore => {
                    assert_eq!(soap::get_media_info(&lan, lead).unwrap().uri, NEW)
                }
                _ => assert_eq!(
                    soap::get_transport_info(&lan, lead).unwrap().state,
                    TransportState::Paused
                ),
            }
            assert!(journal.completed());
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ReturningChange {
    Source,
    Metadata,
    Pause,
}

struct AfterRelease<'a> {
    sim: &'a SimHandle,
    lan: &'a SimLan,
    change: ReturningChange,
    changed: AtomicBool,
    after: AtomicUsize,
}

impl Transport for AfterRelease<'_> {
    fn soap_post(
        &self,
        address: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.lan.soap_post(address, path, action, body);
        let returning = host(self.sim, "Gamma");
        let own = format!(
            "<CurrentURI>x-rincon:{}</CurrentURI>",
            id(self.sim, "Gamma").0
        );
        if address == returning
            && action.contains("#SetAVTransportURI")
            && body.contains(&own)
            && !self.changed.swap(true, Ordering::SeqCst)
        {
            // Self-leave can answer 402 after applying. The listener changes
            // this independent zone while the other original group recovers.
            match self.change {
                ReturningChange::Source => {
                    let uri = "http://example.invalid/new-listener.mp3";
                    soap::set_av_transport_uri(self.lan, returning, uri, &metadata(uri))?;
                    soap::play(self.lan, returning)?;
                }
                ReturningChange::Metadata => {
                    let changed = metadata(NEW).replace("audio/mpeg", "audio/mp4");
                    soap::set_av_transport_uri(self.lan, returning, NEW, &changed)?;
                    soap::play(self.lan, returning)?;
                }
                ReturningChange::Pause => {
                    soap::play(self.lan, returning)?;
                    soap::pause(self.lan, returning)?;
                }
            }
            self.after
                .store(self.sim.soap_log().len(), Ordering::SeqCst);
        }
        result
    }
}

#[test]
fn a_returning_original_zone_preserves_its_new_source_metadata_or_pause_independently() {
    for s2 in [false, true] {
        for change in [
            ReturningChange::Source,
            ReturningChange::Metadata,
            ReturningChange::Pause,
        ] {
            let sim = sim(s2, true);
            let lan = sim.lan();
            queue(&sim, &lan, "Alpha");
            soap::set_av_transport_uri(&lan, host(&sim, "Gamma"), NEW, &metadata(NEW)).unwrap();
            soap::play(&lan, host(&sim, "Gamma")).unwrap();
            soap::set_volume(&lan, host(&sim, "Alpha"), 18).unwrap();
            soap::set_volume(&lan, host(&sim, "Gamma"), 9).unwrap();
            let journal = interrupted(&sim, &lan, &["Alpha", "Gamma"]);
            let during = AfterRelease {
                sim: &sim,
                lan: &lan,
                change,
                changed: AtomicBool::new(false),
                after: AtomicUsize::new(0),
            };
            let report = fast().recover(&during, &journal.record(), &journal, None);
            assert!(
                during.changed.load(Ordering::SeqCst),
                "{change:?}: {report:#?}"
            );
            let log = sim.soap_log();
            assert!(
                mutations(&log[during.after.load(Ordering::SeqCst)..])
                    .iter()
                    .all(|entry| entry.room != "Gamma"),
                "{change:?}: {report:#?}"
            );
            assert_eq!(
                soap::get_volume(&lan, host(&sim, "Gamma")).unwrap(),
                35,
                "new playback retains its current level"
            );
            assert!(
                report
                    .restored
                    .iter()
                    .any(|(zone, restored)| *zone == id(&sim, "Gamma")
                        && !restored.skipped.is_empty()),
                "{change:?}: {report:#?}"
            );
            // A changed independent room does not strand the owned group.
            assert_eq!(soap::get_volume(&lan, host(&sim, "Alpha")).unwrap(), 18);
            assert_eq!(
                soap::get_position_info(&lan, host(&sim, "Alpha"))
                    .unwrap()
                    .track,
                2
            );
            assert_eq!(
                soap::get_transport_info(&lan, host(&sim, "Alpha"))
                    .unwrap()
                    .state,
                TransportState::Playing
            );
            let current = soap::get_media_info(&lan, host(&sim, "Gamma")).unwrap();
            match change {
                ReturningChange::Source => {
                    assert_eq!(current.uri, "http://example.invalid/new-listener.mp3")
                }
                ReturningChange::Metadata => assert!(current.uri_metadata.contains("audio/mp4")),
                ReturningChange::Pause => assert_eq!(
                    soap::get_transport_info(&lan, host(&sim, "Gamma"))
                        .unwrap()
                        .state,
                    TransportState::Paused
                ),
            }
            assert!(journal.completed());
        }
    }
}
