//! Native queue/catalog invalidations through real ContentDirectory leases.
//! No queue polling or fabricated playback update is needed for a paused edit.

use fsonos_core::HouseholdState;
use fsonos_core::events::{ContentChanges, RETRY, Service, wanted};
use fsonos_core::live::{Live, LiveConfig, LiveEvent};
use fsonos_core::playback::Playback;
use fsonos_core::reconcile::Reconciler;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::gena::Notify;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::queue::reorder_tracks;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    sim: SimHandle,
    lan: Arc<Lan>,
    host: IpAddr,
    id: PlayerId,
    house: HouseholdState,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        }
        .spawn()
        .unwrap();
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let id = PlayerId(player.uuid.clone());
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
                .with_ssdp_target(sim.ssdp_addr()),
        );
        for name in ["one", "two", "three"] {
            soap::add_uri_to_queue(
                &*lan,
                host,
                &format!("x-file-cifs://example.invalid/{name}.flac"),
                "",
                false,
            )
            .unwrap();
        }
        soap::set_av_transport_uri(&*lan, host, &format!("x-rincon-queue:{}#0", id.0), "").unwrap();
        soap::play(&*lan, host).unwrap();
        soap::pause(&*lan, host).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&*lan, host).unwrap());
        Self {
            sim,
            lan,
            host,
            id,
            house,
        }
    }

    fn listen(&self) -> EventSink {
        let local = self.lan.local_address_toward(self.host).unwrap();
        EventSink::start(SocketAddr::new(local, 0)).unwrap()
    }

    fn reorder(&self, first: u32, before: u32) {
        let queue = browse_snapshot(&*self.lan, self.host, QUEUE).unwrap();
        reorder_tracks(&*self.lan, self.host, first, 1, before, queue.update_id).unwrap();
    }
}

fn receive(sink: &EventSink) -> Notify {
    sink.recv_timeout(Duration::from_secs(5))
        .expect("ContentDirectory did not notify")
}

#[test]
fn paused_same_length_edits_and_catalog_changes_are_distinct_native_events() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let now = Instant::now();
        let mut rec = Reconciler::new(RETRY, RETRY, now);
        rec.households.push(f.house.clone());
        let want = wanted(&rec.households)
            .into_iter()
            .find(|w| w.service == Service::ContentDirectory)
            .unwrap();
        let sink = f.listen();
        let callback = |service: Service| sink.callback_url(service.tag());
        let subscribed = rec.subscriptions.sync(&*f.lan, &[want], callback, now);
        assert_eq!(subscribed.subscribed, 1);
        assert!(subscribed.failed.is_empty());
        let mut playback = Playback::default();
        let initial = receive(&sink);
        let report = rec
            .on_notify(&*f.lan, &mut playback, &initial, callback, now)
            .unwrap()
            .unwrap();
        assert_eq!(report.content, ContentChanges::ALL);
        assert!(report.changes.is_empty());

        let before = soap::get_position_info(&*f.lan, f.host).unwrap();
        let state = soap::get_transport_info(&*f.lan, f.host).unwrap();
        f.reorder(3, 2); // Keep selected occurrence one, total length and pause unchanged.
        let changed = receive(&sink);
        let calls = f.sim.soap_log().len();
        let report = rec
            .on_notify(&*f.lan, &mut playback, &changed, callback, now)
            .unwrap()
            .unwrap();
        assert_eq!(
            report.content,
            ContentChanges {
                queue: true,
                ..ContentChanges::default()
            }
        );
        assert!(report.changes.is_empty());
        assert_eq!(
            f.sim.soap_log().len(),
            calls,
            "invalidation itself performs no SOAP reads or writes"
        );
        assert!(
            playback.of(&f.id).is_none(),
            "catalog events cannot invent playback state"
        );
        assert_eq!(soap::get_position_info(&*f.lan, f.host).unwrap(), before);
        assert_eq!(soap::get_transport_info(&*f.lan, f.host).unwrap(), state);
        assert_eq!(
            browse_snapshot(&*f.lan, f.host, QUEUE)
                .unwrap()
                .objects
                .len(),
            3
        );
        assert!(
            rec.on_notify(&*f.lan, &mut playback, &changed, callback, now)
                .unwrap()
                .is_none()
        );
        assert!(
            rec.on_notify(&*f.lan, &mut playback, &initial, callback, now)
                .unwrap()
                .is_none()
        );

        f.sim
            .retain_favorites(generation, |uri| !uri.contains("spotify"))
            .unwrap();
        let favorite = receive(&sink);
        let report = rec
            .on_notify(&*f.lan, &mut playback, &favorite, callback, now)
            .unwrap()
            .unwrap();
        assert_eq!(
            report.content,
            ContentChanges {
                favorites: true,
                ..ContentChanges::default()
            }
        );
        soap::save_queue(&*f.lan, f.host, "Saved test queue").unwrap();
        let saved = receive(&sink);
        let report = rec
            .on_notify(&*f.lan, &mut playback, &saved, callback, now)
            .unwrap()
            .unwrap();
        assert_eq!(
            report.content,
            ContentChanges {
                playlists: true,
                ..ContentChanges::default()
            }
        );
        rec.subscriptions.unsubscribe_all(&*f.lan);
    }
}

