//! Keeping a coordinator's queue fed with whole works.
//!
//! The DJ plans [`PlannedWork`]s; [`QueueFeed`] puts them on the speakers.
//! Every movement of a work is enqueued in one call, in order — never part of
//! one — appended after whatever the queue already holds, so the owner's own
//! queue is left alone. The feed keeps [`QueueFeed::lookahead`] more works
//! queued beyond the one playing: when playback enters the last queued work it
//! tops the queue up, so a finishing work always has a successor waiting.
//!
//! It is driven by the coordinator's playback state, which the daemon folds
//! from GENA AVTransport events (`fsonos_core::events` / `playback`):
//! [`QueueFeed::on_playback`] notices a new track from the DJ's queue, records
//! the play for the DJ's anti-repeat, and tops up. [`QueueFeed::skip`] skips
//! the rest of the current work; [`QueueFeed::stop`] stops.
//!
//! Synchronous and generic over the proto `Transport` and the core `Store`, so
//! the daemon drives it from its event loop and tests drive it against
//! `fsonos-sim` over real localhost sockets.

use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::{Store, StoreError};
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;

use crate::dj::{DjConfig, PickContext, PickReason, PlayRecord, Rng, WorkPool, pick_next};
use crate::steer::Steer;

/// Where the feed plays: the transport, the households' state, and the group
/// coordinator it feeds.
pub struct Speakers<'a, T: ?Sized> {
    pub transport: &'a T,
    pub households: &'a [HouseholdState],
    pub coordinator: &'a PlayerId,
}

/// What the DJ plans from right now.
#[derive(Debug, Clone, Copy)]
pub struct Planning<'a> {
    pub pool: &'a WorkPool,
    pub steer: Option<&'a Steer>,
    /// Unix seconds, stamped by the caller.
    pub now: i64,
    /// Local hour 0–23, for the time-of-day energy target.
    pub local_hour: Option<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("the DJ has nothing to play: the library cache holds no classical works")]
    EmptyPool,
    #[error(
        "this household can't play Spotify yet: add any Spotify track to its Sonos \
         favorites so FrankenSonos can learn how this household plays Spotify"
    )]
    NoRenderParams,
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One work on the coordinator's queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedWork {
    pub title: String,
    pub composer: String,
    pub work_key: String,
    /// 1-based queue position of its first movement.
    pub first: u32,
    /// Its movements' `spotify:track:` URIs, in playing order.
    pub tracks: Vec<String>,
    /// Why the DJ chose it.
    pub reason: PickReason,
}

impl QueuedWork {
    /// Queue position of its last movement.
    #[must_use]
    pub fn last(&self) -> u32 {
        self.first
            + u32::try_from(self.tracks.len())
                .unwrap_or(u32::MAX)
                .saturating_sub(1)
    }

    #[must_use]
    pub fn contains(&self, position: u32) -> bool {
        (self.first..=self.last()).contains(&position)
    }

    /// The movement at a queue position, if it is one of this work's.
    #[must_use]
    pub fn track_at(&self, position: u32) -> Option<&str> {
        let i = usize::try_from(position.checked_sub(self.first)?).ok()?;
        self.tracks.get(i).map(String::as_str)
    }
}

/// A DJ session's hold on one coordinator's queue.
#[derive(Debug, Clone)]
pub struct QueueFeed {
    /// The zone plays are recorded under: the coordinator's id.
    zone: String,
    config: DjConfig,
    rng: Rng,
    /// Whole works kept queued beyond the one playing.
    pub lookahead: usize,
    /// The DJ's works on the queue from the current one on, in queue order.
    queued: Vec<QueuedWork>,
    /// The queue position last seen playing.
    playing: Option<u32>,
}

impl QueueFeed {
    #[must_use]
    pub fn new(coordinator: &PlayerId, config: DjConfig, seed: u64) -> Self {
        Self {
            zone: coordinator.0.clone(),
            config,
            rng: Rng::new(seed),
            lookahead: 1,
            queued: Vec::new(),
            playing: None,
        }
    }

