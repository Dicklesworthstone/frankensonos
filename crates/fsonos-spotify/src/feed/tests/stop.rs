//! Explicit DJ stop respects source handoffs and the selected queue occurrence.
//! The speakers are real S1/S2 simulator HTTP endpoints; injected owner actions
//! bypass the observed transport so assertions count only the DJ's own writes.

use std::cell::RefCell;

use super::*;
use fsonos_core::playback::PlaybackObservation;
use fsonos_proto::control as transport;

const ANNOUNCEMENT: &str = "http://example.invalid/announcement.wav";

fn started(s2: bool) -> (Rig, QueueFeed) {
    let rig = Rig::generation(s2);
    let pool = multi_movement_pool();
    let mut store = MemStore::default();
    let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
    feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
        .unwrap();
    (rig, feed)
}

fn switch_source(lan: &SimLan, host: IpAddr, uri: &str) {
    transport::set_av_transport_uri(lan, host, uri, "").unwrap();
    transport::play(lan, host).unwrap();
}

fn owner_adds(rig: &Rig, s2: bool, uri: &str) {
    let params = rig.sim.render_params(if s2 { 2 } else { 1 }).unwrap();
    add_uri_to_queue(
        &rig.lan,
        rig.host(),
        &spotify_queue_uri(uri),
        &spotify_track_didl(uri, "Owner", &params),
        false,
    )
    .unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StopReply {
    Normal,
    Refused,
    Lost,
    LostAndUnreadable,
}

enum Edit<'a> {
    Queue,
    Source,
    Selection(u32),
    Reboot(&'a SimHandle),
}

struct Observed<'a> {
    inner: &'a SimLan,
    writes: RefCell<Vec<String>>,
    stop_reply: StopReply,
    missing_field: Option<&'static str>,
    edit: Option<Edit<'a>>,
    edit_on_browse: usize,
    browses: Cell<usize>,
    edited: Cell<bool>,
}

impl<'a> Observed<'a> {
    fn new(inner: &'a SimLan) -> Self {
        Self {
            inner,
            writes: RefCell::new(Vec::new()),
            stop_reply: StopReply::Normal,
            missing_field: None,
            edit: None,
            edit_on_browse: 1,
            browses: Cell::new(0),
            edited: Cell::new(false),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.writes.borrow().clone()
    }
}

impl Transport for Observed<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.borrow_mut().push(name.to_owned());
        }
        if name == "Stop" && self.stop_reply == StopReply::Refused {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "stop refused by simulator test".into(),
            });
        }
        if name == "GetTransportInfo" && self.stop_reply == StopReply::LostAndUnreadable {
            return Err(ProtoError::Network {
                target: "simulated stop read-back".into(),
                detail: "the stop outcome cannot be observed".into(),
            });
        }
        let mut response = self.inner.soap_post(host, path, action, body)?;
        if name == "Stop"
            && matches!(self.stop_reply, StopReply::Lost | StopReply::LostAndUnreadable)
        {
            return Err(ProtoError::Network {
                target: "simulated stop reply".into(),
                detail: "the command succeeded but its reply was lost".into(),
            });
        }
        if let Some(field) = self.missing_field {
            response = response
                .replace(&format!("<{field}>"), "<MissingField>")
                .replace(&format!("</{field}>"), "</MissingField>");
        }
        if name == "Browse" {
            self.browses.set(self.browses.get() + 1);
        }
        if name == "Browse"
            && self.browses.get() == self.edit_on_browse
            && let Some(edit) = &self.edit
            && !self.edited.replace(true)
        {
            // Change the speaker after it produced a queue proof response.
            match edit {
                Edit::Queue => {
                    add_uri_to_queue(
                        self.inner,
                        host,
                        "http://example.invalid/owners-track.wav",
                        "",
                        false,
                    )?;
                }
                Edit::Source => switch_source(self.inner, host, ANNOUNCEMENT),
                Edit::Selection(position) => transport::seek_track(self.inner, host, *position)?,
                Edit::Reboot(sim) => sim.reboot(ROOM, Duration::ZERO).unwrap(),
            }
        }
        Ok(response)
    }
}

