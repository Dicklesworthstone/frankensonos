//! Keeping a coordinator's queue fed with whole works.
//!
//! The DJ plans [`PlannedWork`](crate::dj::PlannedWork)s; [`QueueFeed`] puts
//! them on the speakers. A work's movements go onto the queue together and in
//! order — never part of one by choice — appended after whatever the queue
//! already holds, so the owner's own queue is left alone. The feed keeps
//! [`QueueFeed::lookahead`] more works queued beyond the one playing: when
//! playback enters the last queued work it tops the queue up, so a finishing
//! work always has a successor waiting.
//!
//! It is driven by the coordinator's playback state, which the daemon folds
//! from GENA AVTransport events (`fsonos_core::events` / `playback`).
//! [`QueueFeed::on_playback`] checks the playing track and the queue's length
//! against its model of the queue — re-reading the queue when the owner has
//! inserted, removed or replaced items, and stepping aside when none of the
//! DJ's works is left on it — records each new DJ track once, and tops up. A
//! top-up that fails (a network error, a fault) is retried on the next
//! event; a work that only partly made it onto the queue is completed in
//! place, right after its first movements, so it still plays whole.
//! [`QueueFeed::skip`] re-reads the queue and skips the rest of the current
//! work; [`QueueFeed::stop`] stops.
//!
//! Synchronous and generic over the proto `Transport` and the core `Store`, so
//! the daemon drives it from its event loop and tests drive it against
//! `fsonos-sim` over real localhost sockets.

use std::net::IpAddr;

use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::{Store, StoreError};
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control::get_position_info;
use fsonos_proto::didl::{
    SpotifyRenderParams, spotify_queue_uri, spotify_track_didl, spotify_uri_from_renderer_uri,
};
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;

use crate::dj::{DjConfig, PickContext, PickReason, PlayRecord, Rng, WorkPool, pick_next};
use crate::feedback::FeedbackModel;
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
    /// The owner's feedback, decayed to now.
    pub feedback: Option<&'a FeedbackModel>,
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
    #[error(
        "the DJ isn't running on this zone (stopped, or the queue was cleared or replaced): \
         start it again"
    )]
    Inactive,
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One movement of a queued work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Movement {
    /// `spotify:track:` URI.
    pub uri: String,
    pub title: String,
    /// Where it sits on the queue; `None` if it isn't there (not added yet,
    /// or removed by the owner).
    pub position: Option<u32>,
    /// Whether the feed has added it (a failed top-up can leave some unadded).
    pub added: bool,
}

/// One of the DJ's works on the coordinator's queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedWork {
    pub title: String,
    pub composer: String,
    pub work_key: String,
    /// In playing order.
    pub movements: Vec<Movement>,
    /// Why the DJ chose it.
    pub reason: PickReason,
}

impl QueuedWork {
    /// Queue position of its first movement on the queue.
    #[must_use]
    pub fn first(&self) -> Option<u32> {
        self.movements.iter().find_map(|m| m.position)
    }

    /// Queue position of its last movement on the queue.
    #[must_use]
    pub fn last(&self) -> Option<u32> {
        self.movements.iter().rev().find_map(|m| m.position)
    }

    /// Every movement has been added.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.movements.iter().all(|m| m.added)
    }

    #[must_use]
    pub fn contains(&self, position: u32) -> bool {
        self.track_at(position).is_some()
    }

    /// The movement at a queue position, if it is one of this work's.
    #[must_use]
    pub fn track_at(&self, position: u32) -> Option<&str> {
        self.movements
            .iter()
            .find(|m| m.position == Some(position))
            .map(|m| m.uri.as_str())
    }

    /// Its movements' URIs, in order.
    #[must_use]
    pub fn tracks(&self) -> Vec<&str> {
        self.movements.iter().map(|m| m.uri.as_str()).collect()
    }
}

/// A DJ session's hold on one coordinator's queue.
#[derive(Debug, Clone)]
pub struct QueueFeed {
    /// The zone plays are recorded under: the coordinator's id.
    zone: String,
    config: DjConfig,
    rng: Rng,
    /// Whole works kept queued beyond the one playing (at least 1).
    pub lookahead: usize,
    /// Started, and neither stopped nor let go.
    active: bool,
    /// The DJ's works from the current one on, in queue order.
    queued: Vec<QueuedWork>,
    /// The queue position last seen playing (or jumped to).
    playing: Option<u32>,
    /// The last play recorded: (position, URI), so repeated events for one
    /// track record it once. It moves with the track when the queue shifts.
    recorded: Option<(u32, String)>,
    /// How long the feed believes the queue is; an event reporting another
    /// length means the queue changed under it.
    queue_len: Option<u32>,
}

impl QueueFeed {
    #[must_use]
    pub fn new(coordinator: &PlayerId, config: DjConfig, seed: u64) -> Self {
        Self {
            zone: coordinator.0.clone(),
            config,
            rng: Rng::new(seed),
            lookahead: 1,
            active: false,
            queued: Vec::new(),
            playing: None,
            recorded: None,
            queue_len: None,
        }
    }

    /// The DJ's works on the queue, the current one first.
    #[must_use]
    pub fn queued(&self) -> &[QueuedWork] {
        &self.queued
    }