    /// The DJ's works on the queue, the current one first.
    #[must_use]
    pub fn queued(&self) -> &[QueuedWork] {
        &self.queued
    }

    /// The work playing now, if the queue is on one of the DJ's works.
    #[must_use]
    pub fn current(&self) -> Option<&QueuedWork> {
        self.playing
            .and_then(|p| self.queued.iter().find(|w| w.contains(p)))
    }

    /// Start: queue the first work and `lookahead` more, and play from the
    /// first. Returns the queued works.
    pub fn start<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &mut S,
    ) -> Result<&[QueuedWork], FeedError> {
        self.queued.clear();
        self.playing = None;
        self.enqueue(at, plan, &*store, 1 + self.lookahead)?;
        let first = self.queued[0].first;
        control::play_queue_from(at.transport, at.households, at.coordinator, first)?;
        Ok(&self.queued)
    }

    /// Fold in the coordinator's latest playback state (after a GENA event).
    /// When a new track from the DJ's queue starts, record the play and, once
    /// the last queued work has begun, queue more. Returns how many works it
    /// queued. Tracks outside the DJ's works (the owner's own queue items) are
    /// left alone.
    pub fn on_playback<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &mut S,
        playback: &PlayerPlayback,
    ) -> Result<usize, FeedError> {
        let Some(position) = playback.queue_position else {
            return Ok(0);
        };
        if self.playing == Some(position) {
            return Ok(0);
        }
        self.playing = Some(position);
        let Some(index) = self.queued.iter().position(|w| w.contains(position)) else {
            return Ok(0);
        };
        // Finished works are history now (the store has their plays).
        self.queued.drain(..index);
        if let Some(uri) = self.queued[0].track_at(position) {
            store.record_play(&self.zone, uri, plan.now)?;
        }
        let ahead = self.queued.len() - 1;
        if ahead >= self.lookahead {
            return Ok(0);
        }
        let more = self.lookahead - ahead;
        self.enqueue(at, plan, &*store, more)?;
        Ok(more)
    }

    /// Skip the rest of the current work: play the next queued work from its
    /// first movement, queuing one if none is waiting. The play is recorded
    /// (and the queue topped up) when its playback event arrives.
    pub fn skip<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &mut S,
    ) -> Result<&QueuedWork, FeedError> {
        let next = match self
            .playing
            .and_then(|p| self.queued.iter().position(|w| w.contains(p)))
        {
            Some(current) => current + 1,
            None => 0,
        };
        if next >= self.queued.len() {
            self.enqueue(at, plan, &*store, next + 1 - self.queued.len())?;
        }
        control::play_queue_from(
            at.transport,
            at.households,
            at.coordinator,
            self.queued[next].first,
        )?;
        Ok(&self.queued[next])
    }

    /// Stop playback and let go of the queue (its items stay; nothing more
    /// is added).
    pub fn stop<T: Transport + ?Sized>(&mut self, at: &Speakers<'_, T>) -> Result<(), FeedError> {
        control::stop(at.transport, at.households, at.coordinator)?;
        self.queued.clear();
        self.playing = None;
        Ok(())
    }

    /// Pick and enqueue `count` whole works.
    fn enqueue<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &S,
        count: usize,
    ) -> Result<(), FeedError> {
        if plan.pool.is_empty() {
            return Err(FeedError::EmptyPool);
        }
        let mut history = self.history(store, plan.now)?;
        for _ in 0..count {
            let ctx = PickContext {
                history: &history,
                now: Some(plan.now),
                local_hour: plan.local_hour,
                energy_target: None,
                steer: plan.steer,
            };
            let work = pick_next(plan.pool, &ctx, &self.config, &mut self.rng)
                .ok_or(FeedError::EmptyPool)?;
            let tracks: Vec<(&str, &str)> = work
                .movements
                .iter()
                .map(|m| (m.track.source_uri.as_str(), m.track.title.as_str()))
                .collect();
            let first = control::queue_spotify_tracks(
                at.transport,
                at.households,
                at.coordinator,
                &tracks,
            )?
            .ok_or(FeedError::NoRenderParams)?;
            let uris: Vec<String> = tracks.iter().map(|&(uri, _)| uri.to_owned()).collect();
            history.extend(uris.iter().map(|uri| PlayRecord::at(uri.clone(), plan.now)));
            self.queued.push(QueuedWork {
                title: work.work.title.clone(),
                composer: work.work.composer.clone(),
                work_key: work.work.work_key.clone(),
                first,
                tracks: uris,
                reason: work.reason,
            });
        }
        Ok(())
    }

    /// The zone's recorded plays, then the queued works not yet reached, as
    /// if just played: the DJ must not pick what is already waiting.
    fn history<S: Store + ?Sized>(
        &self,
        store: &S,
        now: i64,
    ) -> Result<Vec<PlayRecord>, FeedError> {
        let mut history: Vec<PlayRecord> = store
            .recent_plays(Some(&self.zone), self.config.history_horizon)?
            .into_iter()
            .map(|p| PlayRecord::at(p.source_uri, p.played_at))
            .collect();
        let reached = self.playing.unwrap_or(0);
        for work in &self.queued {
            for (offset, uri) in (0u32..).zip(&work.tracks) {
                if work.first + offset > reached {
                    history.push(PlayRecord::at(uri.clone(), now));
                }
            }
        }
        Ok(history)
    }
}

