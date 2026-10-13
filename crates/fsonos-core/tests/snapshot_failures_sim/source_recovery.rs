//! Recovery of the source step, sharing the real-socket snapshot fixture.

use super::*;
use std::cell::Cell;

const OWNER: &str = "x-rincon-mp3radio://example.invalid/owner.mp3";

#[derive(Clone, Copy)]
enum Fault {
    Transient,
    Permanent,
    Lost,
    LostAndReadFailed,
    Malformed,
    Unapplied,
    Ignored,
    SourceChanged,
    GroupChanged,
    QueueChanged,
    ReadFailed,
}

struct Replies<'a> {
    rig: &'a Rig,
    fault: Fault,
    installs: Cell<usize>,
    calls: RefCell<Vec<String>>,
}

impl<'a> Replies<'a> {
    fn new(rig: &'a Rig, fault: Fault) -> Self {
        Self {
            rig,
            fault,
            installs: Cell::new(0),
            calls: RefCell::default(),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost source reply".into(),
        }
    }

    fn refusal() -> ProtoError {
        ProtoError::SoapFault {
            code: 402,
            reason: "injected source teardown refusal".into(),
        }
    }

    fn no_dependent_writes(&self) {
        assert!(
            self.calls
                .borrow()
                .iter()
                .all(|a| a != "Seek" && a != "Play")
        );
    }
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if self.installs.get() > 0
            && name == "GetMediaInfo"
            && matches!(self.fault, Fault::ReadFailed | Fault::LostAndReadFailed)
        {
            return Err(Self::lost(host));
        }
        if name != "SetAVTransportURI" {
            return self.rig.lan.soap_post(host, path, action, body);
        }
        let attempt = self.installs.get() + 1;
        self.installs.set(attempt);
        if matches!(self.fault, Fault::Permanent) {
            return Err(Self::refusal());
        }
        if attempt == 1 {
            match self.fault {
                Fault::Transient | Fault::ReadFailed => return Err(Self::refusal()),
                Fault::Unapplied => return Err(Self::lost(host)),
                Fault::Ignored => {
                    return Ok(
                        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                         <s:Body><u:SetAVTransportURIResponse xmlns:u=\"urn:x\">\
                         </u:SetAVTransportURIResponse></s:Body></s:Envelope>"
                            .into(),
                    );
                }
                Fault::SourceChanged => {
                    soap::set_av_transport_uri(&self.rig.lan, host, OWNER, "")?;
                    soap::play(&self.rig.lan, host)?;
                    return Err(Self::refusal());
                }
                Fault::GroupChanged => {
                    let peer = PlayerId(self.rig.sim.player("Peer").unwrap().uuid.clone());
                    soap::join_group(&self.rig.lan, self.rig.peer, &peer)?;
                    return Err(Self::refusal());
                }
                Fault::QueueChanged => {
                    soap::add_uri_to_queue(&self.rig.lan, host, OWNER, "", false)?;
                    return Err(Self::refusal());
                }
                _ => {}
            }
        }
        let reply = self.rig.lan.soap_post(host, path, action, body)?;
        if attempt == 1 {
            match self.fault {
                Fault::Lost | Fault::LostAndReadFailed => return Err(Self::lost(host)),
                Fault::Malformed => return Ok("<broken-reply>".into()),
                _ => {}
            }
        }
        Ok(reply)
    }
}

fn restored(rig: &Rig, t: &Replies<'_>) {
    let report = snapshot::restore(t, &rig.houses, &rig.snapshot).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    let position = soap::get_position_info(&rig.lan, rig.host).unwrap();
    assert_eq!((position.track, position.position_secs), (2, Some(40)));
    let state = soap::get_transport_info(&rig.lan, rig.host).unwrap().state;
    assert_eq!(state, TransportState::Playing);
    assert_eq!(t.calls.borrow().iter().filter(|a| *a == "Play").count(), 1);
    assert_eq!(t.calls.borrow().iter().filter(|a| *a == "Seek").count(), 2);
    rig.levels_restored();
}

#[test]
fn transient_source_refusal_retries_only_that_step_in_both_generations() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let t = Replies::new(&rig, Fault::Transient);
        restored(&rig, &t);
        assert_eq!(t.installs.get(), 2);
    }
}

#[test]
fn lost_or_malformed_source_replies_are_confirmed_without_reinstalling() {
    for s2 in [false, true] {
        for fault in [Fault::Lost, Fault::Malformed] {
            let rig = Rig::new(s2);
            let t = Replies::new(&rig, fault);
            restored(&rig, &t);
            assert_eq!(t.installs.get(), 1);
        }
    }
}

#[test]
fn repeated_refusal_is_bounded_and_never_replays_the_interruption() {
    let rig = Rig::new(true);
    let t = Replies::new(&rig, Fault::Permanent);
    let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
    assert!(skipped(&report, Aspect::Source));
    assert!(skipped(&report, Aspect::Transport));
    assert!(report.skipped.iter().any(|(_, why)| why.contains("402")));
    assert_eq!(t.installs.get(), 2);
    t.no_dependent_writes();
    rig.levels_restored();
}

