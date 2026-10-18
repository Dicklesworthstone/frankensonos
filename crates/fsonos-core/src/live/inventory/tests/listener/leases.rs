//! Exercise the production pump while a real lease operation is outstanding.

use super::*;
use crate::events::{Subscriber, Want};
use crate::reconcile::Health;
use fsonos_proto::gena::Subscription;

#[derive(Clone, Copy)]
enum Reply {
    Renewed,
    Replaced,
    Failed,
}

struct HeldLease {
    lan: Arc<Lan>,
    player: PlayerId,
    gate: Arc<Gate>,
    reply: Reply,
}

impl HeldLease {
    fn hold(&self) {
        self.gate.entered.store(true, Ordering::Release);
        let deadline = Instant::now() + WAIT;
        while !self.gate.released.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "held lease timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Subscriber for HeldLease {
    fn subscribe(&self, want: &Want, callback: &str) -> Result<Subscription, ProtoError> {
        if want.player == self.player && matches!(self.reply, Reply::Failed) {
            return Err(ProtoError::NotWired("replacement refused"));
        }
        let result = Subscriber::subscribe(&*self.lan, want, callback)?;
        if want.player == self.player && matches!(self.reply, Reply::Replaced) {
            self.hold();
        }
        Ok(result)
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        if want.player == self.player {
            match self.reply {
                Reply::Replaced => return Err(ProtoError::NotWired("lease lost")),
                Reply::Failed => {
                    self.hold();
                    return Err(ProtoError::NotWired("renewal refused"));
                }
                Reply::Renewed => self.hold(),
            }
        }
        Subscriber::renew(&*self.lan, want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        Subscriber::unsubscribe(&*self.lan, want, sid)
    }
}

#[test]
fn event_pump_accepts_notifications_while_lease_io_is_held_on_both_generations() {
    for generation in [1, 2] {
        for reply in [Reply::Renewed, Reply::Replaced, Reply::Failed] {
            let mut f = Fixture::new(generation, Stage::Discovery, true);
            let a = f.sim.player("A").unwrap();
            let a_id = PlayerId(a.uuid.clone());
            let a_player_ip = a.ip;
            let b = f.sim.player("B").unwrap();
            let b_id = PlayerId(b.uuid.clone());
            let b_player_ip = b.ip;
            // Keep the existing, due rendering leases; no private sequence or
            // deadline mutation and no fake event listener are needed here.
            let wants: Vec<_> = wanted(&f.model.rec.households)
                .into_iter()
                .filter(|w| w.service == Service::RenderingControl)
                .collect();
            let sink = f.model.sink.as_ref().unwrap();
            f.model.rec.subscriptions.sync(
                &*f.engine.lan,
                &wants,
                |s| sink.callback_url(s.tag()),
                Instant::now(),
            );
            let gate = Arc::new(Gate::default());
            let release = Release(Arc::clone(&gate));
            let held = Arc::new(HeldLease {
                lan: Arc::clone(&f.engine.lan),
                player: a_id.clone(),
                gate,
                reply,
            });
            let provider: Arc<dyn Subscriber + Send + Sync> = held.clone();
            f.model.rec.subscriptions.start_renewals(
                &provider,
                |s| sink.callback_url(s.tag()),
                &f.engine.stop,
                Instant::now(),
            );
            drop(provider);
            wait_for(|| held.gate.entered.load(Ordering::Acquire));
            set_volume(&f.sim.lan(), a_player_ip, 67).unwrap();
            set_volume(&f.sim.lan(), b_player_ip, 59).unwrap();
            let before = f.sim.soap_log().len();
            wait_for(|| {
                pump::turn(&f.engine, &mut f.model);
                f.model
                    .playback
                    .of(&a_id)
                    .is_some_and(|s| s.volume == Some(67))
                    && f.model
                        .playback
                        .of(&b_id)
                        .is_some_and(|s| s.volume == Some(59))
            });
            assert!(!held.gate.released.load(Ordering::Acquire));
            assert_eq!(
                f.model.rec.health.of(&a_id).unwrap().health,
                Health::Healthy
            );
            drop(release);
            wait_for(|| {
                pump::turn(&f.engine, &mut f.model);
                Arc::strong_count(&held) == 1
            });
            // Finish draining early replacement initial/delta events. This
            // same path must not apply a stale failure over newer live health.
            while let Some(events) = f.engine.next_event(&mut f.model, Duration::ZERO) {
                f.engine.publish(&f.model, events);
            }
            assert_eq!(f.model.playback.of(&a_id).unwrap().volume, Some(67));
            assert_eq!(f.model.playback.of(&b_id).unwrap().volume, Some(59));
            assert_eq!(
                f.model.rec.health.of(&a_id).unwrap().health,
                Health::Healthy
            );
            assert!(
                f.sim.soap_log()[before..]
                    .iter()
                    .all(|call| call.action.starts_with("Get"))
            );
            f.finish(Surveys::default());
        }
    }
}
