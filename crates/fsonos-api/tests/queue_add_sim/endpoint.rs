//! Reviewed queue undo retains the verified endpoint through nested wrappers.

use super::*;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::soap::{ZONE_GROUP_TOPOLOGY, envelope, soap_action_header};
use fsonos_proto::ssdp::description_url;
use fsonos_sim::{SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};

const ORIGINAL: [&str; 2] = [
    "x-file-cifs://example.invalid/first.flac",
    "x-file-cifs://example.invalid/second.flac",
];

/// The player really moved; only its topology Location lags behind. Its own
/// description and all queue operations still travel over real sockets.
struct LaggingLocation {
    lan: SimLan,
    host: IpAddr,
    topology: String,
    descriptions: Arc<AtomicUsize>,
    available: Arc<AtomicBool>,
}

impl Transport for LaggingLocation {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, action, body)?;
        if host == self.host && action.ends_with("#GetZoneGroupState\"") {
            return Ok(self.topology.clone());
        }
        Ok(reply)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        assert_eq!(url, description_url(self.host));
        self.descriptions.fetch_add(1, Ordering::SeqCst);
        if !self.available.load(Ordering::SeqCst) {
            return Err(ProtoError::Network {
                target: url.into(),
                detail: "injected unavailable endpoint description".into(),
            });
        }
        self.lan.http_get(url)
    }
}

struct Fixture {
    sim: SimHandle,
    surface: Surface,
    coordinator: PlayerId,
    host: IpAddr,
    peer: IpAddr,
    descriptions: Arc<AtomicUsize>,
    available: Arc<AtomicBool>,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new(ROOM, model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let mut sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let old_host = sim.player(ROOM).unwrap().ip;
        let peer = sim.player("Peer").unwrap().ip;
        let coordinator = PlayerId(sim.player(ROOM).unwrap().uuid.clone());
        let lan = sim.lan();
        for host in [old_host, peer] {
            for uri in ORIGINAL {
                soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
            }
        }
        soap::play_from_queue(&lan, old_host, &coordinator).unwrap();
        soap::seek_track(&lan, old_host, 2).unwrap();
        soap::seek_position(&lan, old_host, 17).unwrap();
        soap::play(&lan, old_host).unwrap();
        soap::pause(&lan, old_host).unwrap();
        let topology = lan
            .soap_post(
                old_host,
                ZONE_GROUP_TOPOLOGY.control_path,
                &soap_action_header(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState"),
                &envelope(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", ""),
            )
            .unwrap();
        let mut houses = house(&sim);
        let host = sim.change_address(ROOM).unwrap();
        for house in &mut houses {
            if let Some(player) = house.players.iter_mut().find(|p| p.id == coordinator) {
                player.ip = host;
            }
        }
        let descriptions = Arc::new(AtomicUsize::new(0));
        let available = Arc::new(AtomicBool::new(true));
        let surface = Surface::new(
            Box::new(LaggingLocation {
                lan: sim.lan(),
                host,
                topology,
                descriptions: Arc::clone(&descriptions),
                available: Arc::clone(&available),
            }),
            Box::new(move |_| Ok(houses.clone())),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "http");
        Self {
            sim,
            surface,
            coordinator,
            host,
            peer,
            descriptions,
            available,
        }
    }

    fn queue(&self, host: IpAddr) -> Vec<String> {
        browse_snapshot(&self.sim.lan(), host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn writes_since(&self, marker: usize) -> Vec<(String, String)> {
        self.sim.soap_log()[marker..]
            .iter()
            .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
            .map(|call| (call.room.clone(), call.action.clone()))
            .collect()
    }
}

#[test]
fn reviewed_queue_undos_keep_repaired_identity_and_rebase_the_earlier_add() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        for (uri, title) in [
            ("spotify:track:0000000000000000000001", "First addition"),
            ("spotify:track:0000000000000000000002", "Second addition"),
        ] {
            let added = f
                .surface
                .queue_add(&Client::Cli, &add(uri, title, false))
                .unwrap();
            assert_eq!(added.count, 1);
        }
        assert_eq!(f.queue(f.host).len(), 4);
        let marker = f.sim.soap_log().len();
        let before = f.descriptions.load(Ordering::SeqCst);
        for remaining in [3, 2] {
            let preview = f.surface.preview_undo(&Client::Cli).unwrap().unwrap();
            assert!(preview.undoable);
            let undone = f.surface.undo_checked(&Client::Cli, preview.id).unwrap();
            assert_eq!(undone.undone, Some(preview.id));
            assert!(undone.failures.is_empty(), "{undone:?}");
            assert_eq!(f.queue(f.host).len(), remaining);
        }
        assert!(f.descriptions.load(Ordering::SeqCst) > before);
        assert_eq!(f.queue(f.host), ORIGINAL);
        assert_eq!(f.queue(f.peer), ORIGINAL);
        assert_eq!(
            f.writes_since(marker),
            vec![(ROOM.into(), "RemoveTrackRangeFromQueue".into()); 2]
        );
        let position = soap::get_position_info(&f.sim.lan(), f.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.host)
                .unwrap()
                .state,
            TransportState::Paused
        );
        assert_eq!(
            soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri,
            format!("x-rincon-queue:{}#0", f.coordinator.0)
        );
    }
}

#[test]
fn reviewed_queue_undo_still_requires_a_fresh_endpoint_description() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.surface
            .queue_add(
                &Client::Cli,
                &add("spotify:track:0000000000000000000001", "Addition", false),
            )
            .unwrap();
        let queued = f.queue(f.host);
        let preview = f.surface.preview_undo(&Client::Cli).unwrap().unwrap();
        f.available.store(false, Ordering::SeqCst);
        let marker = f.sim.soap_log().len();
        assert!(f.surface.undo_checked(&Client::Cli, preview.id).is_err());
        assert!(f.writes_since(marker).is_empty());
        assert_eq!(f.queue(f.host), queued);
        assert_eq!(f.queue(f.peer), ORIGINAL);
    }
}