#[test]
fn source_group_or_queue_changes_cancel_the_pending_source_retry() {
    for s2 in [false, true] {
        for fault in [
            Fault::SourceChanged,
            Fault::GroupChanged,
            Fault::QueueChanged,
        ] {
            let rig = Rig::new(s2);
            let t = Replies::new(&rig, fault);
            let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
            assert!(skipped(&report, Aspect::Source));
            assert_eq!(t.installs.get(), 1);
            t.no_dependent_writes();
            let expected = if matches!(fault, Fault::SourceChanged) {
                OWNER
            } else {
                OTHER
            };
            let media = soap::get_media_info(&rig.lan, rig.host).unwrap();
            assert_eq!(media.uri, expected);
        }
    }
}

#[test]
fn unapplied_or_unreadable_sources_never_authorize_following_writes() {
    for fault in [Fault::Unapplied, Fault::Ignored, Fault::ReadFailed] {
        let rig = Rig::new(true);
        let t = Replies::new(&rig, fault);
        let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
        assert!(skipped(&report, Aspect::Source));
        assert_eq!(t.installs.get(), 1);
        t.no_dependent_writes();
        rig.levels_restored();
    }
}

#[test]
fn policy_is_rechecked_before_a_transient_source_retry() {
    let rig = Rig::new(true);
    let t = Replies::new(&rig, Fault::Transient);
    let report = snapshot::restore_vetted(
        &t,
        &rig.houses,
        &rig.snapshot,
        &Aspect::ALL,
        &|op| {
            if matches!(op, RestoreOp::PlayQueue) && t.installs.get() > 0 {
                Err("source restore authorization was withdrawn".into())
            } else {
                Ok(op.clone())
            }
        },
    )
    .unwrap();
    assert!(skipped(&report, Aspect::Source));
    assert_eq!(t.installs.get(), 1);
    t.no_dependent_writes();
    rig.levels_restored();
}

#[test]
fn restoring_only_levels_never_reads_or_reinstalls_the_source() {
    let rig = Rig::new(true);
    let t = Replies::new(&rig, Fault::Permanent);
    let report = snapshot::restore_only(
        &t,
        &rig.houses,
        &rig.snapshot,
        &[Aspect::Volume, Aspect::Mute],
    )
    .unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(t.installs.get(), 0);
    assert!(!t.calls.borrow().iter().any(|a| a == "GetMediaInfo"));
    rig.levels_restored();
}

#[test]
fn direct_uri_restoration_uses_the_same_bounded_source_recovery() {
    for s2 in [false, true] {
        let mut rig = Rig::new(s2);
        rig.snapshot.source = snapshot::SnapshotSource::Uri {
            uri: OWNER.into(),
            metadata: String::new(),
            position_secs: Some(10),
        };
        let t = Replies::new(&rig, Fault::Transient);
        let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(t.installs.get(), 2);
        assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OWNER);
        let position = soap::get_position_info(&rig.lan, rig.host).unwrap();
        assert_eq!(position.position_secs, Some(10));
        rig.levels_restored();
    }
}

#[test]
fn a_lost_reply_with_failed_readback_does_not_replay_the_source() {
    let rig = Rig::new(true);
    let t = Replies::new(&rig, Fault::LostAndReadFailed);
    let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
    assert!(skipped(&report, Aspect::Source));
    assert_eq!(t.installs.get(), 1);
    t.no_dependent_writes();
    rig.levels_restored();
}

/// The production announcer needs a Sync transport; refuse just its first
/// queue-source restoration, not installation or playback of the clip.
struct AnnouncementReply<'a> {
    inner: &'a SimLan,
    attempts: std::sync::atomic::AtomicUsize,
}

impl Transport for AnnouncementReply<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#SetAVTransportURI")
            && body.contains("<CurrentURI>x-rincon-queue:")
            && self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        {
            return Err(Replies::refusal());
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn the_production_announcer_recovers_its_actual_queue_source_restore() {
    use fsonos_core::announce::{Announcer, Clip, Ending};
    use fsonos_core::resolve_room;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for s2 in [false, true] {
        let rig = Rig::new(s2);
        snapshot::restore(&rig.lan, &rig.houses, &rig.snapshot).unwrap();
        let t = AnnouncementReply {
            inner: &rig.lan,
            attempts: AtomicUsize::new(0),
        };
        let targets = [
            resolve_room(&rig.houses, "Room").unwrap(),
            resolve_room(&rig.houses, "Peer").unwrap(),
        ];
        let clip = Clip {
            url: "http://192.0.2.250:3400/media/test.wav".into(),
            title: "Test announcement".into(),
            duration: Duration::ZERO,
        };
        // The frozen simulator clock forces deadline Stop. Then exercise the
        // real announcer's cleanup, including its call to restore_vetted.
        let announcer = Announcer::with_timing(Duration::from_millis(1), Duration::ZERO);
        let report = announcer.announce(&t, &targets, &clip, 15, &|_| 100).unwrap();
        let house = &report.households[0];
        assert_eq!(house.outcome, Ok(Ending::Deadline));
        assert!(house.restore_failed.is_empty(), "{report:?}");
        assert!(house.restored.iter().all(|(_, r)| r.skipped.is_empty()));
        assert_eq!(t.attempts.load(Ordering::SeqCst), 2);
        let position = soap::get_position_info(&rig.lan, rig.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(40)));
        let state = soap::get_transport_info(&rig.lan, rig.host).unwrap().state;
        assert_eq!(state, TransportState::Playing);
        rig.levels_restored();
    }
}