#[cfg(test)]
mod tests {
    //! End to end against `fsonos-sim` over real loopback sockets: the DJ's
    //! works go onto a simulated S2 coordinator's queue through core's
    //! control, and the coordinator's own GENA AVTransport events drive the
    //! feed. The simulator doesn't advance on its own, so the tests step
    //! through tracks with `next`, as a listener's speaker would.

    use std::time::{Duration, Instant};

    use fsonos_core::playback::{EventSource, Playback};
    use fsonos_core::resolve_room;
    use fsonos_core::store::MemStore;
    use fsonos_proto::content::{QUEUE, browse_all};
    use fsonos_proto::didl::spotify_uri_from_renderer_uri;
    use fsonos_proto::net::{EventSink, Lan};
    use fsonos_proto::soap::AV_TRANSPORT;
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
    use fsonos_types::TransportState;

    use super::*;
    use crate::test_shelf::{MIDNIGHT, shelf_items, works_of};

    const ROOM: &str = "Living Room";

    /// One simulated S2 player, its household snapshot, and a GENA
    /// subscription to its AVTransport events.
    struct Rig {
        _sim: SimHandle,
        lan: SimLan,
        houses: Vec<HouseholdState>,
        coordinator: PlayerId,
        _events: Lan,
        sink: EventSink,
        sid: String,
        playback: Playback,
    }

    impl Rig {
        fn new() -> Self {
            let sim = SimHousehold::builder()
                .s2([SimPlayerSpec::new(ROOM, SimModel::One)])
                .spawn()
                .unwrap();
            let lan = sim.lan();
            let mut house = HouseholdState::default();
            house
                .apply_topology(&get_zone_group_state(&lan, sim.player(ROOM).unwrap().ip).unwrap());
            let houses = vec![house];
            let coordinator = resolve_room(&houses, ROOM).unwrap().coordinator.id.clone();
            let events = Lan::start().unwrap();
            let sink = EventSink::start("127.0.0.1:0".parse().unwrap()).unwrap();
            let url = format!(
                "{}{}",
                sim.player(ROOM).unwrap().base_url,
                AV_TRANSPORT.event_path
            );
            let sid = events
                .subscribe_at(&url, &sink.callback_url("avt"), 600)
                .unwrap()
                .sid;
            let mut rig = Self {
                _sim: sim,
                lan,
                houses,
                coordinator,
                _events: events,
                sink,
                sid,
                playback: Playback::default(),
            };
            rig.drain(); // the initial event
            rig
        }