    /// The work playing now, if the queue is on one of the DJ's movements.
    #[must_use]
    pub fn current(&self) -> Option<&QueuedWork> {
        let at = self.playing?;
        self.queued.iter().find(|w| w.contains(at))
    }

    /// Whether the feed is running: started, not stopped, and not let go
    /// because the owner cleared or replaced the queue.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    fn lookahead(&self) -> usize {
        self.lookahead.max(1)
    }

    /// Start (or, after a failure, resume starting): queue the first work and
    /// `lookahead` more, and play from the first. Returns the queued works.
    pub fn start<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &mut S,
    ) -> Result<&[QueuedWork], FeedError> {
        if self.active {
            // Resuming: the queue may have changed since.
            self.resync(at)?;
        } else {
            self.release();
            self.active = true;
        }
        self.finish_partial(at)?;
        let want = 1 + self.lookahead();
        if self.queued.len() < want {
            self.enqueue(at, plan, &*store, want - self.queued.len())?;
        }
        let first = self.queued[0]
            .first()
            .expect("a queued work has a queued movement");
        control::play_queue_from(at.transport, at.households, at.coordinator, first)?;
        self.playing = Some(first);
        Ok(&self.queued)
    }

    /// Fold in the coordinator's latest playback state (after a GENA event).
    /// Re-read the queue when the playing track isn't where the feed expects
    /// it or the queue's length changed (the owner inserted, removed or
    /// replaced items); let go if the owner cleared or replaced the queue;
    /// record a new DJ track once; and keep `lookahead` works queued beyond
    /// the current one — retrying a top-up that failed before. Returns how
    /// many works it queued. The owner's own items are left alone.
    pub fn on_playback<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &mut S,
        playback: &PlayerPlayback,
    ) -> Result<usize, FeedError> {
        if !self.active {
            return Ok(0);
        }
        let position = playback.queue_position.filter(|&p| p > 0);
        let uri = playback
            .track_uri
            .as_deref()
            .and_then(spotify_uri_from_renderer_uri);
        let moved = position.is_some_and(|p| !self.agrees(p, uri.as_deref()));
        let resized = playback
            .queue_length
            .is_some_and(|n| self.queue_len != Some(n));
        if moved || resized {
            let had = !self.queued.is_empty();
            self.resync(at)?;
            if had && self.queued.is_empty() {
                // None of the DJ's works is on the queue any more: the owner
                // cleared or replaced it, and the DJ steps aside.
                self.release();
                return Ok(0);
            }
        }
        if let Some(position) = position {
            self.playing = Some(position);
            // Works wholly behind the playing position are history now.
            self.queued
                .retain(|w| w.last().is_some_and(|last| last >= position) || !w.is_whole());
            if let Some(uri) =
                uri.filter(|u| self.current().and_then(|w| w.track_at(position)) == Some(u))
                && self.recorded.as_ref() != Some(&(position, uri.clone()))
            {
                store.record_play(&self.zone, &uri, plan.now)?;
                self.recorded = Some((position, uri));
            }
        }
        self.top_up(at, plan, &*store)
    }

    /// Skip the rest of the current work (or the owner's item playing): play
    /// the next DJ work from its first movement, queuing one if none waits.
    /// The queue and the playing position are read afresh first, so an
    /// insert or removal the feed hasn't heard about yet can't make it land
    /// mid-work or past the end.
    pub fn skip<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &mut S,
    ) -> Result<&QueuedWork, FeedError> {
        if !self.active {
            return Err(FeedError::Inactive);
        }
        self.resync(at)?;
        let track = get_position_info(at.transport, host_of(at)?)?.track;
        if track > 0 {
            self.playing = Some(track);
        }
        self.finish_partial(at)?;
        let playing = self.playing.unwrap_or(0);
        let waiting = self
            .queued
            .iter()
            .position(|w| w.first().is_some_and(|first| first > playing));
        let next = if let Some(next) = waiting {
            next
        } else {
            self.enqueue(at, plan, &*store, 1)?;
            self.queued.len() - 1
        };
        let first = self.queued[next]
            .first()
            .expect("a queued work has a queued movement");
        control::play_queue_from(at.transport, at.households, at.coordinator, first)?;
        self.playing = Some(first);
        Ok(&self.queued[next])
    }

    /// Stop playback and let go of the queue (its items stay; nothing more
    /// is added).
    pub fn stop<T: Transport + ?Sized>(&mut self, at: &Speakers<'_, T>) -> Result<(), FeedError> {
        control::stop(at.transport, at.households, at.coordinator)?;
        self.release();
        Ok(())
    }

    /// Forget the queue and stop feeding it.
    fn release(&mut self) {
        self.active = false;
        self.queued.clear();
        self.playing = None;
        self.recorded = None;
        self.queue_len = None;
    }

    /// Whether the playing (position, track) matches the feed's model of the
    /// queue: a DJ position must hold the DJ track there, and a DJ track must
    /// be at its DJ position.
    fn agrees(&self, position: u32, uri: Option<&str>) -> bool {
        let expected = self.queued.iter().find_map(|w| w.track_at(position));
        match (expected, uri) {
            (Some(expected), Some(uri)) => expected == uri,
            (Some(_), None) => false,
            (None, Some(uri)) => !self
                .queued
                .iter()
                .any(|w| w.movements.iter().any(|m| m.uri == uri)),
            (None, None) => true,
        }
    }

    /// Re-place every queued movement by reading the queue: each work's
    /// movements are found in order after the previous work's; a movement
    /// no longer there (removed by the owner) loses its position, and a work
    /// with nothing left on the queue is dropped. The recorded play moves
    /// with its track.
    fn resync<T: Transport + ?Sized>(&mut self, at: &Speakers<'_, T>) -> Result<(), FeedError> {
        let host = host_of(at)?;
        let queue: Vec<Option<String>> = browse_all(at.transport, host, QUEUE)?
            .iter()
            .map(|o| {
                o.res
                    .as_ref()
                    .and_then(|r| spotify_uri_from_renderer_uri(&r.uri))
            })
            .collect();
        let recorded = self.recorded.as_ref().and_then(|(position, uri)| {
            self.queued.iter().enumerate().find_map(|(w, work)| {
                work.movements
                    .iter()
                    .position(|m| m.position == Some(*position) && m.uri == *uri)
                    .map(|m| (w, m))
            })
        });
        let mut cursor = 0;
        for work in &mut self.queued {
            for m in work.movements.iter_mut().filter(|m| m.added) {
                let found = queue[cursor..]
                    .iter()
                    .position(|q| q.as_deref() == Some(m.uri.as_str()))
                    .map(|i| cursor + i);
                m.position = found.map(|i| u32::try_from(i + 1).unwrap_or(u32::MAX));
                if let Some(i) = found {
                    cursor = i + 1;
                }
            }
        }
        if let Some((w, m)) = recorded {
            let moved = &self.queued[w].movements[m];
            self.recorded = moved.position.map(|p| (p, moved.uri.clone()));
        }
        self.queued.retain(|w| w.first().is_some());
        self.queue_len = Some(u32::try_from(queue.len()).unwrap_or(u32::MAX));
        Ok(())
    }

    /// Keep `lookahead` works queued beyond the playing position, finishing a
    /// partly-queued work first. Returns how many works it queued.
    fn top_up<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &S,
    ) -> Result<usize, FeedError> {
        self.finish_partial(at)?;
        let playing = self.playing.unwrap_or(0);
        let ahead = self
            .queued
            .iter()
            .filter(|w| w.first().is_some_and(|first| first > playing))
            .count();
        let want = self.lookahead();
        if ahead >= want {
            return Ok(0);
        }
        self.enqueue(at, plan, store, want - ahead)?;
        Ok(want - ahead)
    }

    /// Add the movements an earlier failure left unadded, right after the
    /// ones that made it — ahead of anything queued since — so the work
    /// plays whole and in order. A work already played past is let go.
    fn finish_partial<T: Transport + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
    ) -> Result<(), FeedError> {
        let Some(index) = self.queued.iter().position(|w| !w.is_whole()) else {
            return Ok(());
        };
        let Some(last) = self.queued[index].last() else {
            self.queued.remove(index);
            return Ok(());
        };
        if self.playing.is_some_and(|playing| playing > last) {
            self.queued.remove(index);
            return Ok(());
        }
        let params = render_params(at)?;
        let host = host_of(at)?;
        while let Some(m) = self.queued[index].movements.iter().position(|m| !m.added) {
            let desired = self.queued[index].last().map_or(0, |last| last + 1);
            let (position, len) = add_movement(
                at.transport,
                host,
                &params,
                &self.queued[index].movements[m],
                desired,
            )?;
            // Inserted ahead of something (rather than appended): that
            // something moved down one.
            let before = len.map(|len| len.saturating_sub(1)).or(self.queue_len);
            if before.is_some_and(|before| position <= before) {
                self.shift_from(position);
            }
            let movement = &mut self.queued[index].movements[m];
            movement.position = Some(position);
            movement.added = true;
            self.queue_len = len.or(Some(position));
        }
        Ok(())
    }

    /// Everything the feed knows at or after queue `position` moves down
    /// one: an item was inserted there.
    fn shift_from(&mut self, position: u32) {
        let shift = |p: &mut u32| {
            if *p >= position {
                *p += 1;
            }
        };
        for work in &mut self.queued {
            for m in &mut work.movements {
                if let Some(p) = m.position.as_mut() {
                    shift(p);
                }
            }
        }
        if let Some(p) = self.playing.as_mut() {
            shift(p);
        }
        if let Some((p, _)) = self.recorded.as_mut() {
            shift(p);
        }
    }

    /// Pick and enqueue `count` works, each movement added in order. On a
    /// failure partway, the movements that made it stay tracked (the next
    /// top-up adds the rest) and the error is returned.
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
        let params = render_params(at)?;
        let host = host_of(at)?;
        let mut history = self.history(store, plan.now)?;
        for _ in 0..count {
            let ctx = PickContext {
                history: &history,
                now: Some(plan.now),
                local_hour: plan.local_hour,
                energy_target: None,
                steer: plan.steer,
                feedback: plan.feedback,
            };
            let pick = pick_next(plan.pool, &ctx, &self.config, &mut self.rng)
                .ok_or(FeedError::EmptyPool)?;
            let mut work = QueuedWork {
                title: pick.work.title.clone(),
                composer: pick.work.composer.clone(),
                work_key: pick.work.work_key.clone(),
                movements: pick
                    .movements
                    .iter()
                    .map(|m| Movement {
                        uri: m.track.source_uri.clone(),
                        title: m.track.title.clone(),
                        position: None,
                        added: false,
                    })
                    .collect(),
                reason: pick.reason,
            };
            history.extend(
                work.movements
                    .iter()
                    .map(|m| PlayRecord::at(m.uri.clone(), plan.now)),
            );
            let added = self.append(at.transport, host, &params, &mut work);
            if work.first().is_some() {
                self.queued.push(work);
            }
            added?;
        }
        Ok(())
    }

    /// Append a work's unadded movements to the end of the queue, in order,
    /// recording where each lands.
    fn append<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        host: IpAddr,
        params: &SpotifyRenderParams,
        work: &mut QueuedWork,
    ) -> Result<(), FeedError> {
        for m in work.movements.iter_mut().filter(|m| !m.added) {
            let (position, len) = add_movement(t, host, params, m, 0)?;
            m.position = Some(position);
            m.added = true;
            self.queue_len = len.or(Some(position));
        }
        Ok(())
    }

    /// The zone's recorded plays, then the queued movements not yet reached,
    /// as if just played: the DJ must not pick what is already waiting.
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
            for m in &work.movements {
                if m.position.is_none_or(|p| p > reached) {
                    history.push(PlayRecord::at(m.uri.clone(), now));
                }
            }
        }
        Ok(history)
    }
}

