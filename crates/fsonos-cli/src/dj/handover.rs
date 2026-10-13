//! Follow a DJ queue when Sonos elects another coordinator, without playing
//! anything or claiming an unrelated queue. Topology proposes a successor;
//! the successor's own queue and current source must confirm the handover.

use super::SpotifyDj;
use fsonos_core::HouseholdState;
use fsonos_core::live::Snapshot;
use fsonos_core::moving;
use fsonos_core::reconcile::Health;
use fsonos_core::store::{Store, StoreError};
use fsonos_proto::content::{BrowseFlag, QUEUE, browse, browse_snapshot};
use fsonos_proto::control::{get_media_info, get_position_info};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_spotify::feed::{FeedSnapshot, QueueFeed};
use fsonos_types::{HouseholdId, PlayerId};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const RETRY: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Origin {
    household: Option<HouseholdId>,
    members: Vec<PlayerId>,
}

/// Remember the old group even when its coordinator disappears entirely.
/// An unknown origin never permits searching unrelated households for music.
#[derive(Default)]
pub(super) struct Tracker {
    origins: HashMap<PlayerId, Origin>,
    attempts: HashMap<PlayerId, (PlayerId, Instant)>,
}

impl Tracker {
    pub(super) fn remember(&mut self, households: &[HouseholdState], from: &PlayerId) {
        for household in households {
            if let Some(group) = household.groups.iter().find(|g| g.coordinator == *from) {
                self.origins.insert(
                    from.clone(),
                    Origin {
                        household: household.id.clone(),
                        members: group.members.clone(),
                    },
                );
                self.attempts.remove(from);
                return;
            }
        }
    }

    fn replacement(&self, snapshot: &Snapshot, from: &PlayerId) -> Option<PlayerId> {
        if snapshot
            .households
            .iter()
            .any(|h| h.groups.iter().any(|g| g.coordinator == *from))
        {
            return None;
        }
        let origin = self.origins.get(from);
        let mut candidates = HashSet::new();
        for household in &snapshot.households {
            if let (Some(old), Some(current)) = (
                origin.and_then(|o| o.household.as_ref()),
                household.id.as_ref(),
            ) && old != current
            {
                continue;
            }
            for group in &household.groups {
                let related = match origin {
                    Some(origin) => origin.members.contains(&group.coordinator),
                    // A restored feed has no previous live topology yet.
                    // Only its own current group's coordinator is eligible.
                    None => group.members.contains(from),
                };
                if related {
                    candidates.insert(group.coordinator.clone());
                }
            }
        }
        // A split into several possible successors must not pick arbitrarily.
        if candidates.len() != 1 {
            return None;
        }
        let to = candidates.into_iter().next()?;
        if !snapshot.households.iter().any(|h| h.player(&to).is_some())
            || snapshot
                .health
                .of(&to)
                .is_some_and(|h| h.health == Health::Offline)
            || snapshot
                .playback
                .of(&to)
                .is_none_or(|p| p.transport.is_none())
        {
            return None;
        }
        Some(to)
    }
}

impl SpotifyDj {
    pub(super) fn reconcile_topology(
        &self,
        transport: &dyn Transport,
        snapshot: &Snapshot,
        store: &mut dyn Store,
    ) {
        self.reconcile_topology_at(transport, snapshot, store, Instant::now());
    }

