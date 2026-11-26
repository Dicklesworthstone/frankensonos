//! Complete queue round trips through the stream's real cleanup callback.

use super::{NEW, Rig, STREAM, restore};
use fsonos_core::snapshot::{self, Aspect};
use fsonos_core::{control, resolve_room};
use fsonos_proto::content::{self, BrowseSnapshot};
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::{PlayMode, TransportState};
use std::cell::Cell;
use std::net::IpAddr;

fn queued(s2: bool, state: TransportState) -> Rig {
    let mut rig = Rig::new(s2);
    rig.queue();
    control::queue_spotify_tracks(
        &rig.lan,
        &rig.houses,
        &rig.before.coordinator,
        &[("spotify:track:0SimSnapA0000000000001", "First again")],
    )
    .unwrap()
    .unwrap();
    soap::seek_track(&rig.lan, rig.host, 3).unwrap();
    set_play_mode(&rig.lan, rig.host, PlayMode::ShuffleNoRepeat).unwrap();
    match state {
        TransportState::Paused => soap::pause(&rig.lan, rig.host).unwrap(),
        TransportState::Stopped => soap::stop(&rig.lan, rig.host).unwrap(),
        TransportState::Playing => {}
        _ => unreachable!(),
    }
    soap::seek_position(&rig.lan, rig.host, 37).unwrap();
    rig.before = snapshot::capture(&rig.lan, &rig.houses, &rig.before.coordinator, 3_000).unwrap();
    rig
}

fn contents(rig: &Rig) -> BrowseSnapshot {
    content::browse_snapshot(&rig.lan, rig.host, content::QUEUE).unwrap()
}

fn interrupt(rig: &Rig) {
    rig.stream(true);
    // Make restoring the captured mode require its own write.
    set_play_mode(&rig.lan, rig.host, PlayMode::Normal).unwrap();
}

fn assert_queue_restored(rig: &Rig, queue: &BrowseSnapshot, playing: bool) {
    assert_eq!(
        &contents(rig),
        queue,
        "queue contents and revision are retained"
    );
    let position = soap::get_position_info(&rig.lan, rig.host).unwrap();
    assert_eq!((position.track, position.position_secs), (3, Some(37)));
    assert_eq!(
        get_play_mode(&rig.lan, rig.host).unwrap(),
        PlayMode::ShuffleNoRepeat
    );
    let actual = soap::get_transport_info(&rig.lan, rig.host).unwrap().state;
    if playing {
        assert_eq!(actual, TransportState::Playing);
    } else {
        assert!(
            matches!(actual, TransportState::Paused | TransportState::Stopped),
            "{actual:?}"
        );
    }
}

#[test]
fn cleanup_restores_duplicate_occurrence_position_mode_and_prior_activity() {
    for s2 in [false, true] {
        for state in [
            TransportState::Playing,
            TransportState::Paused,
            TransportState::Stopped,
        ] {
            let rig = queued(s2, state);
            let queue = contents(&rig);
            assert_eq!(queue.objects.len(), 3);
            interrupt(&rig);
            let first = rig.sim.soap_log().len();
            let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, true).unwrap();
            assert!(report.skipped.is_empty(), "{report:?}");
            assert!(report.restored.contains(&Aspect::Source), "{report:?}");
            assert!(report.restored.contains(&Aspect::Position), "{report:?}");
            assert_queue_restored(&rig, &queue, state == TransportState::Playing);
            let log = rig.sim.soap_log();
            let writes: Vec<_> = log[first..]
                .iter()
                .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
                .collect();
            assert!(writes.iter().all(|entry| {
                entry.room == "Alpha"
                    && matches!(
                        entry.action.as_str(),
                        "Pause" | "Stop" | "SetAVTransportURI" | "SetPlayMode" | "Seek" | "Play"
                    )
            }));
            assert_eq!(
                writes.iter().filter(|entry| entry.action == "Play").count(),
                usize::from(state == TransportState::Playing)
            );
        }
    }
}