#[test]
fn stop_stops_a_confirmed_owned_occurrence_without_changing_its_queue() {
    for s2 in [false, true] {
        for paused in [false, true] {
            let (rig, mut feed) = started(s2);
            if paused {
                transport::pause(&rig.lan, rig.host()).unwrap();
            }
            let queue = rig.queue();
            let observed = Observed::new(&rig.lan);
            feed.stop(&rig.speakers(&observed)).unwrap();
            assert_eq!(observed.writes(), ["Stop"]);
            assert!(!feed.is_active());
            assert!(feed.snapshot().is_none());
            assert_eq!(rig.queue(), queue);
            assert_eq!(
                transport::get_transport_info(&rig.lan, rig.host())
                    .unwrap()
                    .state,
                TransportState::Stopped,
            );
        }
    }
}

#[test]
fn stop_releases_radio_connect_and_announcements_without_interrupting_them() {
    for s2 in [false, true] {
        for source in [
            ANNOUNCEMENT,
            "x-rincon-mp3radio:http://example.invalid/radio",
            "x-sonos-vli:test-player:2,spotify:test-session",
        ] {
            let (rig, mut feed) = started(s2);
            let queue = rig.queue();
            switch_source(&rig.lan, rig.host(), source);
            let observed = Observed::new(&rig.lan);
            feed.stop(&rig.speakers(&observed)).unwrap();
            assert!(!feed.is_active());
            assert!(feed.snapshot().is_none());
            assert!(observed.writes().is_empty());
            assert_eq!(rig.queue(), queue);
            assert_eq!(
                transport::get_media_info(&rig.lan, rig.host())
                    .unwrap()
                    .uri,
                source,
            );
            assert_eq!(
                transport::get_transport_info(&rig.lan, rig.host())
                    .unwrap()
                    .state,
                TransportState::Playing,
            );
            assert!(matches!(
                feed.stop(&rig.speakers(&observed)),
                Err(FeedError::Inactive)
            ));
            assert!(observed.writes().is_empty());
        }
    }
}

#[test]
fn stop_leaves_a_replacement_queue_playing_even_when_its_first_track_matches() {
    for s2 in [false, true] {
        for matching_first in [false, true] {
            let (rig, mut feed) = started(s2);
            assert!(feed.queued()[0].movements.len() > 1);
            let owner = if matching_first {
                feed.queued()[0].movements[0].uri.clone()
            } else {
                "spotify:track:OwnersReplacement".into()
            };
            remove_all_tracks_from_queue(&rig.lan, rig.host()).unwrap();
            owner_adds(&rig, s2, &owner);
            control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, 1).unwrap();
            let observed = Observed::new(&rig.lan);
            feed.stop(&rig.speakers(&observed)).unwrap();
            assert!(!feed.is_active());
            assert!(observed.writes().is_empty());
            assert_eq!(rig.queue(), [owner]);
            assert_eq!(
                transport::get_transport_info(&rig.lan, rig.host())
                    .unwrap()
                    .state,
                TransportState::Playing,
            );
        }
    }
}

#[test]
fn stop_does_not_claim_an_owners_later_copy_of_the_current_track() {
    for s2 in [false, true] {
        let (rig, mut feed) = started(s2);
        let copy = feed.queued()[0].movements[0].uri.clone();
        owner_adds(&rig, s2, &copy);
        let queue = rig.queue();
        let position = u32::try_from(queue.len()).unwrap();
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, position).unwrap();
        let observed = Observed::new(&rig.lan);
        feed.stop(&rig.speakers(&observed)).unwrap();
        assert!(!feed.is_active());
        assert!(observed.writes().is_empty());
        assert_eq!(rig.queue(), queue);
        assert_eq!(rig.position(), position);
        assert_eq!(
            transport::get_transport_info(&rig.lan, rig.host())
                .unwrap()
                .state,
            TransportState::Playing,
        );
    }
}

