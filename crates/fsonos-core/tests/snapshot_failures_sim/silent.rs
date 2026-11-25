//! A source selection need not stop an already playing real renderer. Exercise
//! silent restoration over real sockets with that behavior and uncertain replies.

use super::*;
use std::cell::Cell;

const SAVED: &str = "http://example.invalid/saved.mp3";
const OWNER: &str = "http://example.invalid/new-choice.mp3";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behavior {
    PreserveState,
    PauseRefused,
    PauseLost,
    PauseLostUnapplied,
    PauseMalformed,
    PauseBeforeDispatch,
    PauseRefusedAfterOwnerPause,
    PauseFaultAfterOwnerPause,
    PauseChangesOccurrence,
    StopChangesOccurrence,
    StopClearsOccurrence,
    Unpausable,
    Unknown,
    ResumeAfterSelection,
    SourceRefusedOnce,
    SourceLost,
}

struct Renderer<'a> {
    inner: &'a SimLan,
    behavior: Behavior,
    installs: Cell<usize>,
    cleared: Cell<usize>,
    calls: RefCell<Vec<String>>,
}

impl<'a> Renderer<'a> {
    fn new(inner: &'a SimLan, behavior: Behavior) -> Self {
        Self {
            inner,
            behavior,
            installs: Cell::new(0),
            cleared: Cell::new(0),
            calls: RefCell::default(),
        }
    }

    fn count(&self, action: &str) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|name| *name == action)
            .count()
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost acknowledgement".into(),
        }
    }

    fn no_later_writes(&self) {
        for action in ["Seek", "Play", "SetVolume", "SetMute"] {
            assert_eq!(self.count(action), 0, "unexpected {action}");
        }
    }
}

impl Transport for Renderer<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if self.behavior == Behavior::PauseBeforeDispatch
            && name == "GetTransportInfo"
            && self.count(name) == 2
        {
            // The first read belongs to the original anchor. A different
            // controller pauses before pause_confirmed's initial state read.
            soap::pause(self.inner, host)?;
        }
        if name == "Pause" {
            match self.behavior {
                Behavior::PauseRefusedAfterOwnerPause | Behavior::PauseFaultAfterOwnerPause => {
                    soap::pause(self.inner, host)?;
                    return if self.behavior == Behavior::PauseFaultAfterOwnerPause {
                        Ok("<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                            <s:Body><s:Fault><faultcode>s:Client</faultcode>\
                            <faultstring>UPnPError</faultstring><detail>\
                            <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
                            <errorCode>701</errorCode>\
                            <errorDescription>injected pause refusal</errorDescription>\
                            </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
                            .into())
                    } else {
                        Err(ProtoError::SoapFault {
                            code: 701,
                            reason: "injected pause refusal".into(),
                        })
                    };
                }
                Behavior::PauseRefused
                | Behavior::Unpausable
                | Behavior::StopChangesOccurrence
                | Behavior::StopClearsOccurrence => {
                    return Err(ProtoError::SoapFault {
                        code: if self.behavior == Behavior::PauseRefused {
                            501
                        } else {
                            701
                        },
                        reason: "injected pause refusal".into(),
                    });
                }
                Behavior::PauseLostUnapplied => return Err(Self::lost(host)),
                _ => {}
            }
        }
        let playing = if name == "SetAVTransportURI" {
            self.installs.set(self.installs.get() + 1);
            if self.behavior == Behavior::SourceRefusedOnce && self.installs.get() == 1 {
                return Err(ProtoError::SoapFault {
                    code: 402,
                    reason: "injected source settling refusal".into(),
                });
            }
            soap::get_transport_info(self.inner, host)?.state == TransportState::Playing
        } else {
            false
        };
        let reply = self.inner.soap_post(host, path, action, body)?;
        if (name == "Pause" && self.behavior == Behavior::PauseChangesOccurrence)
            || (name == "Stop" && self.behavior == Behavior::StopChangesOccurrence)
        {
            // A separate controller chooses another occurrence after our
            // authorized state change, without changing the queue or group.
            soap::seek_track(self.inner, host, 2)?;
        }
        if name == "GetPositionInfo"
            && self.behavior == Behavior::StopClearsOccurrence
            && self.count("Stop") > 0
            && self.installs.get() == 0
        {
            // Some renderers erase the selected position on Stop. Preserve
            // the real socket response while modeling that firmware detail.
            let position = soap::get_position_info(self.inner, host)?;
            let cleared = reply
                .replace(&format!("<Track>{}</Track>", position.track), "<Track>0</Track>")
                .replace(
                    &format!(
                        "<TrackURI>{}</TrackURI>",
                        fsonos_proto::didl::xml_escape(&position.uri)
                    ),
                    "<TrackURI></TrackURI>",
                );
            assert_ne!(cleared, reply, "fixture must erase the selected occurrence");
            self.cleared.set(self.cleared.get() + 1);
            return Ok(cleared);
        }
        if name == "SetAVTransportURI" {
            if playing || self.behavior == Behavior::ResumeAfterSelection {
                // Model the renderer retaining an active state, or a separate
                // controller resuming just after the source was installed.
                soap::play(self.inner, host)?;
            }
            if self.behavior == Behavior::SourceLost {
                return Err(Self::lost(host));
            }
        }
        if name == "Pause" {
            match self.behavior {
                Behavior::PauseLost => return Err(Self::lost(host)),
                Behavior::PauseMalformed => return Ok("<broken>".into()),
                _ => {}
            }
        }
        if name == "GetTransportInfo" && self.behavior == Behavior::Unknown {
            return Ok(reply.replace("PLAYING", "UNKNOWN"));
        }
        Ok(reply)
    }
}

