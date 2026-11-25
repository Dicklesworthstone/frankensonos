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
    calls: RefCell<Vec<String>>,
}

impl<'a> Renderer<'a> {
    fn new(inner: &'a SimLan, behavior: Behavior) -> Self {
        Self {
            inner,
            behavior,
            installs: Cell::new(0),
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
        if name == "Pause" {
            match self.behavior {
                Behavior::PauseRefused | Behavior::Unpausable => {
                    return Err(ProtoError::SoapFault {
                        code: if self.behavior == Behavior::Unpausable {
                            701
                        } else {
                            501
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
