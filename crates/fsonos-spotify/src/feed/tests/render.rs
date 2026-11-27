//! DJ starts, refills and partial works recover a stale favorite account.
//! The real S1/S2 simulator accepts only its own rendering template.

use super::*;
use fsonos_proto::control as transport;
use fsonos_proto::didl::{spotify_track_uri, xml_escape};

const OWNER: &str = "http://example.invalid/owners-music.flac";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Transient800,
    LostReply,
    MalformedReply,
    LostReplyAndRead,
    Read800AfterAccept,
    Favorites800DuringLearning,
    Preflight800AfterLearning,
    QueueDuringLearning,
    SourceDuringLearning,
    RebootDuringLearning,
}

struct Favorites<'a> {
    rig: &'a Rig,
    reply: String,
    fault: Fault,
    reads: Cell<usize>,
    adds: Cell<usize>,
    accepted: Cell<usize>,
}

impl<'a> Favorites<'a> {
    fn new(rig: &'a Rig, stale: bool, current: bool, fault: Fault) -> Self {
        let generation = if rig.houses[0].players[0].generation == fsonos_types::Generation::S1 {
            1
        } else {
            2
        };
        let valid = rig.sim.render_params(generation).unwrap();
        let mut old = valid.clone();
        old.sn += 100;
        old.cdudn.push_str("-stale");
        let mut equivalent = old.clone();
        equivalent.flags += 1;
        let mut templates = Vec::new();
        if stale {
            // The second observed template differs only in a field omitted
            // from queue DIDL; recovery must skip that equivalent payload.
            templates.extend([&old, &old, &old, &equivalent, &equivalent]);
        }
        if current {
            templates.push(&valid);
        }
        let items: Vec<_> = templates
            .iter()
            .enumerate()
            .map(|(index, params)| {
                let uri = format!("spotify:track:RecoveryFavorite{index}");
                format!(
                    "<item id=\"FV:2/{index}\"><res>{}</res><r:resMD>{}</r:resMD></item>",
                    xml_escape(&spotify_track_uri(&uri, params)),
                    xml_escape(&spotify_track_didl(&uri, "Favorite", params)),
                )
            })
            .collect();
        let didl = format!(
            "<DIDL-Lite xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{}</DIDL-Lite>",
            items.join("")
        );
        let reply = format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
             <Result>{}</Result><NumberReturned>{}</NumberReturned>\
             <TotalMatches>{}</TotalMatches><UpdateID>1</UpdateID>\
             </u:BrowseResponse></s:Body></s:Envelope>",
            xml_escape(&didl),
            items.len(),
            items.len(),
        );
        Self {
            rig,
            reply,
            fault,
            reads: Cell::new(0),
            adds: Cell::new(0),
            accepted: Cell::new(0),
        }
    }

    fn lost(&self) -> ProtoError {
        ProtoError::Network {
            target: self.rig.host().to_string(),
            detail: "injected lost enqueue acknowledgement".into(),
        }
    }
}

impl Transport for Favorites<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "Browse" && body.contains("<ObjectID>FV:2</ObjectID>") {
            self.reads.set(self.reads.get() + 1);
            if self.reads.get() == 2 {
                match self.fault {
                    Fault::Favorites800DuringLearning => {
                        return Err(ProtoError::SoapFault {
                            code: 800,
                            reason: "favorites read refused".into(),
                        });
                    }
                    Fault::QueueDuringLearning => {
                        add_uri_to_queue(&self.rig.lan, host, OWNER, "", false)?;
                    }
                    Fault::SourceDuringLearning => {
                        transport::set_av_transport_uri(&self.rig.lan, host, OWNER, "")?;
                    }
                    Fault::RebootDuringLearning => {
                        self.rig.sim.reboot(ROOM, Duration::ZERO).unwrap();
                    }
                    _ => {}
                }
            }
            return Ok(self.reply.clone());
        }
        if self.fault == Fault::Preflight800AfterLearning
            && self.reads.get() >= 2
            && name == "GetMediaInfo"
        {
            return Err(ProtoError::SoapFault {
                code: 800,
                reason: "retry prerequisite refused".into(),
            });
        }
        if self.accepted.get() > 0 && matches!(name, "Browse" | "GetMediaInfo") {
            match self.fault {
                Fault::LostReplyAndRead => return Err(self.lost()),
                Fault::Read800AfterAccept => {
                    return Err(ProtoError::SoapFault {
                        code: 800,
                        reason: "post-add read refused".into(),
                    });
                }
                _ => {}
            }
        }
        if name == "AddURIToQueue" {
            self.adds.set(self.adds.get() + 1);
            if self.fault == Fault::Transient800 && self.adds.get() == 1 {
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "transient add refusal".into(),
                });
            }
        }
        let reply = self.rig.lan.soap_post(host, path, action, body)?;
        if name == "AddURIToQueue" && soap::parse_response(&reply, name).is_ok() {
            self.accepted.set(self.accepted.get() + 1);
            if self.accepted.get() == 1 {
                match self.fault {
                    Fault::LostReply | Fault::LostReplyAndRead => return Err(self.lost()),
                    Fault::MalformedReply => return Ok("<broken-reply>".into()),
                    _ => {}
                }
            }
        }
        Ok(reply)
    }
}