fn inactive_snapshot(rig: &mut Rig, uri: bool, state: TransportState) {
    rig.snapshot.transport_state = state;
    if uri {
        rig.snapshot.source = snapshot::SnapshotSource::Uri {
            uri: SAVED.into(),
            metadata: String::new(),
            position_secs: Some(10),
        };
        rig.snapshot.play_mode = None;
    }
}

fn inactive(rig: &Rig) {
    assert!(matches!(
        soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
        TransportState::Paused | TransportState::Stopped
    ));
}

#[test]
fn paused_and_stopped_snapshots_stay_silent_on_state_preserving_renderers() {
    for s2 in [false, true] {
        for uri in [false, true] {
            for state in [TransportState::Paused, TransportState::Stopped] {
                let mut rig = Rig::new(s2);
                inactive_snapshot(&mut rig, uri, state);
                let t = Renderer::new(&rig.lan, Behavior::PreserveState);
                let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
                assert!(report.skipped.is_empty(), "{report:?}");
                assert_eq!(t.count("Pause"), 1);
                assert_eq!(t.count("Stop"), 0);
                assert_eq!(t.count("Play"), 0);
                assert_eq!(t.installs.get(), 1);
                inactive(&rig);
                let position = soap::get_position_info(&rig.lan, rig.host).unwrap();
                assert_eq!(position.position_secs, Some(if uri { 10 } else { 40 }));
                rig.levels_restored();
            }
        }
    }
}

#[test]
fn source_only_silent_restores_do_not_pause_an_already_inactive_group() {
    for s2 in [false, true] {
        for uri in [false, true] {
            let mut rig = Rig::new(s2);
            inactive_snapshot(&mut rig, uri, TransportState::Paused);
            soap::pause(&rig.lan, rig.host).unwrap();
            let t = Renderer::new(&rig.lan, Behavior::PreserveState);
            let report =
                snapshot::restore_only(&t, &rig.houses, &rig.snapshot, &[Aspect::Source]).unwrap();
            assert!(report.skipped.is_empty(), "{report:?}");
            assert_eq!(t.count("Pause"), 0);
            assert_eq!(t.count("Stop"), 0);
            assert_eq!(t.installs.get(), 1);
            t.no_later_writes();
            inactive(&rig);
        }
    }
}