    fn reconcile_topology_at(
        &self,
        transport: &dyn Transport,
        snapshot: &Snapshot,
        store: &mut dyn Store,
        now: Instant,
    ) {
        let mut state = self.state();
        let mut sources: Vec<PlayerId> = state
            .feeds
            .iter()
            .filter(|(_, feed)| feed.is_active())
            .map(|(id, _)| id.clone())
            .collect();
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        state.handover.origins.retain(|id, _| sources.contains(id));
        state.handover.attempts.retain(|id, _| sources.contains(id));
        for from in &sources {
            state.handover.remember(&snapshot.households, from);
        }
        let candidates: Vec<(PlayerId, PlayerId)> = sources
            .into_iter()
            .filter_map(|from| {
                state
                    .handover
                    .replacement(snapshot, &from)
                    .map(|to| (from, to))
            })
            .collect();
        let mut changed = false;
        for (from, to) in &candidates {
            if state.held.contains(from)
                || state.held.contains(to)
                || state.feeds.get(to).is_some_and(QueueFeed::is_active)
                || candidates.iter().filter(|(_, target)| target == to).count() != 1
                || state
                    .handover
                    .attempts
                    .get(from)
                    .is_some_and(|(target, due)| target == to && now < *due)
            {
                continue;
            }
            let Some(expected) = state.feeds.get(from).and_then(QueueFeed::snapshot) else {
                continue;
            };
            // Volume or topology churn cannot hammer an unready successor.
            state
                .handover
                .attempts
                .insert(from.clone(), (to.clone(), now + RETRY));
            match inherited(transport, snapshot, to, &expected) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    tracing::warn!(from = %from.0, to = %to.0, %error, "DJ handover read failed");
                    continue;
                }
            }
            // Save steering first. A store failure keeps the original feed
            // and its retry intact; no speaker has been changed by this pass.
            let steering = (|| -> Result<(), StoreError> {
                if !moving::rekey_dj_session(store, from, to)? {
                    store.delete_dj_session(&to.0)?;
                }
                Ok(())
            })();
            if let Err(error) = steering {
                tracing::warn!(from = %from.0, to = %to.0, %error, "DJ handover steering not saved");
                continue;
            }
            let Some(mut feed) = state.feeds.remove(from) else {
                continue;
            };
            feed.rekey(to);
            state.feeds.insert(to.clone(), feed);
            if let Some(listening) = state.listening.remove(from) {
                state.listening.insert(to.clone(), listening);
            }
            state.refill_retries.remove(from);
            state.refill_retries.remove(to);
            state.handover.origins.remove(from);
            state.handover.attempts.remove(from);
            state.handover.remember(&snapshot.households, to);
            tracing::info!(from = %from.0, to = %to.0, "DJ followed the inherited queue");
            changed = true;
        }
        if changed {
            self.save(&mut state);
        }
    }
}