#[test]
fn stop_releases_without_writes_if_the_live_group_disagrees_with_the_snapshot() {
    let (rig, mut feed) = started(true);
    let mut stale = rig.houses.clone();
    stale[0].groups[0]
        .members
        .push(PlayerId("RINCON_other_listener".into()));
    let observed = Observed::new(&rig.lan);
    feed.stop(&Speakers {
        transport: &observed,
        households: &stale,
        coordinator: &rig.coordinator,
    })
    .unwrap();
    assert!(!feed.is_active());
    assert!(observed.writes().is_empty());
    assert_eq!(
        transport::get_transport_info(&rig.lan, rig.host())
            .unwrap()
            .state,
        TransportState::Playing,
    );
}

#[test]
fn stop_does_not_write_after_queue_source_selection_or_boot_changes_during_proof() {
    for s2 in [false, true] {
        for change in 0..4 {
            let (rig, mut feed) = started(s2);
            let edit = match change {
                0 => Edit::Queue,
                1 => Edit::Source,
                2 => Edit::Selection(feed.queued()[0].movements[1].position.unwrap()),
                _ => Edit::Reboot(&rig.sim),
            };
            let observed = Observed {
                edit: Some(edit),
                ..Observed::new(&rig.lan)
            };
            feed.stop(&rig.speakers(&observed)).unwrap();
            assert!(observed.edited.get(), "the concurrent owner change happened");
            assert!(observed.writes().is_empty());
            assert!(!feed.is_active());
            assert_eq!(
                transport::get_transport_info(&rig.lan, rig.host())
                    .unwrap()
                    .state,
                TransportState::Playing,
            );
        }
    }
}

#[test]
fn stop_checks_selection_and_boot_again_after_the_final_queue_read() {
    for s2 in [false, true] {
        for reboot in [false, true] {
            let (rig, mut feed) = started(s2);
            owner_adds(&rig, s2, "spotify:track:OwnersLaterItem");
            let queue = rig.queue();
            let owner_position = u32::try_from(queue.len()).unwrap();
            let observed = Observed {
                edit: Some(if reboot {
                    Edit::Reboot(&rig.sim)
                } else {
                    Edit::Selection(owner_position)
                }),
                edit_on_browse: 2,
                ..Observed::new(&rig.lan)
            };
            feed.stop(&rig.speakers(&observed)).unwrap();
            assert!(observed.edited.get());
            assert!(observed.writes().is_empty());
            assert!(!feed.is_active());
            assert_eq!(rig.queue(), queue);
            if !reboot {
                assert_eq!(rig.position(), owner_position);
            }
            assert_eq!(
                transport::get_transport_info(&rig.lan, rig.host())
                    .unwrap()
                    .state,
                TransportState::Playing,
            );
        }
    }
}

#[test]
fn stop_read_failures_preserve_the_session_and_never_send_a_write() {
    for action in [
        "GetMediaInfo",
        "GetPositionInfo",
        "GetZoneGroupState",
        "Browse",
    ] {
        let (rig, mut feed) = started(true);
        let before = feed.snapshot();
        let queue = rig.queue();
        rig.sim.upnp_fault(ROOM, action, 701).unwrap();
        let observed = Observed::new(&rig.lan);
        assert!(feed.stop(&rig.speakers(&observed)).is_err());
        assert_eq!(feed.snapshot(), before);
        assert!(observed.writes().is_empty());
        rig.sim.clear_faults(ROOM).unwrap();
        assert_eq!(rig.queue(), queue);
        feed.stop(&rig.speakers(&observed)).unwrap();
        assert_eq!(observed.writes(), ["Stop"]);
    }
}