#[test]
fn denied_failed_or_unknown_pause_never_loads_the_source_or_raises_levels() {
    for (behavior, deny) in [
        (Behavior::PreserveState, true),
        (Behavior::PauseRefused, false),
        (Behavior::PauseLostUnapplied, false),
        (Behavior::Unknown, false),
    ] {
        let mut rig = Rig::new(true);
        inactive_snapshot(&mut rig, false, TransportState::Paused);
        let t = Renderer::new(&rig.lan, behavior);
        let report =
            snapshot::restore_vetted(&t, &rig.houses, &rig.snapshot, &Aspect::ALL, &|op| {
                if deny && matches!(op, RestoreOp::Stop) {
                    Err("caller cannot pause".into())
                } else {
                    Ok(op.clone())
                }
            })
            .unwrap();
        assert!(skipped(&report, Aspect::Source), "{behavior:?}: {report:?}");
        assert_eq!(t.installs.get(), 0);
        assert_eq!(t.count("Stop"), 0, "uncertain Pause cannot trigger Stop");
        if deny || behavior == Behavior::Unknown {
            assert_eq!(t.count("Pause"), 0);
        }
        t.no_later_writes();
        assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OTHER);
        assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 5);
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn lost_pause_replies_and_definite_stream_refusals_keep_one_silent_transition() {
    for s2 in [false, true] {
        for behavior in [
            Behavior::PauseLost,
            Behavior::PauseMalformed,
            Behavior::Unpausable,
            Behavior::SourceRefusedOnce,
            Behavior::SourceLost,
        ] {
            let mut rig = Rig::new(s2);
            inactive_snapshot(&mut rig, false, TransportState::Paused);
            let t = Renderer::new(&rig.lan, behavior);
            let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
            assert!(report.skipped.is_empty(), "{behavior:?}: {report:?}");
            assert_eq!(t.count("Pause"), 1, "{behavior:?}");
            assert_eq!(
                t.count("Stop"),
                usize::from(behavior == Behavior::Unpausable)
            );
            assert_eq!(
                t.installs.get(),
                if behavior == Behavior::SourceRefusedOnce {
                    2
                } else {
                    1
                }
            );
            assert_eq!(t.count("Play"), 0);
            inactive(&rig);
            rig.levels_restored();
        }
    }
}