#[test]
fn a_manual_pause_restores_the_queue_without_resuming_it() {
    for s2 in [false, true] {
        let rig = queued(s2, TransportState::Playing);
        let queue = contents(&rig);
        interrupt(&rig);
        soap::pause(&rig.lan, rig.host).unwrap();
        let first = rig.sim.soap_log().len();
        let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, true).unwrap();
        assert_queue_restored(&rig, &queue, false);
        assert!(report.restored.contains(&Aspect::Source), "{report:?}");
        assert!(report.restored.contains(&Aspect::Position), "{report:?}");
        assert!(
            report
                .skipped
                .iter()
                .any(|(aspect, _)| *aspect == Aspect::Transport),
            "{report:?}"
        );
        assert!(
            rig.sim.soap_log()[first..]
                .iter()
                .all(|entry| entry.action != "Play")
        );
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Refused,
    RefusedBody,
    Lost,
    Malformed,
    Unapplied,
    Ignored,
}

struct Replies<'a> {
    rig: &'a Rig,
    reply: Reply,
    selections: Cell<usize>,
}

impl Replies<'_> {
    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost cleanup selection reply".into(),
        }
    }
}

impl Transport for Replies<'_> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.rig.lan.http_get(url)
    }

    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if !action.ends_with("#SetAVTransportURI\"") {
            return self.rig.lan.soap_post(host, path, action, body);
        }
        let attempt = self.selections.get() + 1;
        self.selections.set(attempt);
        if attempt == 1 {
            match self.reply {
                Reply::Refused => {
                    return Err(ProtoError::SoapFault {
                        code: 402,
                        reason: "injected transient source refusal".into(),
                    });
                }
                Reply::RefusedBody => {
                    return Ok(
                        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                         <s:Body><s:Fault><detail><UPnPError>\
                         <errorCode>402</errorCode></UPnPError></detail></s:Fault>\
                         </s:Body></s:Envelope>"
                            .into(),
                    );
                }
                Reply::Unapplied => return Err(Self::lost(host)),
                Reply::Ignored => {
                    return Ok(
                        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                         <s:Body><u:SetAVTransportURIResponse xmlns:u=\"urn:x\"/>\
                         </s:Body></s:Envelope>"
                            .into(),
                    );
                }
                _ => {}
            }
        }
        let response = self.rig.lan.soap_post(host, path, action, body)?;
        if attempt == 1 {
            match self.reply {
                Reply::Lost => return Err(Self::lost(host)),
                Reply::Malformed => return Ok("<broken-reply>".into()),
                _ => {}
            }
        }
        Ok(response)
    }
}

#[test]
fn queue_cleanup_recovers_refused_and_lost_selection_replies() {
    for s2 in [false, true] {
        for reply in [
            Reply::Refused,
            Reply::RefusedBody,
            Reply::Lost,
            Reply::Malformed,
        ] {
            let rig = queued(s2, TransportState::Playing);
            let queue = contents(&rig);
            interrupt(&rig);
            let transport = Replies {
                rig: &rig,
                reply,
                selections: Cell::new(0),
            };
            let report = restore(&transport, &rig.houses, &rig.before, STREAM, true).unwrap();
            assert!(report.skipped.is_empty(), "{report:?}");
            assert_queue_restored(&rig, &queue, true);
            let expected = if matches!(reply, Reply::Refused | Reply::RefusedBody) {
                2
            } else {
                1
            };
            assert_eq!(transport.selections.get(), expected);
        }
    }
}

#[test]
fn unconfirmed_queue_selection_never_authorizes_mode_seek_or_resume() {
    for reply in [Reply::Unapplied, Reply::Ignored] {
        let rig = queued(true, TransportState::Playing);
        interrupt(&rig);
        let first = rig.sim.soap_log().len();
        let transport = Replies {
            rig: &rig,
            reply,
            selections: Cell::new(0),
        };
        let report = restore(&transport, &rig.houses, &rig.before, STREAM, true).unwrap();
        for aspect in [Aspect::Source, Aspect::Position, Aspect::Transport] {
            assert!(report.skipped.iter().any(|(failed, _)| *failed == aspect));
        }
        assert_eq!(transport.selections.get(), 1);
        assert_eq!(
            soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
            STREAM
        );
        rig.readonly_since(first);
    }
}

#[derive(Clone, Copy)]
enum Change {
    QueueBeforeSelection,
    Source,
    Queue,
    Group,
}

struct Changed<'a> {
    rig: &'a Rig,
    change: Change,
    selected: Cell<bool>,
    media_reads: Cell<usize>,
    changed: Cell<bool>,
    after: Cell<usize>,
}