/// The household's Spotify render parameters, learned from its favorites.
fn render_params<T: Transport + ?Sized>(
    at: &Speakers<'_, T>,
) -> Result<SpotifyRenderParams, FeedError> {
    control::spotify_params(at.transport, at.households, at.coordinator)?
        .ok_or(FeedError::NoRenderParams)
}

fn host_of<T: Transport + ?Sized>(at: &Speakers<'_, T>) -> Result<IpAddr, FeedError> {
    Ok(control::locate(at.households, at.coordinator)?.ip)
}

/// Add one movement to the queue at position `desired` (0: the end), the
/// way the Sonos app does (`AddURIToQueue`). Returns where it landed and,
/// when the player says, the queue's new length.
fn add_movement<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    params: &SpotifyRenderParams,
    m: &Movement,
    desired: u32,
) -> Result<(u32, Option<u32>), FeedError> {
    let response = soap::call(
        t,
        host,
        &AV_TRANSPORT,
        "AddURIToQueue",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("EnqueuedURI", &spotify_queue_uri(&m.uri)),
            (
                "EnqueuedURIMetaData",
                &spotify_track_didl(&m.uri, &m.title, params),
            ),
            ("DesiredFirstTrackNumberEnqueued", &desired.to_string()),
            ("EnqueueAsNext", "0"),
        ]),
    )?;
    let len = response
        .get("NewQueueLength")
        .and_then(|n| n.trim().parse().ok());
    Ok((response.require_u32("FirstTrackNumberEnqueued")?, len))
}