/// Read-only proof: the target leads, renders its own queue, still holds
/// every located DJ movement, and is currently on one of those movements.
fn inherited(
    transport: &dyn Transport,
    snapshot: &Snapshot,
    to: &PlayerId,
    expected: &FeedSnapshot,
) -> Result<bool, ProtoError> {
    let Some(player) = snapshot.households.iter().find_map(|h| h.player(to)) else {
        return Ok(false);
    };
    let host = player.ip;
    let topology = get_zone_group_state(transport, host)?;
    if !topology.groups.iter().any(|g| g.coordinator == *to) {
        return Ok(false);
    }
    let queue = browse_snapshot(transport, host, QUEUE)?;
    let tracks: Vec<Option<String>> = queue
        .objects
        .iter()
        .map(|object| {
            object
                .res
                .as_ref()
                .and_then(|res| spotify_uri_from_renderer_uri(&res.uri))
        })
        .collect();
    let position = get_position_info(transport, host)?;
    let media = get_media_info(transport, host)?;
    if media.uri != format!("x-rincon-queue:{}#0", to.0)
        || !matches_queue(expected, &tracks, position.track, &position.uri)
    {
        return Ok(false);
    }
    // Do not adopt a queue edited while position and source were being read.
    let check = browse(transport, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    Ok(check.update_id == queue.update_id
        && usize::try_from(check.total_matches).ok() == Some(tracks.len()))
}

fn matches_queue(
    expected: &FeedSnapshot,
    tracks: &[Option<String>],
    current: u32,
    uri: &str,
) -> bool {
    let Some(uri) = spotify_uri_from_renderer_uri(uri) else {
        return false;
    };
    let mut positions = HashSet::new();
    let mut playing = false;
    for movement in expected.queued.iter().flat_map(|work| &work.movements) {
        let Some(position) = movement.position.filter(|_| movement.added) else {
            continue;
        };
        let Some(index) = position.checked_sub(1).and_then(|p| usize::try_from(p).ok()) else {
            return false;
        };
        if !positions.insert(position)
            || tracks.get(index).and_then(Option::as_deref) != Some(movement.uri.as_str())
        {
            return false;
        }
        playing |= position == current && movement.uri == uri;
    }
    playing
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_api::dj::DjEngine;
    use fsonos_core::playback::EventSource;
    use fsonos_core::store::MemStore;
    use fsonos_proto::control as control;
    use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl};
    use fsonos_proto::gena::Notify;
    use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_spotify::dj::{DjConfig, PickReason};
    use fsonos_spotify::feed::{Movement, QueuedWork};
    use fsonos_spotify::steer::{DjConstraints, DjSession};

    const TRACKS: [&str; 2] = [
        "spotify:track:0000000000000000000001",
        "spotify:track:0000000000000000000002",
    ];

    fn saved(from: &PlayerId) -> FeedSnapshot {
        FeedSnapshot {
            zone: from.0.clone(),
            earlier: Vec::new(),
            lookahead: 1,
            queued: vec![QueuedWork {
                title: "Synthetic work".into(),
                composer: "Synthetic composer".into(),
                work_key: "synthetic-work".into(),
                movements: TRACKS
                    .iter()
                    .enumerate()
                    .map(|(i, uri)| Movement {
                        uri: (*uri).into(),
                        title: format!("Movement {i}"),
                        position: Some(u32::try_from(i + 1).unwrap()),
                        added: true,
                    })
                    .collect(),
                reason: PickReason {
                    factors: Vec::new(),
                    relaxed: Vec::new(),
                    summary: "fixture".into(),
                },
            }],
            playing: Some(1),
            recorded: Some((1, TRACKS[0].into())),
            played: Vec::new(),
        }
    }

    fn snapshot(sim: &SimHandle) -> Snapshot {
        let mut household = HouseholdState::default();
        let lan = sim.lan();
        household.apply_topology(
            &get_zone_group_state(&lan, sim.player("Office").unwrap().ip).unwrap(),
        );
        let mut snapshot = Snapshot {
            households: vec![household],
            ..Snapshot::default()
        };
        let event = Notify {
            sid: "uuid:handover-test".into(),
            seq: 0,
            path: "/AVTransport".into(),
            properties: vec![(
                "LastChange".into(),
                "<Event><InstanceID val=\"0\"><TransportState val=\"PLAYING\"/>\
                 </InstanceID></Event>"
                    .into(),
            )],
        };
        for group in &snapshot.households[0].groups {
            snapshot
                .playback
                .apply(&group.coordinator, EventSource::AvTransport, &event, Instant::now())
                .unwrap();
        }
        snapshot
    }

    fn fixture() -> (SimHandle, SpotifyDj, MemStore, PlayerId, PlayerId) {
        let sim = SimHousehold::builder()
            .s2([
                SimPlayerSpec::new("Den", SimModel::One),
                SimPlayerSpec::new("Office", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = sim.lan();
        let from = PlayerId(sim.player("Den").unwrap().uuid.clone());
        let to = PlayerId(sim.player("Office").unwrap().uuid.clone());
        let host = sim.player("Den").unwrap().ip;
        control::join_group(&lan, sim.player("Office").unwrap().ip, &from).unwrap();
        let before = snapshot(&sim);
        let params = fsonos_core::control::spotify_params(&lan, &before.households, &from)
            .unwrap()
            .unwrap();
        for uri in TRACKS {
            control::add_uri_to_queue(
                &lan,
                host,
                &spotify_queue_uri(uri),
                &spotify_track_didl(uri, "Synthetic movement", &params),
                false,
            )
            .unwrap();
        }
        control::play_from_queue(&lan, host, &from).unwrap();
        control::play(&lan, host).unwrap();
        let dj = SpotifyDj::new(None);
        dj.state().feeds.insert(
            from.clone(),
            QueueFeed::restore(saved(&from), DjConfig::default(), 1),
        );
        let mut store = MemStore::default();
        let steering = DjSession {
            zone: from.0.clone(),
            mood: Some("calm".into()),
            constraints: DjConstraints::default(),
            expires_at: Some(10_000),
        };
        store.save_dj_session(&steering.to_store().unwrap()).unwrap();
        dj.on_topology(&lan, &before, &mut store);
        (sim, dj, store, from, to)
    }

    #[test]
    fn reelection_moves_the_feed_steering_and_history_without_speaker_writes() {
        let (sim, dj, mut store, from, to) = fixture();
        sim.reelect_coordinator("Den").unwrap();
        let after = snapshot(&sim);
        let before = sim.soap_log().len();
        dj.on_topology(&sim.lan(), &after, &mut store);
        assert!(!dj.feeds(&from));
        assert!(dj.feeds(&to));
        let adopted = dj.state().feeds[&to].snapshot().unwrap();
        assert_eq!(adopted.earlier, vec![from.0.clone()]);
        assert_eq!(adopted.queued, saved(&from).queued);
        assert_eq!(adopted.recorded, saved(&from).recorded);
        assert!(store.dj_session(&from.0).unwrap().is_none());
        assert!(store.dj_session(&to.0).unwrap().is_some());
        assert!(sim.soap_log()[before..].iter().all(|entry| {
            matches!(
                entry.action.as_str(),
                "Browse" | "GetPositionInfo" | "GetMediaInfo" | "GetZoneGroupState"
            )
        }));
        let reads = sim.soap_log().len();
        dj.on_topology(&sim.lan(), &after, &mut store);
        assert_eq!(sim.soap_log().len(), reads, "settled ownership needs no reads");
    }

    #[test]
    fn vanished_source_can_follow_its_remembered_group_but_not_a_replaced_queue() {
        let (sim, dj, mut store, from, to) = fixture();
        sim.reelect_coordinator("Den").unwrap();
        sim.set_offline("Den", true).unwrap();
        let after = snapshot(&sim);
        assert!(after.households.iter().all(|h| h.player(&from).is_none()));
        let host = sim.player("Office").unwrap().ip;
        control::remove_all_tracks_from_queue(&sim.lan(), host).unwrap();
        dj.on_topology(&sim.lan(), &after, &mut store);
        assert!(dj.feeds(&from));
        assert!(!dj.feeds(&to));
        assert!(store.dj_session(&from.0).unwrap().is_some());
    }

    #[test]
    fn held_moves_and_existing_destination_feeds_are_never_overwritten() {
        let (sim, dj, mut store, from, to) = fixture();
        sim.reelect_coordinator("Den").unwrap();
        let after = snapshot(&sim);
        dj.moving(&from);
        let reads = sim.soap_log().len();
        dj.on_topology(&sim.lan(), &after, &mut store);
        assert_eq!(sim.soap_log().len(), reads);
        dj.moved(&from, &from);
        dj.state().feeds.insert(
            to.clone(),
            QueueFeed::restore(saved(&to), DjConfig::default(), 2),
        );
        dj.on_topology(&sim.lan(), &after, &mut store);
        assert_eq!(sim.soap_log().len(), reads);
        assert!(dj.feeds(&from) && dj.feeds(&to));
    }

    #[test]
    fn failed_successor_reads_retry_without_losing_the_source_feed() {
        let (sim, dj, mut store, from, to) = fixture();
        sim.reelect_coordinator("Den").unwrap();
        let after = snapshot(&sim);
        let now = Instant::now();
        sim.upnp_fault("Office", "Browse", 701).unwrap();
        dj.reconcile_topology_at(&sim.lan(), &after, &mut store, now);
        assert!(dj.feeds(&from));
        let reads = sim.soap_log().len();
        sim.clear_faults("Office").unwrap();
        dj.reconcile_topology_at(&sim.lan(), &after, &mut store, now + RETRY / 2);
        assert_eq!(sim.soap_log().len(), reads);
        dj.reconcile_topology_at(&sim.lan(), &after, &mut store, now + RETRY);
        assert!(!dj.feeds(&from));
        assert!(dj.feeds(&to));
    }

    #[test]
    fn queue_proof_rejects_empty_partial_shifted_and_different_tracks() {
        let from = PlayerId("test-origin".into());
        let expected = saved(&from);
        let tracks: Vec<Option<String>> = TRACKS.iter().map(|uri| Some((*uri).into())).collect();
        assert!(matches_queue(&expected, &tracks, 1, TRACKS[0]));
        assert!(matches_queue(&expected, &tracks, 2, TRACKS[1]));
        assert!(!matches_queue(&expected, &[], 1, TRACKS[0]));
        assert!(!matches_queue(&expected, &tracks[..1], 1, TRACKS[0]));
        assert!(!matches_queue(&expected, &tracks, 2, TRACKS[0]));
        let mut shifted = vec![None];
        shifted.extend(tracks);
        assert!(!matches_queue(&expected, &shifted, 2, TRACKS[0]));
        let mut empty = expected;
        empty.queued.clear();
        assert!(!matches_queue(&empty, &shifted, 2, TRACKS[0]));
    }
}
