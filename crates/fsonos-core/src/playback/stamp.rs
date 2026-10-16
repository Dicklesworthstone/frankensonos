//! Service-local observation identities. Equal values are not proof that no
//! event arrived: volume can change away and back while a read is pending.

use super::{EventSource, Playback};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::sync::Arc;

/// Clones identify the same observation; a new allocation never reuses the
/// identity while any pending read still holds it. No wrapping counters.
#[derive(Debug, Clone, Default)]
pub(crate) struct Stamp(Arc<()>);

impl PartialEq for Stamp {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Stamp {}

#[derive(Debug, Clone, Default)]
pub(super) struct Observations(HashMap<PlayerId, [Option<Stamp>; 3]>);

fn index(source: EventSource) -> usize {
    match source {
        EventSource::AvTransport => 0,
        EventSource::RenderingControl => 1,
        EventSource::GroupRenderingControl => 2,
    }
}

impl Observations {
    pub(super) fn mark(&mut self, player: &PlayerId, source: EventSource) {
        self.0.entry(player.clone()).or_default()[index(source)] = Some(Stamp::default());
    }

    pub(super) fn forget(&mut self, player: &PlayerId) {
        self.0.remove(player);
    }
}

impl Playback {
    pub(crate) fn observation_stamp(&self, player: &PlayerId, source: EventSource) -> Option<Stamp> {
        self.observations.0.get(player)?[index(source)].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::gena::Notify;
    use std::time::Instant;

    fn notify(body: &str) -> Notify {
        Notify {
            sid: "uuid:stamp-test".into(),
            seq: 1,
            path: "/RenderingControl".into(),
            properties: vec![(
                "LastChange".into(),
                format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
            )],
        }
    }

    #[test]
    fn equal_values_and_a_return_to_the_old_value_are_new_observations() {
        let id = PlayerId("test-player".into());
        let mut playback = Playback::default();
        let source = EventSource::RenderingControl;
        for volume in [10, 20, 10, 10] {
            let before = playback.observation_stamp(&id, source);
            let body = format!("<Volume channel=\"Master\" val=\"{volume}\"/>");
            playback.apply(&id, source, &notify(&body), Instant::now()).unwrap();
            let after = playback.observation_stamp(&id, source);
            assert_ne!(after, before);
            assert_eq!(after, playback.clone().observation_stamp(&id, source));
            assert_eq!(playback.of(&id).unwrap().volume, Some(volume));
            assert!(playback.observation_stamp(&id, EventSource::AvTransport).is_none());
        }
    }

    #[test]
    fn rejected_events_do_not_invalidate_usable_observations() {
        let id = PlayerId("test-player".into());
        let mut playback = Playback::default();
        let source = EventSource::AvTransport;
        playback.apply(&id, source, &notify(""), Instant::now()).unwrap();
        let before = playback.observation_stamp(&id, source);
        let bad = notify("<CurrentTrackMetaData val=\"not-xml\"/>");
        assert!(playback.apply(&id, source, &bad, Instant::now()).is_err());
        assert_eq!(playback.observation_stamp(&id, source), before);
    }

    #[test]
    fn removing_and_reintroducing_a_player_cannot_reuse_an_old_stamp() {
        let id = PlayerId("test-player".into());
        let mut playback = Playback::default();
        let source = EventSource::RenderingControl;
        playback.apply_room_levels(&id, 10, false);
        let before = playback.observation_stamp(&id, source);
        assert!(before.is_some());
        playback.remove(&id);
        assert!(playback.observation_stamp(&id, source).is_none());
        playback.apply_room_levels(&id, 10, false);
        assert_ne!(playback.observation_stamp(&id, source), before);
        assert_eq!(playback.observations.0.len(), 1);
    }
}