#[cfg(test)]
mod tests {
    //! End to end against `fsonos-sim` over real loopback sockets: the DJ's
    //! works go onto a simulated S2 coordinator's queue, and the coordinator's
    //! own GENA AVTransport events — delivered to the feed one at a time, as
    //! the daemon would — drive it. The simulator doesn't advance on its own,
    //! so the tests step through tracks with `next`, as a listener's speaker
    //! would.

    use std::cell::Cell;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use fsonos_core::playback::{EventSource, Playback};
    use fsonos_core::resolve_room;
    use fsonos_core::store::MemStore;
    use fsonos_proto::control::{add_uri_to_queue, remove_all_tracks_from_queue};
    use fsonos_proto::gena::Notify;
    use fsonos_proto::net::{EventSink, Lan};
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
    use fsonos_types::TransportState;

    use super::*;
    use crate::test_shelf::{MIDNIGHT, shelf_items, works_of};

    const ROOM: &str = "Living Room";

    /// The simulator's transport, able to fail the `n`th `AddURIToQueue` from
    /// now on, or to answer that the household has no favorites at all.
    struct Flaky {
        inner: SimLan,
        adds: Cell<usize>,
        fail_add: Cell<Option<usize>>,
        no_favorites: bool,
    }

    impl Flaky {
        fn new(inner: SimLan) -> Self {
            Self {
                inner,
                adds: Cell::new(0),
                fail_add: Cell::new(None),
                no_favorites: false,
            }
        }

        /// Fail the `n`th add from now (1 = the very next).
        fn fail_add_in(&self, n: usize) {
            self.fail_add.set(Some(self.adds.get() + n));
        }
    }

