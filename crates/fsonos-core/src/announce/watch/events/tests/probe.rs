//! The ownership read itself may be in flight while accepted callbacks queue.

use super::*;
use crate::HouseholdState;
use crate::announce::{Clip, plan_group};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

struct Probed {
    lan: SimLan,
    player: PlayerId,
    events: Sender<LiveEvent>,
    first: AtomicBool,
    reads: AtomicUsize,
}

impl Transport for Probed {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.trim_matches('"').ends_with("#GetMediaInfo")
            && self.first.swap(false, Ordering::AcqRel)
        {
            // The response still names the clip, but the accepted event
            // prefix recorded a source away and back while it was read.
            for uri in ["test:newer", CLIP] {
                self.events
                    .send(LiveEvent::Playback {
                        player: self.player.clone(),
                        changes: Changes {
                            track: Some((None, Some(uri.into()))),
                            transport: Some((None, TransportState::Playing)),
                            ..Changes::default()
                        },
                    })
                    .unwrap();
            }
        }
        Ok(response)
    }
}

#[test]
fn recovery_read_keeps_intervening_source_events_terminal() {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Alpha", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let host = sim.player("Alpha").unwrap().ip;
    let clip = Clip {
        url: CLIP.into(),
        title: "Held ownership observation".into(),
        duration: Duration::from_secs(3),
    };
    soap::set_av_transport_uri(&lan, host, CLIP, &clip.didl()).unwrap();
    soap::play(&lan, host).unwrap();
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let plan = plan_group(&household, &[&household.rooms[0]]).unwrap();
    let (send, receive) = mpsc::channel();
    send.send(LiveEvent::Resync).unwrap();
    let transport = Probed {
        lan,
        player: plan.lead.clone(),
        events: send,
        first: AtomicBool::new(true),
        reads: AtomicUsize::new(0),
    };
    let mut watch = Watch {
        transport: &transport,
        household: &household,
        plan: &plan,
        clip: &clip,
        host,
        started: Instant::now(),
        deadline: Duration::from_millis(500),
        began: false,
        observed: None,
    };
    assert!(matches!(
        wait(&mut watch, &receive),
        Err(Interrupted::LeaveAlone(why)) if why.contains("superseded")
    ));
    assert_eq!(transport.reads.load(Ordering::Relaxed), 2);
    assert!(
        receive.try_recv().is_ok(),
        "never discard the queued return to our clip"
    );
    assert_eq!(
        soap::get_transport_info(&transport.lan, host)
            .unwrap()
            .state,
        TransportState::Playing
    );
}