fn expected(feed: &QueueFeed) -> Vec<String> {
    feed.queued()
        .iter()
        .flat_map(|work| &work.movements)
        .map(|movement| movement.uri.clone())
        .collect()
}

fn observation(rig: &Rig) -> PlayerPlayback {
    let live = control::playback(&rig.lan, &rig.houses, &rig.coordinator).unwrap();
    let media = transport::get_media_info(&rig.lan, rig.host()).unwrap();
    let mut playback = PlayerPlayback::default();
    playback.media_uri = Some(media.uri);
    playback.track_uri = Some(live.position.uri);
    playback.queue_position = Some(live.position.track);
    playback.queue_length = Some(media.tracks);
    playback.transport = Some(live.transport.state);
    playback.transport_status = Some(live.transport.status);
    playback.play_mode = Some("NORMAL".into());
    playback
}

#[test]
fn starts_recover_stale_track_accounts_and_reuse_the_accepted_template_for_whole_works() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        add_uri_to_queue(&rig.lan, rig.host(), OWNER, "", false).unwrap();
        let favorites = Favorites::new(&rig, true, true, Fault::None);
        feed.start(&rig.speakers(&favorites), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(rig.queue(), expected(&feed));
        assert!(feed.queued().iter().all(QueuedWork::is_whole));
        assert_eq!(favorites.reads.get(), 2);
        assert_eq!(favorites.adds.get(), rig.queue().len() + 1);
        assert_eq!(favorites.accepted.get(), rig.queue().len());
        let generation = if s2 { 2 } else { 1 };
        let speakers = rig.speakers(&rig.lan);
        let cached = store
            .render_params(household_of(&speakers).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(cached.0, rig.sim.render_params(generation).unwrap());
        let queue = browse_all(&rig.lan, rig.host(), QUEUE).unwrap();
        assert_eq!(queue[0].res.as_ref().unwrap().uri, OWNER);
        assert_eq!(rig.position(), 2);
        assert_eq!(observation(&rig).transport, Some(TransportState::Playing));
        assert!(plays(&store).is_empty(), "enqueue is not a listen");
    }
}

#[test]
fn an_automatic_refill_recovers_without_restarting_the_current_work() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        feed.start(&rig.speakers(&rig.lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let position = feed.queued().last().unwrap().first().unwrap();
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, position).unwrap();
        transport::seek_position(&rig.lan, rig.host(), 37).unwrap();
        let playback = observation(&rig);
        let queue = rig.queue();
        let marker = rig.sim.soap_log().len();
        let favorites = Favorites::new(&rig, true, true, Fault::None);
        assert_eq!(
            feed.on_playback(
                &rig.speakers(&favorites),
                plan(&pool, MIDNIGHT + 37),
                &mut store,
                &playback,
            )
            .unwrap(),
            1
        );
        assert_eq!(&rig.queue()[..queue.len()], queue);
        assert!(feed.queued().iter().all(QueuedWork::is_whole));
        assert_eq!(favorites.reads.get(), 2);
        assert_eq!(favorites.adds.get(), rig.queue().len() - queue.len() + 1);
        let live = control::playback(&rig.lan, &rig.houses, &rig.coordinator).unwrap();
        assert_eq!(live.position.track, position);
        assert_eq!(live.position.position_secs, Some(37));
        assert_eq!(live.transport.state, TransportState::Playing);
        assert!(
            rig.sim.soap_log()[marker..]
                .iter()
                .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
                .all(|call| call.action == "AddURIToQueue")
        );

        // Removing the favorite later still permits the accepted household
        // cache to feed the next work; the rejected majority was superseded.
        let position = feed.queued().last().unwrap().first().unwrap();
        control::play_queue_from(&rig.lan, &rig.houses, &rig.coordinator, position).unwrap();
        let mut no_favorites = Flaky::new(rig.lan.clone());
        no_favorites.no_favorites = true;
        assert_eq!(
            feed.on_playback(
                &rig.speakers(&no_favorites),
                plan(&pool, MIDNIGHT + 120),
                &mut store,
                &observation(&rig),
            )
            .unwrap(),
            1
        );
        assert!(feed.queued().iter().all(QueuedWork::is_whole));
    }
}

