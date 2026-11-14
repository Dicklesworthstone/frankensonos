//! A failed destination must not make a relocation stop its healthy source.
//! Replies still travel over the simulator's real sockets; only the independent
//! renderer status (and the selected acknowledgement fault) is changed here.

use super::*;

struct StatusReplies {
    inner: SimLan,
    target: IpAddr,
    /// Healthy transport reads after Play before the reported status changes.
    after: Option<(usize, Option<&'static str>)>,
    error_before_play: bool,
    one_read_only: bool,
    reported_state: Option<&'static str>,
    play_reply: Reply,
    played: Cell<bool>,
    reads: Cell<usize>,
    injected: Cell<bool>,
}

impl StatusReplies {
    fn new(rig: &Rig) -> Self {
        Self {
            inner: rig.sim.lan(),
            target: rig.host(&rig.to),
            after: None,
            error_before_play: false,
            one_read_only: false,
            reported_state: None,
            play_reply: Reply::Pass,
            played: Cell::new(false),
            reads: Cell::new(0),
            injected: Cell::new(false),
        }
    }
}

fn status(response: &mut String, value: Option<&str>) {
    if let Some(value) = value {
        field(response, "CurrentTransportStatus", value);
    } else {
        let start = response.find("<CurrentTransportStatus>").unwrap();
        let end = response[start..]
            .find("</CurrentTransportStatus>")
            .unwrap()
            + start
            + "</CurrentTransportStatus>".len();
        response.replace_range(start..end, "");
    }
}

impl Transport for StatusReplies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let mut response = self.inner.soap_post(host, path, action, body)?;
        if host != self.target {
            return Ok(response);
        }
        match name(action) {
            "Play" => {
                self.played.set(true);
                match self.play_reply {
                    Reply::Lost => return Err(lost(host)),
                    Reply::Malformed => return Ok("<incomplete-reply>".into()),
                    _ => {}
                }
            }
            "GetTransportInfo" if self.played.get() => {
                let read = self.reads.get();
                self.reads.set(read + 1);
                if let Some((healthy, value)) = self.after
                    && read >= healthy
                    && (!self.one_read_only || read == healthy)
                {
                    self.injected.set(true);
                    status(&mut response, value);
                    if let Some(state) = self.reported_state {
                        field(&mut response, "CurrentTransportState", state);
                    }
                }
            }
            "GetTransportInfo" if self.error_before_play => {
                status(&mut response, Some("ERROR_OCCURRED"));
            }
            _ => {}
        }
        Ok(response)
    }
}

fn source_kept(rig: &Rig, since: usize) {
    rig.source_playing();
    let log = rig.sim.soap_log();
    assert!(
        log[since..]
            .iter()
            .all(|entry| entry.room != rig.from || entry.action != "Stop"),
        "a failed destination must not release the source"
    );
    assert_eq!(
        log[since..]
            .iter()
            .filter(|entry| entry.room == rig.to && entry.action == "Play")
            .count(),
        1,
        "destination Play must not be replayed"
    );
}

#[test]
fn an_error_with_playing_or_transitioning_never_releases_the_source() {
    for (from, to) in PAIRS {
        for kind in [Kind::Radio, Kind::File, Kind::Queue] {
            for state in ["PLAYING", "TRANSITIONING"] {
                let rig = Rig::new(from, to, kind, false);
                let mut replies = StatusReplies::new(&rig);
                replies.after = Some((0, Some(" ERROR_OCCURRED ")));
                replies.reported_state = Some(state);
                let since = rig.sim.soap_log().len();
                let error = rig.copy(&replies).unwrap_err().to_string();
                assert!(error.contains("ERROR_OCCURRED"), "{kind:?}: {state}: {error}");
                assert!(replies.injected.get());
                assert_eq!(
                    replies.reads.get(),
                    1,
                    "a reported renderer failure is not a buffering wait"
                );
                source_kept(&rig, since);
            }
        }
    }
}