#[test]
fn a_source_or_group_takeover_during_pause_authorization_is_left_alone() {
    for regroup in [false, true] {
        let mut rig = Rig::new(true);
        inactive_snapshot(&mut rig, false, TransportState::Paused);
        let t = Renderer::new(&rig.lan, Behavior::PreserveState);
        let changed = Cell::new(false);
        let report =
            snapshot::restore_vetted(&t, &rig.houses, &rig.snapshot, &Aspect::ALL, &|op| {
                if matches!(op, RestoreOp::Stop) && !changed.replace(true) {
                    if regroup {
                        let peer = PlayerId(rig.sim.player("Peer").unwrap().uuid.clone());
                        soap::join_group(&rig.lan, rig.peer, &peer).unwrap();
                    } else {
                        soap::set_av_transport_uri(&rig.lan, rig.host, OWNER, "").unwrap();
                        soap::play(&rig.lan, rig.host).unwrap();
                    }
                }
                Ok(op.clone())
            })
            .unwrap();
        assert!(changed.get());
        assert!(skipped(&report, Aspect::Source), "{report:?}");
        assert_eq!(t.installs.get(), 0);
        assert_eq!(t.count("Pause"), 0);
        assert_eq!(t.count("Stop"), 0);
        t.no_later_writes();
        assert_eq!(
            soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
            if regroup { OTHER } else { OWNER }
        );
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn resumed_playback_after_selection_blocks_seek_and_level_restoration() {
    for s2 in [false, true] {
        let mut rig = Rig::new(s2);
        inactive_snapshot(&mut rig, false, TransportState::Paused);
        let t = Renderer::new(&rig.lan, Behavior::ResumeAfterSelection);
        let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
        assert!(skipped(&report, Aspect::Source), "{report:?}");
        assert!(skipped(&report, Aspect::Position), "{report:?}");
        assert!(skipped(&report, Aspect::Volume), "{report:?}");
        assert_eq!(t.installs.get(), 1);
        assert_eq!(t.count("Pause"), 1);
        t.no_later_writes();
        assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 5);
        assert_eq!(soap::get_volume(&rig.lan, rig.peer).unwrap(), 70);
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn same_queue_changes_during_pause_or_fallback_authorization_are_left_alone() {
    for s2 in [false, true] {
        for fallback in [false, true] {
            for (manual_pause, deny) in [(false, false), (true, false), (true, true)] {
                let mut rig = Rig::new(s2);
                inactive_snapshot(&mut rig, true, TransportState::Paused);
                control::play_queue_from(
                    &rig.lan,
                    &rig.houses,
                    &rig.snapshot.coordinator,
                    1,
                )
                .unwrap();
                let t = Renderer::new(
                    &rig.lan,
                    if fallback {
                        Behavior::Unpausable
                    } else {
                        Behavior::PreserveState
                    },
                );
                let pause_vets = Cell::new(0);
                let change_at = if fallback { 2 } else { 1 };
                let report = snapshot::restore_vetted(
                    &t,
                    &rig.houses,
                    &rig.snapshot,
                    &Aspect::ALL,
                    &|op| {
                        if matches!(op, RestoreOp::Stop) {
                            pause_vets.set(pause_vets.get() + 1);
                            if pause_vets.get() == change_at {
                                if manual_pause {
                                    soap::pause(&rig.lan, rig.host).unwrap();
                                } else {
                                    soap::seek_track(&rig.lan, rig.host, 2).unwrap();
                                }
                                if deny {
                                    return Err(
                                        "caller paused independently and denied restoration".into(),
                                    );
                                }
                            }
                        }
                        Ok(op.clone())
                    },
                )
                .unwrap();
                assert_eq!(pause_vets.get(), change_at);
                for aspect in [
                    Aspect::Source,
                    Aspect::Position,
                    Aspect::Volume,
                    Aspect::Mute,
                ] {
                    assert!(skipped(&report, aspect), "{report:?}");
                }
                assert_eq!(t.installs.get(), 0);
                assert_eq!(t.count("Pause"), usize::from(fallback));
                assert_eq!(t.count("Stop"), 0);
                t.no_later_writes();
                assert_eq!(
                    soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
                    format!("x-rincon-queue:{}#0", rig.snapshot.coordinator.0)
                );
                assert_eq!(
                    soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                    if manual_pause { 1 } else { 2 }
                );
                assert_eq!(
                    soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
                    if manual_pause {
                        TransportState::Paused
                    } else {
                        TransportState::Playing
                    }
                );
                assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 5);
                assert_eq!(soap::get_volume(&rig.lan, rig.peer).unwrap(), 70);
            }
        }
    }
}

#[test]
fn source_reauthorization_cannot_retarget_an_inactive_queue_occurrence() {
    for s2 in [false, true] {
        for stop in [false, true] {
            let mut rig = Rig::new(s2);
            inactive_snapshot(&mut rig, true, TransportState::Paused);
            control::play_queue_from(&rig.lan, &rig.houses, &rig.snapshot.coordinator, 1)
                .unwrap();
            soap::pause(&rig.lan, rig.host).unwrap();
            let t = Renderer::new(&rig.lan, Behavior::PreserveState);
            let source_vets = Cell::new(0);
            let report = snapshot::restore_vetted(
                &t,
                &rig.houses,
                &rig.snapshot,
                &Aspect::ALL,
                &|op| {
                    if matches!(op, RestoreOp::SetUri { .. }) {
                        source_vets.set(source_vets.get() + 1);
                        // The first approval precedes source::restore's anchor.
                        // Change the queue only at its final source approval.
                        if source_vets.get() == 2 {
                            if stop {
                                soap::stop(&rig.lan, rig.host).unwrap();
                            } else {
                                soap::seek_track(&rig.lan, rig.host, 2).unwrap();
                            }
                        }
                    }
                    Ok(op.clone())
                },
            )
            .unwrap();
            assert_eq!(source_vets.get(), 2);
            for aspect in [
                Aspect::Source,
                Aspect::Position,
                Aspect::Volume,
                Aspect::Mute,
            ] {
                assert!(skipped(&report, aspect), "{report:?}");
            }
            assert_eq!(t.installs.get(), 0);
            assert_eq!(t.count("Pause"), 0);
            assert_eq!(t.count("Stop"), 0);
            t.no_later_writes();
            assert_eq!(
                soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
                format!("x-rincon-queue:{}#0", rig.snapshot.coordinator.0)
            );
            assert_eq!(
                soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
                if stop {
                    TransportState::Stopped
                } else {
                    TransportState::Paused
                }
            );
            if !stop {
                assert_eq!(soap::get_position_info(&rig.lan, rig.host).unwrap().track, 2);
            }
            assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 5);
            assert_eq!(soap::get_volume(&rig.lan, rig.peer).unwrap(), 70);
        }
    }
}

#[test]
fn unowned_inactive_readback_does_not_authorize_silent_restoration() {
    for s2 in [false, true] {
        for behavior in [
            Behavior::PauseBeforeDispatch,
            Behavior::PauseRefusedAfterOwnerPause,
            Behavior::PauseFaultAfterOwnerPause,
        ] {
            let mut rig = Rig::new(s2);
            inactive_snapshot(&mut rig, true, TransportState::Paused);
            let t = Renderer::new(&rig.lan, behavior);
            let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
            for aspect in [
                Aspect::Source,
                Aspect::Position,
                Aspect::Volume,
                Aspect::Mute,
            ] {
                assert!(skipped(&report, aspect), "{behavior:?}: {report:?}");
            }
            assert_eq!(t.installs.get(), 0);
            assert_eq!(
                t.count("Pause"),
                usize::from(behavior != Behavior::PauseBeforeDispatch)
            );
            assert_eq!(t.count("Stop"), 0);
            t.no_later_writes();
            assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OTHER);
            assert_eq!(
                soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
                TransportState::Paused
            );
            assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 5);
            assert_eq!(soap::get_volume(&rig.lan, rig.peer).unwrap(), 70);
        }
    }
}