#[test]
fn a_partly_queued_work_recovers_before_the_owners_intervening_music() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        let flaky = Flaky::new(rig.lan.clone());
        flaky.fail_add_in(2);
        assert!(
            feed.start(&rig.speakers(&flaky), plan(&pool, MIDNIGHT), &mut store)
                .is_err()
        );
        let first = feed.queued()[0].movements[0].uri.clone();
        assert_eq!(rig.queue(), [first]);
        add_uri_to_queue(&rig.lan, rig.host(), OWNER, "", false).unwrap();
        let favorites = Favorites::new(&rig, true, true, Fault::None);
        feed.start(
            &rig.speakers(&favorites),
            plan(&pool, MIDNIGHT + 1),
            &mut store,
        )
        .unwrap();
        assert_eq!(rig.queue(), expected(&feed));
        let queue = browse_all(&rig.lan, rig.host(), QUEUE).unwrap();
        let first_work = &feed.queued()[0];
        assert!(first_work.is_whole());
        assert_eq!(
            queue[first_work.movements.len()].res.as_ref().unwrap().uri,
            OWNER
        );
        assert_eq!(favorites.adds.get(), favorites.accepted.get() + 2);
        assert_eq!(
            favorites.reads.get(),
            4,
            "one recovery per completion/refill batch"
        );
        assert_eq!(observation(&rig).transport, Some(TransportState::Playing));
    }
}

#[test]
fn no_alternative_stops_after_one_retry_and_a_transient_800_can_recover() {
    for s2 in [false, true] {
        for transient in [false, true] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            let favorites = Favorites::new(
                &rig,
                !transient,
                transient,
                if transient {
                    Fault::Transient800
                } else {
                    Fault::None
                },
            );
            let result = feed.start(&rig.speakers(&favorites), plan(&pool, MIDNIGHT), &mut store);
            assert_eq!(favorites.reads.get(), 2);
            if transient {
                result.unwrap();
                assert_eq!(rig.queue(), expected(&feed));
                assert_eq!(favorites.adds.get(), favorites.accepted.get() + 1);
            } else {
                assert!(matches!(
                    result,
                    Err(FeedError::Core(CoreError::RenderParamsStale))
                ));
                assert_eq!(favorites.adds.get(), 2);
                assert!(rig.queue().is_empty());
                assert!(!feed.is_active());
            }
        }
    }
}

#[test]
fn acknowledged_by_readback_movements_are_never_replayed_during_account_recovery() {
    for s2 in [false, true] {
        for stale in [false, true] {
            for fault in [Fault::LostReply, Fault::MalformedReply] {
                let rig = Rig::generation(s2);
                let pool = multi_movement_pool();
                let mut store = MemStore::default();
                let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
                let favorites = Favorites::new(&rig, stale, true, fault);
                feed.start(&rig.speakers(&favorites), plan(&pool, MIDNIGHT), &mut store)
                    .unwrap();
                assert_eq!(rig.queue(), expected(&feed));
                assert_eq!(favorites.adds.get(), rig.queue().len() + usize::from(stale));
                assert_eq!(favorites.reads.get(), 1 + usize::from(stale));
            }
        }
    }
}