    impl Transport for Flaky {
        fn soap_post(
            &self,
            host: IpAddr,
            control_path: &str,
            soap_action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            if soap_action.ends_with("#AddURIToQueue\"") || soap_action.ends_with("#AddURIToQueue")
            {
                self.adds.set(self.adds.get() + 1);
                if self.fail_add.get() == Some(self.adds.get()) {
                    return Err(ProtoError::Network {
                        target: format!("{host}"),
                        detail: "connection reset (injected)".into(),
                    });
                }
            }
            if self.no_favorites && soap_action.contains("#Browse") && body.contains("FV:2") {
                return Ok(EMPTY_BROWSE.to_owned());
            }
            self.inner.soap_post(host, control_path, soap_action, body)
        }
    }

    /// A ContentDirectory answer with no items: a household with no favorites.
    const EMPTY_BROWSE: &str = concat!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" "#,
        r#"s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>"#,
        r#"<u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">"#,
        r#"<Result>&lt;DIDL-Lite xmlns=&quot;urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/&quot;&gt;&lt;/DIDL-Lite&gt;</Result>"#,
        r#"<NumberReturned>0</NumberReturned><TotalMatches>0</TotalMatches><UpdateID>1</UpdateID>"#,
        r#"</u:BrowseResponse></s:Body></s:Envelope>"#
    );

    /// One simulated S2 player, its household snapshot, and a GENA
    /// subscription to its AVTransport events.
    struct Rig {
        sim: SimHandle,
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
                sim,
                lan,
                houses,
                coordinator,
                _events: events,
                sink,
                sid,
                playback: Playback::default(),
            };
            rig.notifies(); // the initial event
            rig
        }

        fn speakers<'a, T: Transport + ?Sized>(&'a self, t: &'a T) -> Speakers<'a, T> {
            Speakers {
                transport: t,
                households: &self.houses,
                coordinator: &self.coordinator,
            }
        }

        /// This player's AVTransport NOTIFYs, until the line goes quiet.
        fn notifies(&mut self) -> Vec<Notify> {
            let mut got = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let wait = Duration::from_millis(if got.is_empty() { 200 } else { 150 });
                match self.sink.recv_timeout(wait) {
                    Some(n) if n.sid == self.sid => got.push(n),
                    None if !got.is_empty() => break,
                    Some(_) | None => {}
                }
            }
            got
        }

        /// Deliver each NOTIFY to the feed in turn, as the daemon would; the
        /// works queued, or the first error.
        fn pump<T: Transport + ?Sized>(
            &mut self,
            t: &T,
            feed: &mut QueueFeed,
            plan: Planning<'_>,
            store: &mut MemStore,
        ) -> Result<usize, FeedError> {
            let notifies = self.notifies();
            assert!(!notifies.is_empty(), "no AVTransport event arrived");
            let mut queued = 0;
            for n in &notifies {
                self.playback
                    .apply(
                        &self.coordinator,
                        EventSource::AvTransport,
                        n,
                        Instant::now(),
                    )
                    .unwrap();
                let state = self
                    .playback
                    .of(&self.coordinator)
                    .expect("a playback state");
                queued += feed.on_playback(&self.speakers(t), plan, store, state)?;
            }
            Ok(queued)
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

        fn next(&self) {
            control::next(&self.lan, &self.houses, &self.coordinator).unwrap();
        }

        /// Pause and resume: fresh events for the same track.
        fn nudge(&self) {
            control::pause(&self.lan, &self.houses, &self.coordinator).unwrap();
            control::resume(&self.lan, &self.houses, &self.coordinator).unwrap();
        }

        fn host(&self) -> IpAddr {
            control::locate(&self.houses, &self.coordinator).unwrap().ip
        }

        /// The owner adds a track in the Sonos app: at the end, or "Play
        /// Next" (right after the current track).
        fn owner_adds(&self, uri: &str, play_next: bool) {
            let params = self.sim.render_params(2).unwrap();
            add_uri_to_queue(
                &self.lan,
                self.host(),
                &spotify_queue_uri(uri),
                &spotify_track_didl(uri, "Owner", &params),
                play_next,
            )
            .unwrap();
        }

        /// The owner removes the item at a queue position.
        fn owner_removes(&self, position: u32) {
            soap::call(
                &self.lan,
                self.host(),
                &AV_TRANSPORT,
                "RemoveTrackFromQueue",
                &soap::args_xml(&[
                    ("InstanceID", "0"),
                    ("ObjectID", &format!("Q:0/{position}")),
                    ("UpdateID", "0"),
                ]),
            )
            .unwrap();
        }
    }

    fn plan(pool: &WorkPool, now: i64) -> Planning<'_> {
        Planning {
            pool,
            steer: None,
            feedback: None,
            now,
            local_hour: Some(20),
        }
    }

    /// Works of several movements only, so mid-work scenarios always exist.
    fn multi_movement_pool() -> WorkPool {
        let items: Vec<_> = shelf_items(1)
            .into_iter()
            .filter(|i| i.title.contains(": "))
            .collect();
        works_of(&items)
    }

    fn plays(store: &MemStore) -> Vec<String> {
        store
            .recent_plays(None, 500)
            .unwrap()
            .into_iter()
            .map(|p| p.source_uri)
            .collect()
    }

    /// The queue is whole works back to back from `from` (1-based): each
    /// starts with its first movement and holds all of them, in order.
    fn assert_whole_works(rig: &Rig, pool: &WorkPool, from: usize) {
        let queue = rig.queue();
        let mut i = from - 1;
        while i < queue.len() {
            let work = pool.work_of(&queue[i]).expect("a DJ track");
            let whole: Vec<&str> = work
                .movements
                .iter()
                .map(|m| m.track.source_uri.as_str())
                .collect();
            let run: Vec<&str> = queue[i..]
                .iter()
                .take(whole.len())
                .map(String::as_str)
                .collect();
            assert_eq!(run, whole, "work at queue position {}", i + 1);
            i += whole.len();
        }
    }

    #[test]
    fn whole_works_are_queued_and_topped_up_as_they_play() {
        let mut rig = Rig::new();
        let lan = rig.lan.clone();
        let pool = works_of(&shelf_items(1));
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 7);

        let started = feed
            .start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(started.len(), 2, "the first work and one more");
        assert_eq!(started[0].first(), Some(1));
        assert_eq!(started[1].first(), started[0].last().map(|l| l + 1));
        let expected: Vec<String> = started
            .iter()
            .flat_map(|w| w.tracks())
            .map(str::to_owned)
            .collect();
        assert_eq!(rig.queue(), expected, "every movement, in order");
        assert_whole_works(&rig, &pool, 1);
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(rig.position(), 1);
        assert_eq!(feed.current().unwrap().title, started[0].title);

        // A pause and resume: more events for the same track, one play.
        rig.nudge();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(
            plays(&store).len(),
            1,
            "repeated events record a track once"
        );

        let mut topped = 0;
        for step in 1..=30 {
            rig.next();
            topped += rig
                .pump(
                    &lan,
                    &mut feed,
                    plan(&pool, MIDNIGHT + 300 * step),
                    &mut store,
                )
                .unwrap();
            let at = rig.position();
            assert!(
                feed.queued()
                    .iter()
                    .any(|w| w.first().is_some_and(|f| f > at)),
                "a work waits"
            );
            assert_eq!(
                feed.current().unwrap().track_at(at),
                Some(rig.queue()[at as usize - 1].as_str())
            );
        }
        assert!(topped >= 2, "the feed topped the queue up");
        assert_eq!(
            plays(&store),
            rig.queue()[..31].to_vec(),
            "the plays are exactly the tracks heard"
        );
        assert_whole_works(&rig, &pool, 1);
    }

    #[test]
    fn a_failed_top_up_is_retried_and_a_half_queued_work_completed() {
        let mut rig = Rig::new();
        let flaky = Flaky::new(rig.lan.clone());
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 5);
        let started = feed
            .start(&rig.speakers(&flaky), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();

        // Walk to the second (last queued) work; entering it tops up — and
        // the top-up dies on its second movement.
        let second = started[1].first().unwrap();
        while rig.position() + 1 < second {
            rig.next();
            rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
        }
        flaky.fail_add_in(2);
        rig.next();
        let err = rig
            .pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        let partial = feed.queued().last().unwrap().clone();
        assert!(
            !partial.is_whole(),
            "one movement made it, the rest did not"
        );
        assert_eq!(rig.queue().len(), partial.first().unwrap() as usize);

        // The next event for the same track retries: the half work is
        // completed in place, whole and in order.
        rig.nudge();
        rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let completed = feed
            .queued()
            .iter()
            .find(|w| w.work_key == partial.work_key)
            .unwrap();
        assert!(completed.is_whole());
        assert_whole_works(&rig, &pool, 1);

        // And playback carries on through it.
        for _ in 0..completed.movements.len() + 2 {
            rig.next();
            rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
        }
        assert_eq!(plays(&store), rig.queue()[..plays(&store).len()].to_vec());
    }

    #[test]
    fn play_next_from_the_app_is_followed_not_misrecorded() {
        let mut rig = Rig::new();
        let lan = rig.lan.clone();
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 9);
        let started = feed
            .start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert!(started[0].movements.len() > 1);

        // The owner taps "Play Next" in the Sonos app: it lands at position 2,
        // shifting every DJ movement after it.
        let owner = "spotify:track:0OwnerPlayNext00000001";
        let host = control::locate(&rig.houses, &rig.coordinator).unwrap().ip;
        let params = rig.sim.render_params(2).unwrap();
        add_uri_to_queue(
            &lan,
            host,
            &spotify_queue_uri(owner),
            &spotify_track_didl(owner, "Owner", &params),
            true,
        )
        .unwrap();
        rig.next();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(rig.position(), 2);
        assert!(feed.current().is_none(), "the owner's track isn't the DJ's");
        let shifted = feed.queued()[0].clone();
        assert_eq!(
            shifted.movements[1].position,
            Some(3),
            "the work moved down one"
        );

        // Back on the DJ's work, recorded correctly.
        rig.next();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(
            plays(&store),
            [
                shifted.movements[0].uri.clone(),
                shifted.movements[1].uri.clone()
            ]
        );

        // Skipping now lands on the next work's first movement, wherever it
        // moved to — not mid-work.
        let next = feed
            .skip(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .clone();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(next.work_key, started[1].work_key);
        assert_eq!(rig.position(), next.first().unwrap());
        assert_eq!(
            rig.queue()[rig.position() as usize - 1],
            next.movements[0].uri
        );
    }

    #[test]
    fn skip_on_the_owners_item_or_the_last_work_never_replays_a_finished_one() {
        let mut rig = Rig::new();
        let lan = rig.lan.clone();
        let pool = works_of(&shelf_items(1));
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 11);
        let started = feed
            .start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let first = started[0].clone();

        // Two skips before any event: the second finds no queued work after
        // the one just jumped to, so it queues one and plays that.
        let second = feed
            .skip(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .clone();
        assert_eq!(second.work_key, started[1].work_key);
        let third = feed
            .skip(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .clone();
        assert!(
            third.first().unwrap() > second.last().unwrap(),
            "a new work, after the last"
        );
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(rig.position(), third.first().unwrap());
        let seen: HashSet<&str> = [first.work_key.as_str(), second.work_key.as_str()].into();
        assert!(!seen.contains(third.work_key.as_str()));
        assert!(
            feed.queued().iter().all(|w| w.work_key != first.work_key),
            "finished works let go"
        );

        feed.stop(&rig.speakers(&lan)).unwrap();
        rig.notifies();
        let state = control::playback(&rig.lan, &rig.houses, &rig.coordinator)
            .unwrap()
            .transport
            .state;
        assert_eq!(state, TransportState::Stopped);
        assert!(feed.queued().is_empty() && feed.current().is_none());
    }

    const OWNER: &str = "spotify:track:0OwnerPlayNext00000001";
    const OWNER_2: &str = "spotify:track:0OwnerPlayNext00000002";

    #[test]
    fn play_next_then_an_immediate_skip_lands_on_the_next_work() {
        let mut rig = Rig::new();
        let lan = rig.lan.clone();
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 9);
        let started = feed
            .start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert!(started[0].movements.len() > 1);

        // "Play Next" lands right after the current track, and the owner
        // skips before its event arrives: where the next work used to start
        // is now the current work's last movement.
        rig.owner_adds(OWNER, true);
        let next = feed
            .skip(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .clone();
        assert_eq!(next.work_key, started[1].work_key);
        assert_eq!(rig.position(), started[1].first().unwrap() + 1);
        assert_eq!(
            rig.queue()[rig.position() as usize - 1],
            next.movements[0].uri,
            "the next work's first movement, not the current work's last"
        );
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();

        // Another "Play Next", heard only through its event (the same track
        // plays on; the queue grew): the waiting works move down with it.
        let before: Vec<Option<u32>> = feed.queued().iter().map(QueuedWork::first).collect();
        rig.owner_adds(OWNER_2, true);
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let after: Vec<Option<u32>> = feed.queued().iter().map(QueuedWork::first).collect();
        let at = rig.position();
        for (was, now) in before.iter().zip(&after) {
            let shifted = was.map(|p| if p > at { p + 1 } else { p });
            assert_eq!(*now, shifted, "{before:?} -> {after:?}");
        }
        for work in feed.queued() {
            for m in work.movements.iter().filter(|m| m.position.is_some()) {
                assert_eq!(rig.queue()[m.position.unwrap() as usize - 1], m.uri);
            }
        }
    }

    #[test]
    fn a_cleared_or_replaced_queue_lets_the_dj_go() {
        let mut rig = Rig::new();
        let lan = rig.lan.clone();
        let pool = works_of(&shelf_items(1));
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 13);
        feed.start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();

        // The owner replaces the queue with two tracks of their own: shorter
        // than the DJ's, and none of it the DJ's.
        remove_all_tracks_from_queue(&lan, rig.host()).unwrap();
        rig.owner_adds(OWNER, false);
        rig.owner_adds(OWNER_2, false);
        control::play_queue_from(&lan, &rig.houses, &rig.coordinator, 1).unwrap();
        let queued = rig
            .pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(queued, 0);
        assert!(!feed.is_active() && feed.queued().is_empty());
        assert_eq!(rig.queue(), [OWNER, OWNER_2], "nothing added to theirs");

        // Later events change nothing, and a skip says why it can't.
        rig.next();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(rig.queue().len(), 2);
        let err = feed
            .skip(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap_err();
        assert!(matches!(err, FeedError::Inactive), "{err}");
        assert_eq!(rig.queue().len(), 2);

        // Started again, the DJ queues after the owner's tracks.
        let again = feed
            .start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(again[0].first(), Some(3));
        assert!(feed.is_active());
    }

    #[test]
    fn removals_move_the_recorded_play_and_a_removed_work_is_not_sought() {
        let mut rig = Rig::new();
        let lan = rig.lan.clone();
        control::queue_spotify_tracks(&lan, &rig.houses, &rig.coordinator, &[(OWNER, "Owner")])
            .unwrap();
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 17);
        let started = feed
            .start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(started[0].first(), Some(2));
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        rig.next();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!((rig.position(), plays(&store).len()), (3, 2));

        // The owner removes their own item ahead: the playing movement moves
        // from 3 to 2 and is not recorded a second time.
        rig.owner_removes(1);
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(rig.position(), 2);
        assert_eq!(plays(&store).len(), 2, "{:?}", plays(&store));
        assert_eq!(
            feed.current().and_then(|w| w.track_at(2)),
            Some(rig.queue()[1].as_str())
        );
        rig.next();
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        assert_eq!(plays(&store), rig.queue()[..3].to_vec());

        // The owner removes the waiting work outright, and skips before the
        // events arrive: the DJ queues a fresh work rather than seeking to
        // where the removed one was (past the end of the queue by now).
        let waiting = feed.queued().last().unwrap().clone();
        let mut gone: Vec<u32> = waiting
            .movements
            .iter()
            .filter_map(|m| m.position)
            .collect();
        gone.sort_unstable();
        for position in gone.into_iter().rev() {
            rig.owner_removes(position);
        }
        let len = rig.queue().len();
        let next = feed
            .skip(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .clone();
        assert_ne!(next.work_key, waiting.work_key);
        assert_eq!(next.first(), Some(u32::try_from(len).unwrap() + 1));
        assert_eq!(rig.position(), next.first().unwrap());
        assert_eq!(rig.queue()[len], next.movements[0].uri);
        rig.pump(&lan, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
    }

    #[test]
    fn a_half_queued_work_is_completed_in_place_ahead_of_later_items() {
        let mut rig = Rig::new();
        let flaky = Flaky::new(rig.lan.clone());
        let pool = multi_movement_pool();
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 5);
        let started = feed
            .start(&rig.speakers(&flaky), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let second = started[1].first().unwrap();
        while rig.position() + 1 < second {
            rig.next();
            rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
        }
        flaky.fail_add_in(2);
        rig.next();
        rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap_err();
        let partial = feed.queued().last().unwrap().clone();
        assert!(!partial.is_whole());
        let first = partial.first().unwrap() as usize;

        // Before the retry the owner queues a track: it lands right after
        // the half work's first movement. The retry fills the work in ahead
        // of it rather than leaving a fragment.
        rig.owner_adds(OWNER, false);
        rig.nudge();
        rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
            .unwrap();
        let whole = partial.tracks().len();
        let queue = rig.queue();
        assert_eq!(queue[first - 1..first - 1 + whole], partial.tracks()[..]);
        assert_eq!(queue[first - 1 + whole], OWNER, "the owner's track follows");
        let completed = feed
            .queued()
            .iter()
            .find(|w| w.work_key == partial.work_key)
            .unwrap();
        assert!(completed.is_whole());
        for work in feed.queued() {
            for m in &work.movements {
                let at = m.position.unwrap() as usize;
                assert_eq!(queue[at - 1], m.uri, "the feed knows where it all is");
            }
        }

        // Playback runs on through the current work, the completed one whole,
        // then the owner's track (not recorded as the DJ's), then the DJ's
        // next work.
        let owner_at = first + whole;
        while (rig.position() as usize) <= owner_at {
            rig.next();
            rig.pump(&flaky, &mut feed, plan(&pool, MIDNIGHT), &mut store)
                .unwrap();
        }
        let plays = plays(&store);
        let i = plays
            .iter()
            .position(|p| *p == partial.movements[0].uri)
            .unwrap();
        assert_eq!(plays[i..i + whole], partial.tracks()[..]);
        assert_eq!(plays.len(), i + whole + 1, "{plays:?}");
        assert!(!plays.iter().any(|p| p == OWNER));
        assert!(feed.current().is_some(), "on the DJ's next work");
    }

    #[test]
    fn the_owners_queue_is_left_alone_and_dead_ends_are_explained() {
        let rig = Rig::new();
        let lan = rig.lan.clone();
        let owner = "spotify:track:0OwnerQueued0000000001";
        let at =
            control::queue_spotify_tracks(&lan, &rig.houses, &rig.coordinator, &[(owner, "Owner")])
                .unwrap();
        assert_eq!(at, Some(1));
        let pool = works_of(&shelf_items(1));
        let mut store = MemStore::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 3);
        let started = feed
            .start(&rig.speakers(&lan), plan(&pool, MIDNIGHT), &mut store)
            .unwrap()
            .to_vec();
        assert_eq!(
            started[0].first(),
            Some(2),
            "appended after the owner's item"
        );
        assert_eq!(rig.queue()[0], owner);
        assert_eq!(rig.position(), 2, "plays from the DJ's first work");
        let queue_len = rig.queue().len();

        let empty = WorkPool::default();
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 3);
        let err = feed
            .start(&rig.speakers(&lan), plan(&empty, MIDNIGHT), &mut store)
            .unwrap_err();
        assert!(matches!(err, FeedError::EmptyPool), "{err}");

        // A household with no Spotify favorite to learn from.
        let bare = Flaky {
            no_favorites: true,
            ..Flaky::new(lan.clone())
        };
        let mut feed = QueueFeed::new(&rig.coordinator, DjConfig::default(), 3);
        let err = feed
            .start(&rig.speakers(&bare), plan(&pool, MIDNIGHT), &mut store)
            .unwrap_err();
        assert!(matches!(err, FeedError::NoRenderParams), "{err}");
        assert!(err.to_string().contains("Sonos favorites"));
        assert_eq!(rig.queue().len(), queue_len, "dead ends queue nothing");
    }
}