#[test]
fn a_new_occurrence_after_an_authorized_pause_or_stop_is_left_alone() {
    for s2 in [false, true] {
        for behavior in [
            Behavior::PauseChangesOccurrence,
            Behavior::StopChangesOccurrence,
        ] {
            let mut rig = Rig::new(s2);
            inactive_snapshot(&mut rig, true, TransportState::Paused);
            control::play_queue_from(&rig.lan, &rig.houses, &rig.snapshot.coordinator, 1)
                .unwrap();
            let t = Renderer::new(&rig.lan, behavior);
            let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
            for aspect in [
                Aspect::Source,
                Aspect::Position,
                Aspect::Volume,
                Aspect::Mute,
            ] {
                assert!(skipped(&report, aspect), "{behavior:?}: {report:?}");
            }
            assert_eq!(t.installs.get(), 0);
            assert_eq!(t.count("Pause"), 1);
            assert_eq!(
                t.count("Stop"),
                usize::from(behavior == Behavior::StopChangesOccurrence)
            );
            t.no_later_writes();
            assert_eq!(
                soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
                format!("x-rincon-queue:{}#0", rig.snapshot.coordinator.0)
            );
            assert_eq!(soap::get_position_info(&rig.lan, rig.host).unwrap().track, 2);
            assert_eq!(
                soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
                if behavior == Behavior::StopChangesOccurrence {
                    TransportState::Stopped
                } else {
                    TransportState::Paused
                }
            );
            assert_eq!(soap::get_volume(&rig.lan, rig.host).unwrap(), 5);
            assert_eq!(soap::get_volume(&rig.lan, rig.peer).unwrap(), 70);
        }
    }
}

#[test]
fn an_authorized_stop_can_clear_position_evidence_before_silent_restoration() {
    for s2 in [false, true] {
        let mut rig = Rig::new(s2);
        inactive_snapshot(&mut rig, true, TransportState::Paused);
        control::play_queue_from(&rig.lan, &rig.houses, &rig.snapshot.coordinator, 1).unwrap();
        let t = Renderer::new(&rig.lan, Behavior::StopClearsOccurrence);
        let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(t.count("Pause"), 1);
        assert_eq!(t.count("Stop"), 1);
        assert_eq!(t.installs.get(), 1);
        assert!(t.cleared.get() >= 2);
        assert_eq!(t.count("Play"), 0);
        inactive(&rig);
        rig.levels_restored();
    }
}