#[test]
fn stop_missing_source_or_position_fields_are_read_errors_not_ownership_loss() {
    for field in ["CurrentURI", "Track", "TrackURI"] {
        let (rig, mut feed) = started(true);
        let before = feed.snapshot();
        let observed = Observed {
            missing_field: Some(field),
            ..Observed::new(&rig.lan)
        };
        assert!(matches!(
            feed.stop(&rig.speakers(&observed)),
            Err(FeedError::Proto(ProtoError::Malformed(_)))
        ));
        assert_eq!(feed.snapshot(), before);
        assert!(observed.writes().is_empty());
    }
}

#[test]
fn stop_refusals_and_uncertain_replies_are_never_replayed() {
    for s2 in [false, true] {
        for reply in [
            StopReply::Refused,
            StopReply::Lost,
            StopReply::LostAndUnreadable,
        ] {
            let (rig, mut feed) = started(s2);
            let before = feed.snapshot();
            let queue = rig.queue();
            let observed = Observed {
                stop_reply: reply,
                ..Observed::new(&rig.lan)
            };
            let result = feed.stop(&rig.speakers(&observed));
            assert_eq!(observed.writes(), ["Stop"]);
            assert_eq!(rig.queue(), queue);
            if reply == StopReply::Lost {
                result.unwrap();
            } else {
                assert!(result.is_err());
            }
            if reply == StopReply::Refused {
                assert_eq!(feed.snapshot(), before);
            } else {
                assert!(!feed.is_active());
                assert!(feed.snapshot().is_none());
            }
            assert_eq!(
                transport::get_transport_info(&rig.lan, rig.host())
                    .unwrap()
                    .state,
                if reply == StopReply::Refused {
                    TransportState::Playing
                } else {
                    TransportState::Stopped
                },
            );
        }
    }
}

#[test]
fn an_uncertain_stop_disarms_pending_exhaustion_before_another_playback_event() {
    for s2 in [false, true] {
        let (rig, mut feed) = started(s2);
        feed.dry_end = feed.left_at();
        let queue = rig.queue();
        let observed = Observed {
            stop_reply: StopReply::LostAndUnreadable,
            ..Observed::new(&rig.lan)
        };
        assert!(feed.stop(&rig.speakers(&observed)).is_err());
        assert_eq!(observed.writes(), ["Stop"]);
        assert!(!feed.is_active());
        assert!(feed.snapshot().is_none());
        assert!(feed.dry_end.is_none());

        let observation = PlaybackObservation {
            transport: transport::get_transport_info(&rig.lan, rig.host()).unwrap(),
            position: transport::get_position_info(&rig.lan, rig.host()).unwrap(),
            media: transport::get_media_info(&rig.lan, rig.host()).unwrap(),
        };
        let mut playback = Playback::default();
        playback.apply_observation(&rig.coordinator, &observation, Instant::now());
        let event = playback.of(&rig.coordinator).unwrap();
        assert_eq!(event.transport, Some(TransportState::Stopped));
        let pool = multi_movement_pool();
        assert_eq!(
            feed.on_playback(
                &rig.speakers(&NoIo),
                plan(&pool, MIDNIGHT + 1),
                &mut MemStore::default(),
                event,
            )
            .unwrap(),
            0,
        );
        assert_eq!(rig.queue(), queue);
    }
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("an inactive or different feed cannot inspect or stop this player")
    }
}

#[test]
fn stop_rejects_an_inactive_feed_or_a_different_coordinator_without_io() {
    let (rig, mut feed) = started(true);
    let before = feed.snapshot();
    let other = PlayerId("RINCON_other_coordinator".into());
    assert!(matches!(
        feed.stop(&Speakers {
            transport: &NoIo,
            households: &rig.houses,
            coordinator: &other,
        }),
        Err(FeedError::Inactive)
    ));
    assert_eq!(feed.snapshot(), before);
    feed.release();
    assert!(matches!(
        feed.stop(&rig.speakers(&NoIo)),
        Err(FeedError::Inactive)
    ));
}
