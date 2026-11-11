//! Deduplicate content invalidations within one live ContentDirectory lease.
//! Counters are equality tokens, not ordered clocks and never queue revisions
//! that callers may use to authorize a write.

use super::{Service, Subscriptions};
use fsonos_proto::content::{ContentUpdate, MAX_UPDATE_CONTAINERS};
use fsonos_proto::gena::Notify;
use fsonos_types::PlayerId;
use std::collections::{BTreeMap, HashMap};

/// The explicit reads a ContentDirectory event invalidated. Queue changes
/// belong to its coordinator; favorites and playlists belong to its household.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ContentChanges {
    pub queue: bool,
    pub favorites: bool,
    pub playlists: bool,
}

impl ContentChanges {
    pub const ALL: Self = Self {
        queue: true,
        favorites: true,
        playlists: true,
    };

    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.queue && !self.favorites && !self.playlists
    }

    fn container(&mut self, name: &str) {
        match name {
            "Q:" | "Q:0" => self.queue = true,
            "FV:2" => self.favorites = true,
            _ if name.starts_with("FV:2/") => self.favorites = true,
            _ if name.starts_with("SQ:") => self.playlists = true,
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
struct Observed {
    sid: String,
    system: Option<u32>,
    favorites: Option<String>,
    playlists: Option<String>,
    containers: BTreeMap<String, u32>,
}

#[derive(Debug, Default)]
pub(super) struct Tracker(HashMap<PlayerId, Observed>);

impl Observed {
    fn apply(&mut self, update: &ContentUpdate) -> ContentChanges {
        let mut changes = ContentChanges::default();
        if let Some(counter) = &update.favorites_update_id {
            changes.favorites = self.favorites.as_ref() != Some(counter);
            self.favorites = Some(counter.clone());
        }
        if let Some(counter) = &update.saved_queues_update_id {
            changes.playlists = self.playlists.as_ref() != Some(counter);
            self.playlists = Some(counter.clone());
        }
        for (name, counter) in &update.containers {
            let mut relevant = ContentChanges::default();
            relevant.container(name);
            if relevant.is_empty() {
                continue;
            }
            if self.containers.get(name) != Some(counter) {
                changes.queue |= relevant.queue;
                changes.favorites |= relevant.favorites;
                changes.playlists |= relevant.playlists;
            }
            // A long-lived speaker can name arbitrarily many saved playlists.
            // Eviction causes only a conservative future invalidation.
            if !self.containers.contains_key(name)
                && self.containers.len() == MAX_UPDATE_CONTAINERS
            {
                self.containers.pop_first();
            }
            self.containers.insert(name.clone(), *counter);
        }
        if let Some(counter) = update.system_update_id {
            if self.system != Some(counter) && update.containers.is_empty() && changes.is_empty() {
                // A system-only notice cannot identify which displayed
                // collection changed. Ask consumers to reread lazily.
                changes = ContentChanges::ALL;
            }
            self.system = Some(counter);
        }
        changes
    }
}

impl Subscriptions {
    pub(crate) fn content_changes(
        &mut self,
        notify: &Notify,
        update: &ContentUpdate,
        missed: bool,
    ) -> ContentChanges {
        self.content.0.retain(|player, observed| {
            self.active.iter().any(|active| {
                active.want.service == Service::ContentDirectory
                    && active.want.player == *player
                    && active.sid == observed.sid
            })
        });
        let Some(player) = self
            .active
            .iter()
            .find(|active| {
                active.sid == notify.sid && active.want.service == Service::ContentDirectory
            })
            .map(|active| active.want.player.clone())
        else {
            return ContentChanges::default();
        };
        let observed = self.content.0.entry(player).or_default();
        let fresh = observed.sid != notify.sid;
        if fresh || missed || notify.seq == 0 {
            *observed = Observed {
                sid: notify.sid.clone(),
                ..Observed::default()
            };
        }
        let changes = observed.apply(update);
        if fresh || missed || notify.seq == 0 {
            // A gap may hide a different container; a new lease's initial
            // counters cannot prove that any preexisting view is still fresh.
            ContentChanges::ALL
        } else {
            changes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{RETRY, Subscriber, Want};
    use crate::playback::Playback;
    use crate::reconcile::Reconciler;
    use fsonos_proto::ProtoError;
    use fsonos_proto::gena::Subscription;
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[test]
    fn repeated_counters_do_not_invalidate_unrelated_collections() {
        let mut state = Observed::default();
        let mut update = ContentUpdate {
            system_update_id: Some(10),
            containers: BTreeMap::from([("Q:0".into(), 1)]),
            favorites_update_id: Some("fav-a".into()),
            saved_queues_update_id: Some("saved-a".into()),
            has_container_updates: true,
        };
        assert_eq!(state.apply(&update), ContentChanges::ALL);
        assert!(state.apply(&update).is_empty());
        update.containers.insert("Q:0".into(), 2);
        update.system_update_id = Some(11);
        assert_eq!(
            state.apply(&update),
            ContentChanges {
                queue: true,
                ..ContentChanges::default()
            }
        );
        update.saved_queues_update_id = Some("saved-b".into());
        assert_eq!(
            state.apply(&update),
            ContentChanges {
                playlists: true,
                ..ContentChanges::default()
            }
        );
        update.favorites_update_id = Some("fav-b".into());
        assert_eq!(
            state.apply(&update),
            ContentChanges {
                favorites: true,
                ..ContentChanges::default()
            }
        );
    }

    #[test]
    fn system_only_changes_invalidate_all_but_known_unrelated_containers_do_not() {
        let mut state = Observed::default();
        let mut update = ContentUpdate {
            system_update_id: Some(5),
            ..ContentUpdate::default()
        };
        assert_eq!(state.apply(&update), ContentChanges::ALL);
        assert!(state.apply(&update).is_empty());
        update.system_update_id = Some(0); // Counter wrap is still a change.
        assert_eq!(state.apply(&update), ContentChanges::ALL);
        update.system_update_id = Some(1);
        update.containers.insert("A:ARTIST".into(), 99);
        assert!(state.apply(&update).is_empty());
        assert!(state.containers.is_empty());
    }

    #[test]
    fn unchanged_included_catalog_tokens_do_not_hide_a_system_invalidation() {
        let mut state = Observed::default();
        let mut update = ContentUpdate {
            system_update_id: Some(1),
            favorites_update_id: Some("favorite-a".into()),
            saved_queues_update_id: Some("saved-a".into()),
            has_container_updates: true,
            ..ContentUpdate::default()
        };
        state.apply(&update);
        assert!(state.apply(&update).is_empty());
        update.system_update_id = Some(2);
        assert_eq!(state.apply(&update), ContentChanges::ALL);
        // A missing property and an explicitly empty list are both unable to
        // attribute a changed system counter to one known collection.
        update.has_container_updates = false;
        update.system_update_id = Some(3);
        assert_eq!(state.apply(&update), ContentChanges::ALL);
        update.system_update_id = Some(4);
        update.favorites_update_id = Some("favorite-b".into());
        assert_eq!(
            state.apply(&update),
            ContentChanges {
                favorites: true,
                ..ContentChanges::default()
            }
        );
    }

    #[test]
    fn saved_playlist_and_favorite_children_invalidate_their_catalog_only() {
        for (name, expected) in [
            (
                "SQ:7",
                ContentChanges {
                    playlists: true,
                    ..ContentChanges::default()
                },
            ),
            (
                "FV:2/4",
                ContentChanges {
                    favorites: true,
                    ..ContentChanges::default()
                },
            ),
            ("Q:1", ContentChanges::default()),
            ("FV:20", ContentChanges::default()),
        ] {
            let update = ContentUpdate {
                containers: BTreeMap::from([(name.into(), 1)]),
                ..ContentUpdate::default()
            };
            assert_eq!(Observed::default().apply(&update), expected);
        }
    }

    #[test]
    fn retained_container_observations_remain_bounded() {
        let mut state = Observed::default();
        for id in 0..(MAX_UPDATE_CONTAINERS * 3) {
            let update = ContentUpdate {
                containers: BTreeMap::from([(format!("SQ:{id}"), 1)]),
                ..ContentUpdate::default()
            };
            assert!(state.apply(&update).playlists);
            assert!(state.containers.len() <= MAX_UPDATE_CONTAINERS);
        }
    }

    #[derive(Default)]
    struct ContentProvider {
        calls: Cell<u32>,
        refuse: Cell<bool>,
    }

    impl Subscriber for ContentProvider {
        fn subscribe(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            self.calls.set(self.calls.get() + 1);
            if self.refuse.get() {
                return Err(ProtoError::Malformed("test subscribe failure".into()));
            }
            Ok(Subscription {
                sid: format!("uuid:content-{}", self.calls.get()),
                timeout_secs: 300,
            })
        }

        fn renew(&self, _: &Want, _: &str) -> Result<Subscription, ProtoError> {
            panic!("malformed stream needs replacement, not renewal");
        }

        fn unsubscribe(&self, _: &Want, _: &str) -> Result<(), ProtoError> {
            Ok(())
        }
    }

    fn content_callback(_: Service) -> String {
        "http://127.0.0.1:3400/ContentDirectory".into()
    }

    fn content_event(sid: &str, seq: u32, pairs: &str) -> Notify {
        Notify {
            sid: sid.into(),
            seq,
            path: "/ContentDirectory".into(),
            properties: vec![("ContainerUpdateIDs".into(), pairs.into())],
        }
    }

    #[test]
    fn failed_refresh_preserves_counter_proof_and_corrected_sequence_until_retry() {
        let callback = content_callback;
        let event = content_event;
        let now = Instant::now();
        let provider = ContentProvider::default();
        let mut rec = Reconciler::new(RETRY, RETRY, now);
        rec.subscriptions.sync(
            &provider,
            &[Want {
                player: PlayerId("content-player".into()),
                host: std::net::Ipv4Addr::LOCALHOST.into(),
                service: Service::ContentDirectory,
            }],
            callback,
            now,
        );
        let mut playback = Playback::default();
        let initial = event("uuid:content-1", 0, "Q:0,1");
        assert_eq!(
            rec.on_notify(&provider, &mut playback, &initial, callback, now)
                .unwrap()
                .unwrap()
                .content,
            ContentChanges::ALL
        );
        provider.refuse.set(true);
        let malformed = event("uuid:content-1", 1, "Q:0,broken");
        assert!(
            rec.on_notify(&provider, &mut playback, &malformed, callback, now)
                .is_err()
        );
        assert_eq!(provider.calls.get(), 2);
        assert_eq!(rec.subscriptions.next_due(), Some(now + RETRY));
        let corrected = event("uuid:content-1", 1, "Q:0,2");
        assert_eq!(
            rec.on_notify(&provider, &mut playback, &corrected, callback, now)
                .unwrap()
                .unwrap()
                .content,
            ContentChanges {
                queue: true,
                ..ContentChanges::default()
            }
        );
        assert!(
            rec.on_notify(&provider, &mut playback, &corrected, callback, now)
                .unwrap()
                .is_none()
        );
        let repeated = event("uuid:content-1", 2, "Q:0,2");
        assert!(
            rec.on_notify(&provider, &mut playback, &repeated, callback, now)
                .unwrap()
                .unwrap()
                .content
                .is_empty()
        );
        let bad_again = event("uuid:content-1", 3, "Q:0,bad");
        assert!(
            rec.on_notify(
                &provider,
                &mut playback,
                &bad_again,
                callback,
                now + Duration::from_secs(1),
            )
            .is_err()
        );
        assert_eq!(
            provider.calls.get(),
            2,
            "malformed events cannot bypass retry backoff"
        );
        provider.refuse.set(false);
        assert_eq!(
            rec.subscriptions
                .renew_due(&provider, callback, now + RETRY)
                .resubscribed,
            1
        );
        let fresh = event("uuid:content-3", 0, "Q:0,2");
        assert_eq!(
            rec.on_notify(&provider, &mut playback, &fresh, callback, now + RETRY)
                .unwrap()
                .unwrap()
                .content,
            ContentChanges::ALL
        );
        assert!(
            rec.on_notify(&provider, &mut playback, &bad_again, callback, now + RETRY)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            rec.subscriptions.content.0.len(),
            1,
            "retired lease evidence is pruned"
        );
    }
}