#[test]
fn unconfirmed_adds_never_trigger_another_account_attempt() {
    for s2 in [false, true] {
        for stale in [false, true] {
            for fault in [Fault::LostReplyAndRead, Fault::Read800AfterAccept] {
                let rig = Rig::generation(s2);
                let pool = multi_movement_pool();
                let mut store = MemStore::default();
                let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
                let favorites = Favorites::new(&rig, stale, true, fault);
                assert!(matches!(
                    feed.start(&rig.speakers(&favorites), plan(&pool, MIDNIGHT), &mut store),
                    Err(FeedError::UncertainEnqueue(_))
                ));
                assert_eq!(favorites.adds.get(), 1 + usize::from(stale));
                assert_eq!(favorites.reads.get(), 1 + usize::from(stale));
                assert_eq!(rig.queue().len(), 1);
                assert!(!feed.is_active());
            }
        }
    }
}

#[test]
fn a_preflight_refusal_is_not_a_render_rejection() {
    for s2 in [false, true] {
        let rig = Rig::generation(s2);
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
        rig.sim.upnp_fault(ROOM, "GetMediaInfo", 800).unwrap();
        let favorites = Favorites::new(&rig, true, true, Fault::None);
        let error = feed
            .start(&rig.speakers(&favorites), plan(&pool, MIDNIGHT), &mut store)
            .unwrap_err();
        assert!(
            matches!(
                error,
                FeedError::Proto(ProtoError::SoapFault { code: 800, .. })
            ),
            "{error}"
        );
        assert_eq!(favorites.adds.get(), 0);
        assert_eq!(favorites.reads.get(), 1);
    }
}

#[test]
fn recovery_read_refusals_keep_their_diagnosis_without_another_add() {
    for s2 in [false, true] {
        for (fault, reason) in [
            (Fault::Favorites800DuringLearning, "favorites read refused"),
            (
                Fault::Preflight800AfterLearning,
                "retry prerequisite refused",
            ),
        ] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            let favorites = Favorites::new(&rig, true, true, fault);
            let error = feed
                .start(&rig.speakers(&favorites), plan(&pool, MIDNIGHT), &mut store)
                .unwrap_err();
            assert!(
                matches!(&error, FeedError::Proto(ProtoError::SoapFault { code: 800, reason: actual }) if actual == reason),
                "{error}"
            );
            assert_eq!(favorites.adds.get(), 1);
            assert_eq!(favorites.reads.get(), 2);
            assert_eq!(favorites.accepted.get(), 0);
            assert!(rig.queue().is_empty());
            assert!(!feed.is_active());
        }
    }
}

#[test]
fn a_confirmed_render_refusal_cannot_hide_a_changed_or_unreadable_coordinator() {
    for s2 in [false, true] {
        let rig = Rig::rooms(s2, &[ROOM, "Other"]);
        let other = PlayerId(rig.sim.player("Other").unwrap().uuid.clone());
        transport::join_group(&rig.lan, rig.host(), &other).unwrap();
        let error = render_refused(&rig.speakers(&rig.lan), CoreError::RenderParamsStale.into());
        assert!(
            matches!(error, FeedError::Core(CoreError::TargetChanged(_))),
            "{error}"
        );

        rig.sim.upnp_fault(ROOM, "GetZoneGroupState", 800).unwrap();
        let error = render_refused(&rig.speakers(&rig.lan), CoreError::RenderParamsStale.into());
        assert!(
            matches!(
                error,
                FeedError::Core(CoreError::Proto(ProtoError::SoapFault { code: 800, .. }))
            ),
            "{error}"
        );
    }
}

#[test]
fn relearning_cannot_rebase_an_enqueue_onto_an_owner_edit_or_reboot() {
    for s2 in [false, true] {
        for fault in [
            Fault::QueueDuringLearning,
            Fault::SourceDuringLearning,
            Fault::RebootDuringLearning,
        ] {
            let rig = Rig::generation(s2);
            let pool = multi_movement_pool();
            let mut store = MemStore::default();
            let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);
            let favorites = Favorites::new(&rig, true, true, fault);
            assert!(
                feed.start(&rig.speakers(&favorites), plan(&pool, MIDNIGHT), &mut store)
                    .is_err(),
                "{fault:?}"
            );
            assert_eq!(favorites.adds.get(), 1, "{fault:?}");
            assert_eq!(favorites.reads.get(), 2);
            assert_eq!(favorites.accepted.get(), 0);
            assert!(!feed.is_active());
            if fault == Fault::QueueDuringLearning {
                let queue = browse_all(&rig.lan, rig.host(), QUEUE).unwrap();
                assert_eq!(queue.len(), 1);
                assert_eq!(queue[0].res.as_ref().unwrap().uri, OWNER);
            }
        }
    }
}