#[test]
fn late_renderer_errors_remain_a_barrier_through_final_destination_checks() {
    for (from, to) in PAIRS {
        for kind in [Kind::Radio, Kind::File, Kind::Queue] {
            // Direct sources have one final ownership read after Play.
            // Queues also verify the staged owner and final transport state.
            let final_reads = if kind == Kind::Queue { 4 } else { 2 };
            for healthy in 1..final_reads {
                let rig = Rig::new(from, to, kind, false);
                let mut replies = StatusReplies::new(&rig);
                replies.after = Some((healthy, Some("ERROR_OCCURRED")));
                replies.one_read_only = true;
                let since = rig.sim.soap_log().len();
                let error = rig.copy(&replies).unwrap_err().to_string();
                assert!(error.contains("ERROR_OCCURRED"), "{kind:?}: {healthy}: {error}");
                assert!(replies.injected.get(), "the late failure was observed");
                source_kept(&rig, since);
            }
        }
    }
}

#[test]
fn missing_unknown_and_success_statuses_keep_the_existing_move_contract() {
    for (from, to) in PAIRS {
        for kind in [Kind::Radio, Kind::File, Kind::Queue] {
            for value in [None, Some(""), Some("OK"), Some("vendor-specific")] {
                let rig = Rig::new(from, to, kind, false);
                let mut replies = StatusReplies::new(&rig);
                replies.after = Some((0, value));
                let report = rig.copy(&replies).unwrap();
                assert_eq!(report.method, MoveMethod::Copied);
                assert!(replies.injected.get());
                rig.moved();
            }
        }
    }
}

#[test]
fn a_failed_old_destination_can_still_be_replaced_by_healthy_playback() {
    for (from, to) in PAIRS {
        for kind in [Kind::Radio, Kind::File, Kind::Queue] {
            let rig = Rig::new(from, to, kind, false);
            let mut replies = StatusReplies::new(&rig);
            replies.error_before_play = true;
            let report = rig.copy(&replies).unwrap();
            assert_eq!(report.method, MoveMethod::Copied);
            assert!(replies.played.get());
            rig.moved();
        }
    }
}

#[test]
fn a_paused_move_does_not_require_or_start_successful_playback() {
    for (from, to) in PAIRS {
        for kind in [Kind::Radio, Kind::File, Kind::Queue] {
            let rig = Rig::new(from, to, kind, true);
            let mut replies = StatusReplies::new(&rig);
            replies.error_before_play = true;
            let since = rig.sim.soap_log().len();
            let report = rig.copy(&replies).unwrap();
            assert_eq!(report.method, MoveMethod::Copied);
            assert!(!replies.played.get());
            assert!(
                rig.sim.soap_log()[since..]
                    .iter()
                    .all(|entry| entry.action != "Play")
            );
            rig.moved();
        }
    }
}

#[test]
fn a_renderer_error_does_not_convert_a_lost_direct_play_reply_into_success() {
    for (from, to) in PAIRS {
        for kind in [Kind::Radio, Kind::File] {
            for reply in [Reply::Lost, Reply::Malformed] {
                let rig = Rig::new(from, to, kind, false);
                let mut replies = StatusReplies::new(&rig);
                replies.after = Some((0, Some("ERROR_OCCURRED")));
                replies.play_reply = reply;
                let since = rig.sim.soap_log().len();
                let error = rig.copy(&replies).unwrap_err().to_string();
                if reply == Reply::Lost {
                    assert!(error.contains("injected lost relocation reply"), "{error}");
                }
                assert!(replies.injected.get());
                source_kept(&rig, since);
            }
        }
    }
}

#[test]
fn a_failed_native_handoff_fallback_keeps_the_source_running() {
    for generation in [1, 2] {
        let (from, to) = if generation == 1 {
            ("S1 Source", "S1 Target")
        } else {
            ("S2 Source", "S2 Target")
        };
        for kind in [Kind::Radio, Kind::Queue] {
            let rig = Rig::new(from, to, kind, false);
            rig.sim
                .upnp_fault(from, "DelegateGroupCoordinationTo", 401)
                .unwrap();
            let mut replies = StatusReplies::new(&rig);
            replies.after = Some((0, Some("ERROR_OCCURRED")));
            let since = rig.sim.soap_log().len();
            let result = moving::move_playback_within(
                &replies,
                &rig.houses,
                &resolve_room(&rig.houses, from).unwrap(),
                &resolve_room(&rig.houses, to).unwrap(),
                Duration::ZERO,
            );
            assert!(result.is_err(), "{generation}: {kind:?}");
            assert!(replies.injected.get());
            source_kept(&rig, since);
        }
    }
}