impl Changed<'_> {
    fn interfere(&self) -> Result<(), ProtoError> {
        match self.change {
            Change::QueueBeforeSelection => {
                soap::play_from_queue(&self.rig.lan, self.rig.host, &self.rig.before.coordinator)?;
                soap::seek_track(&self.rig.lan, self.rig.host, 1)?;
                soap::play(&self.rig.lan, self.rig.host)?;
            }
            Change::Source => {
                soap::set_av_transport_uri(&self.rig.lan, self.rig.host, NEW, "")?;
                soap::play(&self.rig.lan, self.rig.host)?;
            }
            Change::Queue => {
                soap::add_uri_to_queue(&self.rig.lan, self.rig.host, NEW, "", false)?;
            }
            Change::Group => {
                let gamma = &resolve_room(&self.rig.houses, "Gamma").unwrap().player.id;
                soap::join_group(
                    &self.rig.lan,
                    self.rig.sim.player("Beta").unwrap().ip,
                    gamma,
                )?;
            }
        }
        self.changed.set(true);
        self.after.set(self.rig.sim.soap_log().len());
        Ok(())
    }
}

impl Transport for Changed<'_> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.rig.lan.http_get(url)
    }

    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let media = action.ends_with("#GetMediaInfo\"");
        if media && !self.selected.get() && matches!(self.change, Change::QueueBeforeSelection) {
            self.media_reads.set(self.media_reads.get() + 1);
            if self.media_reads.get() == 2 && !self.changed.get() {
                self.interfere()?;
            }
        }
        let response = self.rig.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#SetAVTransportURI\"") {
            self.selected.set(true);
            self.media_reads.set(0);
        } else if media && self.selected.get() && !self.changed.get() {
            self.media_reads.set(self.media_reads.get() + 1);
            if self.media_reads.get() == 2 {
                // Return the old response from the final source-confirmation
                // read. The following mode authorization must observe the
                // owner's intervening change before any dependent write.
                self.interfere()?;
            }
        }
        Ok(response)
    }
}

#[test]
fn independent_queue_selection_never_counts_as_our_cleanup_write() {
    for s2 in [false, true] {
        let rig = queued(s2, TransportState::Playing);
        interrupt(&rig);
        let transport = Changed {
            rig: &rig,
            change: Change::QueueBeforeSelection,
            selected: Cell::new(false),
            media_reads: Cell::new(0),
            changed: Cell::new(false),
            after: Cell::new(0),
        };
        let report = restore(&transport, &rig.houses, &rig.before, STREAM, true).unwrap();
        assert!(transport.changed.get(), "owner selected the old queue");
        assert!(!transport.selected.get(), "cleanup sent no source write");
        assert!(report.restored.is_empty(), "{report:?}");
        rig.readonly_since(transport.after.get());
        assert_eq!(
            soap::get_position_info(&rig.lan, rig.host).unwrap().track,
            1
        );
        assert_eq!(get_play_mode(&rig.lan, rig.host).unwrap(), PlayMode::Normal);
    }
}

#[test]
fn source_queue_and_group_are_rechecked_after_our_queue_selection() {
    for s2 in [false, true] {
        for change in [Change::Source, Change::Queue, Change::Group] {
            let rig = queued(s2, TransportState::Playing);
            interrupt(&rig);
            let transport = Changed {
                rig: &rig,
                change,
                selected: Cell::new(false),
                media_reads: Cell::new(0),
                changed: Cell::new(false),
                after: Cell::new(0),
            };
            let report = restore(&transport, &rig.houses, &rig.before, STREAM, true).unwrap();
            assert!(
                transport.selected.get(),
                "cleanup selected the original queue"
            );
            assert!(
                transport.changed.get(),
                "owner changed the confirmed source"
            );
            assert!(report.restored.is_empty(), "{report:?}");
            for aspect in [Aspect::Source, Aspect::Position, Aspect::Transport] {
                assert!(report.skipped.iter().any(|(failed, _)| *failed == aspect));
            }
            rig.readonly_since(transport.after.get());
            assert_eq!(get_play_mode(&rig.lan, rig.host).unwrap(), PlayMode::Normal);
        }
    }
}
