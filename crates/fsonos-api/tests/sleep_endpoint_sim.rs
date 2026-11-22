//! A cached group must not change the native timer of an unrelated endpoint.
//! Exercise both native-only control and a restarted daemon before adoption.

use fsonos_api::surface::schedules::{SLEEP_FILE, Sleep, SleepRequest};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn requests() -> [SleepRequest; 3] {
    [
        SleepRequest {
            expected_revision: None,
            zone: "SleepA".into(),
            duration: Some("20m".into()),
            extend: false,
            cancel: false,
        },
        SleepRequest {
            expected_revision: None,
            zone: "SleepA".into(),
            duration: Some("5m".into()),
            extend: true,
            cancel: false,
        },
        SleepRequest {
            expected_revision: None,
            zone: "SleepA".into(),
            duration: None,
            extend: false,
            cancel: true,
        },
    ]
}

struct House {
    sim: SimHandle,
    cached: Vec<HouseholdState>,
    saved: Option<PathBuf>,
    _scratch: tempfile::TempDir,
}

impl House {
    fn new(s2: bool, daemon: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let specs = ["SleepA", "SleepB"].map(|name| SimPlayerSpec::new(name, model));
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(specs)
        } else {
            builder.s1(specs)
        }
        .spawn()
        .unwrap();
        let mut household = HouseholdState::default();
        household.apply_topology(
            &get_zone_group_state(&sim.lan(), sim.player("SleepA").unwrap().ip).unwrap(),
        );
        let scratch = tempfile::tempdir().unwrap();
        let h = Self {
            sim,
            cached: vec![household],
            saved: daemon.then(|| scratch.path().join(SLEEP_FILE)),
            _scratch: scratch,
        };
        if daemon {
            // Seed a saved candidate, then drop the daemon that armed it.
            // The tested replacement has no armed timer and must not discard
            // this pending recovery state when its endpoint check refuses.
            let first = h.surface(Box::new(h.sim.lan()));
            let timer = first.set_sleep_timer(&Client::Cli, &requests()[0]).unwrap();
            assert!(timer.fades);
        } else {
            soap::configure_sleep_timer(&h.sim.lan(), h.ip("SleepA"), Some(1_200)).unwrap();
        }
        soap::configure_sleep_timer(&h.sim.lan(), h.ip("SleepB"), Some(3_600)).unwrap();
        h
    }

    fn ip(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn surface(&self, transport: Box<dyn Transport + Send + Sync>) -> Surface {
        let cached = self.cached.clone();
        let survey: Survey = Box::new(move |_| Ok(cached.clone()));
        let surface = Surface::new(transport, survey, Policy::default(), Box::new(SystemClock));
        match &self.saved {
            Some(path) => surface.with_sleep(Sleep::default().saved_in(path.clone())),
            None => surface,
        }
    }

    fn saved_bytes(&self) -> Option<Vec<u8>> {
        self.saved.as_ref().map(|path| std::fs::read(path).unwrap())
    }

    fn generations(&self) -> [Option<u32>; 2] {
        ["SleepA", "SleepB"].map(|room| {
            let timer = soap::get_sleep_timer_info(&self.sim.lan(), self.ip(room)).unwrap();
            assert!(timer.remaining_secs.is_some_and(|remaining| remaining > 0));
            assert!(timer.generation.is_some());
            timer.generation
        })
    }
}

#[test]
fn sleep_changes_refuse_independent_and_reassigned_endpoints_without_retiring_saved_timers() {
    for s2 in [false, true] {
        for daemon in [false, true] {
            for reassigned in [false, true] {
                let mut h = House::new(s2, daemon);
                if reassigned {
                    let IpAddr::V4(old) = h.ip("SleepA") else {
                        panic!("the simulator uses IPv4");
                    };
                    h.sim.change_address("SleepA").unwrap();
                    h.sim
                        .move_to("SleepB", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                        .unwrap();
                } else {
                    // A household-wide topology from B still lists A, but B
                    // independently leads another group at this physical IP.
                    let wrong = h.ip("SleepB");
                    h.cached[0]
                        .players
                        .iter_mut()
                        .find(|player| player.room_name == "SleepA")
                        .unwrap()
                        .ip = wrong;
                }
                let surface = h.surface(Box::new(h.sim.lan()));
                let saved = h.saved_bytes();
                let generations = h.generations();
                for request in requests() {
                    let before = h.sim.soap_log().len();
                    let failure = surface.set_sleep_timer(&Client::Cli, &request).unwrap_err();
                    assert_eq!(failure.code, ErrorCode::NotCoordinator, "{request:?}");
                    assert_eq!(failure.upnp_code, None);
                    assert!(failure.hint.contains("intended room"));
                    let log = h.sim.soap_log();
                    let calls = &log[before..];
                    assert_eq!(calls.len(), 1, "{request:?}: {calls:?}");
                    assert_eq!(calls[0].action, "GetZoneGroupState");
                    assert_eq!(calls[0].room, "SleepB");
                    assert_eq!(h.generations(), generations);
                    assert_eq!(
                        h.saved_bytes(),
                        saved,
                        "recovery state must survive refusal"
                    );
                }
            }
        }
    }
}

struct BrokenTopology {
    inner: SimLan,
    malformed: bool,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Transport for BrokenTopology {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.calls.lock().unwrap().push(header.to_owned());
        if header.contains("#GetZoneGroupState") {
            if self.malformed {
                return Ok("<not-a-soap-reply>".into());
            }
            return Err(ProtoError::Network {
                target: "synthetic-sleep-player".into(),
                detail: "topology is unavailable".into(),
            });
        }
        self.inner.soap_post(host, path, header, body)
    }
}

#[test]
fn unreadable_sleep_endpoint_proofs_keep_the_protocol_error_and_never_mutate_or_replay() {
    for s2 in [false, true] {
        for daemon in [false, true] {
            for malformed in [false, true] {
                let h = House::new(s2, daemon);
                let calls = Arc::new(Mutex::new(Vec::new()));
                let transport = BrokenTopology {
                    inner: h.sim.lan(),
                    malformed,
                    calls: Arc::clone(&calls),
                };
                let expected = get_zone_group_state(&transport, h.ip("SleepA"))
                    .map_err(CoreError::from)
                    .map_err(Failure::from)
                    .unwrap_err();
                assert_ne!(expected.code, ErrorCode::NotCoordinator);
                let surface = h.surface(Box::new(transport));
                let saved = h.saved_bytes();
                let generations = h.generations();
                for request in requests() {
                    calls.lock().unwrap().clear();
                    let before = h.sim.soap_log().len();
                    let failure = surface.set_sleep_timer(&Client::Cli, &request).unwrap_err();
                    assert_eq!(failure, expected, "{request:?}");
                    let attempted = calls.lock().unwrap();
                    assert_eq!(attempted.len(), 1);
                    assert!(attempted[0].contains("#GetZoneGroupState"));
                    assert_eq!(h.sim.soap_log().len(), before, "no timer I/O or write");
                    assert_eq!(h.generations(), generations);
                    assert_eq!(h.saved_bytes(), saved);
                }
            }
        }
    }
}