        fn speakers(&self) -> Speakers<'_, SimLan> {
            Speakers {
                transport: &self.lan,
                households: &self.houses,
                coordinator: &self.coordinator,
            }
        }

        /// Fold the AVTransport NOTIFYs that arrive into the playback state;
        /// returns how many arrived.
        fn drain(&mut self) -> usize {
            let mut seen = 0;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match self.sink.recv_timeout(Duration::from_millis(if seen == 0 {
                    200
                } else {
                    150
                })) {
                    Some(n) if n.sid == self.sid => {
                        self.playback
                            .apply(
                                &self.coordinator,
                                EventSource::AvTransport,
                                &n,
                                Instant::now(),
                            )
                            .unwrap();
                        seen += 1;
                    }
                    None if seen > 0 => break,
                    Some(_) | None => {}
                }
            }
            seen
        }

        /// Deliver this player's events to the feed, as the daemon would.
        fn pump(
            &mut self,
            feed: &mut QueueFeed,
            plan: Planning<'_>,
            store: &mut MemStore,
        ) -> usize {
            assert!(self.drain() > 0, "no AVTransport event arrived");
            let state = self
                .playback
                .of(&self.coordinator)
                .expect("a playback state");
            feed.on_playback(&self.speakers(), plan, store, state)
                .unwrap()
        }

        /// The coordinator's queue as `spotify:track:` URIs.
        fn queue(&self) -> Vec<String> {
            let host = control::locate(&self.houses, &self.coordinator).unwrap().ip;
            browse_all(&self.lan, host, QUEUE)
                .unwrap()
                .iter()
                .filter_map(|o| {
                    o.res
                        .as_ref()
                        .and_then(|r| spotify_uri_from_renderer_uri(&r.uri))
                })
                .collect()
        }

        fn position(&self) -> u32 {
            control::playback(&self.lan, &self.houses, &self.coordinator)
                .unwrap()
                .position
                .track
        }
    }

    fn plan(pool: &WorkPool, now: i64) -> Planning<'_> {
        Planning {
            pool,
            steer: None,
            now,
            local_hour: Some(20),
        }
    }

    #[test]
    fn whole_works_are_queued_and_topped_up_as_they_play() {
        let mut rig = Rig::new();
        let pool = works_of(&shelf_items(1));
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);

        let started = feed
            .start(&rig.speakers(), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(started.len(), 2, "the first work and one more");
        assert_eq!(started[0].first, 1);
        assert_eq!(started[1].first, started[0].last() + 1);
        let expected: Vec<String> = started.iter().flat_map(|w| w.tracks.clone()).collect();
        assert_eq!(rig.queue(), expected, "every movement, in order");
        // Every queued work is exactly a pool work's movements, in order.
        for w in &started {
            let work = pool.work_of(&w.tracks[0]).unwrap();
            let movements: Vec<&str> = work
                .movements
                .iter()
                .map(|m| m.track.source_uri.as_str())
                .collect();
            assert_eq!(movements, w.tracks, "{} split or reordered", w.title);
        }
        rig.pump(&mut feed, plan(&pool, MIDNIGHT), &mut store);
        assert_eq!(rig.position(), 1);
        assert_eq!(feed.current().unwrap().title, started[0].title);

        // Play through 30 tracks; the queue never runs dry and every work
        // plays whole, in order.
        let mut queue_tops = 0;
        for step in 1..=30 {
            control::next(&rig.lan, &rig.houses, &rig.coordinator).unwrap();
            queue_tops += rig.pump(&mut feed, plan(&pool, MIDNIGHT + 300 * step), &mut store);
            let current = feed.current().expect("on a DJ work");
            assert!(
                feed.queued().len() >= 2,
                "a work is always waiting after the current one"
            );
            assert!(feed.queued().last().unwrap().first > rig.position());
            assert_eq!(
                current.track_at(rig.position()),
                rig.queue()
                    .get(rig.position() as usize - 1)
                    .map(String::as_str)
            );
        }
        assert!(queue_tops >= 2, "the feed topped the queue up");

        // The plays the DJ recorded are exactly the tracks heard, in order.
        let zone = rig.coordinator.0.as_str();
        let plays: Vec<String> = store
            .recent_plays(Some(zone), 100)
            .unwrap()
            .into_iter()
            .map(|p| p.source_uri)
            .collect();
        let heard: Vec<String> = rig.queue()[..31].to_vec();
        assert_eq!(plays, heard);
        // And the queue is whole works back to back: each starts with its
        // first movement and holds all of them.
        let mut i = 0;
        let queue = rig.queue();
        while i < queue.len() {
            let work = pool.work_of(&queue[i]).unwrap();
            let len = work.movements.len();
            let run: Vec<&str> = queue[i..i + len].iter().map(String::as_str).collect();
            let whole: Vec<&str> = work
                .movements
                .iter()
                .map(|m| m.track.source_uri.as_str())
                .collect();
            assert_eq!(run, whole, "work at queue position {}", i + 1);
            i += len;
        }
    }

    #[test]
    fn skip_jumps_to_the_next_whole_work_and_stop_stops() {
        let mut rig = Rig::new();
        let pool = works_of(&shelf_items(1));
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 11);
        feed.start(&rig.speakers(), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        rig.pump(&mut feed, plan(&pool, MIDNIGHT), &mut store);
        let first = feed.current().unwrap().clone();

        let next = feed
            .skip(&rig.speakers(), plan(&pool, MIDNIGHT + 60), &mut store)
            .unwrap()
            .clone();
        assert_eq!(next.first, first.last() + 1);
        rig.pump(&mut feed, plan(&pool, MIDNIGHT + 60), &mut store);
        assert_eq!(
            rig.position(),
            next.first,
            "on the next work's first movement"
        );
        assert_eq!(feed.current().unwrap().work_key, next.work_key);
        assert_eq!(feed.queued().len(), 2, "topped up behind the new work");
        let plays: Vec<String> = store
            .recent_plays(None, 10)
            .unwrap()
            .into_iter()
            .map(|p| p.source_uri)
            .collect();
        assert_eq!(
            plays,
            [first.tracks[0].clone(), next.tracks[0].clone()],
            "the skipped rest never played"
        );

        feed.stop(&rig.speakers()).unwrap();
        rig.drain();
        let state = control::playback(&rig.lan, &rig.houses, &rig.coordinator)
            .unwrap()
            .transport
            .state;
        assert_eq!(state, TransportState::Stopped);
        assert!(feed.queued().is_empty() && feed.current().is_none());
    }

    #[test]
    fn the_owners_queue_is_left_alone_and_empty_libraries_are_explained() {
        let rig = Rig::new();
        // Something the owner queued before the DJ started.
        let owner = "spotify:track:0OwnerQueued0000000001";
        let at = control::queue_spotify_tracks(
            &rig.lan,
            &rig.houses,
            &rig.coordinator,
            &[(owner, "Owner")],
        )
        .unwrap();
        assert_eq!(at, Some(1));
        let pool = works_of(&shelf_items(1));
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 3);
        let started = feed
            .start(&rig.speakers(), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(started[0].first, 2, "appended after the owner's item");
        assert_eq!(rig.queue()[0], owner);
        assert_eq!(rig.position(), 2, "plays from the DJ's first work");

        let empty = WorkPool::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 3);
        let err = feed
            .start(&rig.speakers(), plan(&empty, MIDNIGHT), &mut store)
            .unwrap_err();
        assert!(matches!(err, FeedError::EmptyPool), "{err}");
        assert!(err.to_string().contains("no classical works"));
    }
}