#[test]
fn native_content_leases_renew_recover_gaps_and_reject_malformed_or_retired_streams() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let now = Instant::now();
        let mut rec = Reconciler::new(RETRY, RETRY, now);
        rec.households.push(f.house.clone());
        let want = wanted(&rec.households)
            .into_iter()
            .find(|w| w.service == Service::ContentDirectory)
            .unwrap();
        let sink = f.listen();
        let callback = |service: Service| sink.callback_url(service.tag());
        rec.subscriptions.sync(&*f.lan, &[want], callback, now);
        let mut playback = Playback::default();
        let initial = receive(&sink);
        rec.on_notify(&*f.lan, &mut playback, &initial, callback, now)
            .unwrap()
            .unwrap();
        let due = rec.subscriptions.next_due().unwrap();
        assert_eq!(
            rec.subscriptions.renew_due(&*f.lan, callback, due).renewed,
            1
        );
        f.reorder(3, 2);
        let after_renewal = receive(&sink);
        assert_eq!(after_renewal.sid, initial.sid);
        let report = rec
            .on_notify(&*f.lan, &mut playback, &after_renewal, callback, due)
            .unwrap()
            .unwrap();
        assert_eq!(
            report.content,
            ContentChanges {
                queue: true,
                ..ContentChanges::default()
            }
        );

        // The missed favorite update must invalidate its catalog even if the
        // next event concerns only a queue counter.
        f.sim
            .retain_favorites(generation, |uri| !uri.contains("spotify"))
            .unwrap();
        let lost = receive(&sink);
        f.reorder(3, 2);
        let gap = receive(&sink);
        let report = rec
            .on_notify(&*f.lan, &mut playback, &gap, callback, due)
            .unwrap()
            .unwrap();
        assert_eq!(report.content, ContentChanges::ALL);
        assert_eq!(report.events.resubscribed, 1);
        let replacement = receive(&sink);
        assert_ne!(replacement.sid, initial.sid);
        assert_eq!(
            rec.on_notify(&*f.lan, &mut playback, &replacement, callback, due)
                .unwrap()
                .unwrap()
                .content,
            ContentChanges::ALL
        );
        assert!(
            rec.on_notify(&*f.lan, &mut playback, &lost, callback, due)
                .unwrap()
                .is_none()
        );

        f.reorder(3, 2);
        let mut malformed = receive(&sink);
        malformed
            .properties
            .iter_mut()
            .find(|(name, _)| name == "ContainerUpdateIDs")
            .unwrap()
            .1 = "Q:0,broken".into();
        assert!(
            rec.on_notify(&*f.lan, &mut playback, &malformed, callback, due)
                .is_err()
        );
        let restored = receive(&sink);
        assert_ne!(restored.sid, replacement.sid);
        assert_eq!(
            rec.on_notify(&*f.lan, &mut playback, &restored, callback, due)
                .unwrap()
                .unwrap()
                .content,
            ContentChanges::ALL
        );
        assert!(
            rec.on_notify(&*f.lan, &mut playback, &malformed, callback, due)
                .unwrap()
                .is_none()
        );
        assert!(playback.of(&f.id).is_none());
        rec.subscriptions.unsubscribe_all(&*f.lan);
    }
}

#[test]
fn live_model_publishes_an_external_paused_reorder_without_a_survey() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let mut config = LiveConfig::new(vec![f.host]);
        config.interval = Duration::from_secs(300);
        let live = Live::start(Arc::clone(&f.lan), config);
        let incoming = live.subscribe();
        assert!(
            live.wait_ready(Duration::from_secs(10)),
            "{:?}",
            live.snapshot().last_error
        );
        // Consume the initial lease's broad invalidation before the edit.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = incoming
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("initial content invalidation");
            if matches!(event, LiveEvent::ContentChanged { player, changes } if player == f.id && changes == ContentChanges::ALL)
            {
                break;
            }
        }
        f.reorder(3, 2);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = incoming
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("live queue invalidation");
            if let LiveEvent::ContentChanged { player, changes } = event {
                assert_eq!(player, f.id);
                assert_eq!(
                    changes,
                    ContentChanges {
                        queue: true,
                        ..ContentChanges::default()
                    }
                );
                break;
            }
        }
        live.stop();
    }
}
