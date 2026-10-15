//! A bounded live subscriber must learn that it missed deltas, even when
//! the last event of a burst was lost and the house has gone quiet.

use super::LiveEvent;
use std::sync::mpsc::{SyncSender, TrySendError};

/// One bit of recovery state per bounded channel, never a second backlog.
pub(super) struct Pending {
    sender: SyncSender<LiveEvent>,
    lagged: bool,
}

impl Pending {
    pub(super) fn new(sender: SyncSender<LiveEvent>) -> Self {
        Self {
            sender,
            lagged: false,
        }
    }

    /// Return false only when the receiver is gone. The producer has already
    /// published its snapshot before calling this. A reset covers this whole
    /// batch, so do not send its stale deltas after that reset. Empty batches
    /// retry only the marker, without requiring another speaker event.
    pub(super) fn deliver(&mut self, events: &[LiveEvent]) -> bool {
        if self.lagged {
            return match self.sender.try_send(LiveEvent::Resync) {
                Ok(()) => {
                    self.lagged = false;
                    true
                }
                Err(TrySendError::Full(_)) => true,
                Err(TrySendError::Disconnected(_)) => false,
            };
        }
        for event in events {
            match self.sender.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.lagged = true;
                    return true;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::Changes;
    use fsonos_types::PlayerId;
    use std::sync::mpsc::{TryRecvError, sync_channel};

    fn event(level: u8) -> LiveEvent {
        LiveEvent::Playback {
            player: PlayerId("test-player".into()),
            changes: Changes {
                volume: Some(level),
                ..Changes::default()
            },
        }
    }

    #[test]
    fn healthy_delivery_preserves_every_event_without_resets() {
        let (send, receive) = sync_channel(3);
        let mut subscriber = Pending::new(send);
        assert!(subscriber.deliver(&[event(1), event(2), event(3)]));
        assert_eq!(
            receive.try_iter().collect::<Vec<_>>(),
            [event(1), event(2), event(3)]
        );
        for _ in 0..3 {
            assert!(subscriber.deliver(&[]));
        }
        assert_eq!(receive.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn loss_at_the_end_of_a_burst_recovers_without_a_new_event() {
        let (send, receive) = sync_channel(1);
        let mut subscriber = Pending::new(send);
        assert!(subscriber.deliver(&[event(1), event(2)]));
        assert!(subscriber.lagged);
        assert_eq!(receive.try_recv(), Ok(event(1)));
        assert!(subscriber.deliver(&[]));
        assert_eq!(receive.try_recv(), Ok(LiveEvent::Resync));
        assert!(!subscriber.lagged);
        assert!(subscriber.deliver(&[]));
        assert_eq!(receive.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn a_slow_reader_gets_one_marker_after_its_already_queued_prefix() {
        let (send, receive) = sync_channel(2);
        let mut subscriber = Pending::new(send);
        assert!(subscriber.deliver(&[event(1), event(2), event(3)]));
        for _ in 0..10_000 {
            assert!(subscriber.deliver(&[event(4), event(5)]));
        }
        assert_eq!(receive.try_recv(), Ok(event(1)));
        // The snapshot already includes these changes. Resync, not an old
        // value from this batch, must be delivered after the retained prefix.
        assert!(subscriber.deliver(&[event(6), event(7)]));
        assert_eq!(receive.try_recv(), Ok(event(2)));
        assert_eq!(receive.try_recv(), Ok(LiveEvent::Resync));
        assert_eq!(receive.try_recv(), Err(TryRecvError::Empty));
        assert!(subscriber.deliver(&[event(8)]));
        assert_eq!(receive.try_recv(), Ok(event(8)));
    }

    #[test]
    fn a_second_overflow_is_not_hidden_by_the_first_reset() {
        let (send, receive) = sync_channel(1);
        let mut subscriber = Pending::new(send);
        for _ in 0..3 {
            assert!(subscriber.deliver(&[event(1), event(2)]));
            assert_eq!(receive.try_recv(), Ok(event(1)));
            assert!(subscriber.deliver(&[]));
            assert_eq!(receive.try_recv(), Ok(LiveEvent::Resync));
        }
    }

    #[test]
    fn a_full_subscriber_cannot_block_healthy_subscribers() {
        let (slow_send, slow) = sync_channel(1);
        let (fast_send, fast) = sync_channel(1);
        let mut subscribers = [Pending::new(slow_send), Pending::new(fast_send)];
        for level in 0..100 {
            for subscriber in &mut subscribers {
                assert!(subscriber.deliver(&[event(level)]));
            }
            assert_eq!(fast.try_recv(), Ok(event(level)));
        }
        assert_eq!(slow.try_recv(), Ok(event(0)));
        for subscriber in &mut subscribers {
            assert!(subscriber.deliver(&[]));
        }
        assert_eq!(slow.try_recv(), Ok(LiveEvent::Resync));
        assert_eq!(fast.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn disconnected_receivers_are_removed_before_or_after_overflow() {
        for overflow in [false, true] {
            let (send, receive) = sync_channel(1);
            let mut subscriber = Pending::new(send);
            if overflow {
                assert!(subscriber.deliver(&[event(1), event(2)]));
            }
            drop(receive);
            assert!(!subscriber.deliver(&[event(3)]));
        }
    }
}
