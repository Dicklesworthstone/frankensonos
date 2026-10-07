//! Announcements and chimes that put the music back afterwards.
//!
//! [`Announcer::announce`] plays a clip in the target rooms, then restores
//! every zone it touched as it was (see [`crate::snapshot`]): grouping,
//! source, track and position, volumes, mute, and whether it was playing.
//! Per household:
//!
//! 1. snapshot each zone a target room plays in;
//! 2. group the target rooms under one of them, so the clip plays in sync;
//! 3. set each room to the announcement level, capped per room by the
//!    caller (house policy), and unmute it;
//! 4. play the clip, and wait for the player to stop by itself, at most the
//!    clip's length plus [`STOP_GRACE`];
//! 5. take the rooms out of the temporary group and restore the snapshots,
//!    even when a step above failed. A zone whose coordinator was not
//!    announced to played on throughout: it gets back only its grouping and
//!    levels, so its music is not rewound.
//!
//! Households announce in parallel (S1 and S2 can never group). Each
//! household takes one announcement at a time; later ones wait their turn,
//! in order.
//!
//! The players learn the clip's end from their transport state, polled over
//! SOAP. [`clip`] makes and stores the clips; the daemon serves them
//! read-only at `/media/<id>.wav` on its GENA listener.
//!
//! Known limit: when a target room leads a group whose other rooms are not
//! announced to, those rooms play on during the announcement and are put
//! back to the snapshot (rewound) afterwards.

pub mod clip;

use crate::control;
use crate::snapshot::{self, Aspect, RestoreReport, ZoneSnapshot};
use crate::{ControlTarget, CoreError, HouseholdState, Room};
use fsonos_proto::Transport;
use fsonos_proto::control as soap;
use fsonos_proto::didl::xml_escape;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::{PlayerId, TransportState};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The level rooms announce at unless asked otherwise (before policy caps).
pub const DEFAULT_VOLUME: u8 = 35;

/// How long past the clip's length to wait for the player to stop.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// A player still STOPPED this long after Play never started the clip.
const START_GRACE: Duration = Duration::from_secs(2);

/// How often the transport state is checked while the clip plays.
const POLL: Duration = Duration::from_millis(250);

/// A clip the players can fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub url: String,
    pub title: String,
    pub duration: Duration,
}

impl Clip {
    /// DIDL-Lite for `SetAVTransportURI`: the title, a WAV resource, and its
    /// length.
    #[must_use]
    pub fn didl(&self) -> String {
        let ms = self.duration.as_millis();
        let duration = format!(
            "{}:{:02}:{:02}.{:03}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60,
            ms % 1000
        );
        format!(
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
             xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\
             <item id=\"announcement\" parentID=\"-1\" restricted=\"true\">\
             <dc:title>{}</dc:title><upnp:class>object.item.audioItem</upnp:class>\
             <res protocolInfo=\"http-get:*:audio/wav:*\" duration=\"{duration}\">{}</res>\
             </item></DIDL-Lite>",
            xml_escape(&self.title),
            xml_escape(&self.url),
        )
    }
}

/// How the clip's playback ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The player played it and stopped by itself.
    Finished,
    /// Still going at the deadline; it was stopped.
    Deadline,
    /// The player never started it (e.g. it could not fetch the clip).
    NeverStarted,
}

/// How one household's target rooms are grouped for an announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPlan {
    /// The room (primary) that plays the clip for all of them.
    pub lead: PlayerId,
    /// The lead first leaves its group, which has rooms not announced to.
    pub lead_leaves: bool,
    /// Rooms (primaries) that join the lead.
    pub joins: Vec<PlayerId>,
    /// The zones (coordinators) the announcement changes: to snapshot.
    pub zones: Vec<PlayerId>,
}

/// Plan the temporary group for `rooms` (one household's targets). The lead
/// is, in order of preference: a room leading a zone that is announced to
/// whole (nothing to take apart); a room that is only a member of its group
/// (leaving it disturbs nobody); else the first room.
#[must_use]
pub fn plan_group(household: &HouseholdState, rooms: &[&Room]) -> Option<GroupPlan> {
    let targeted = |r: &Room| rooms.iter().any(|t| t.primary == r.primary);
    let leads_zone = |r: &Room| r.players.contains(&r.coordinator);
    let whole_zone = |c: &PlayerId| {
        household
            .rooms
            .iter()
            .filter(|r| r.coordinator == *c)
            .all(targeted)
    };
    let (lead, lead_leaves) = match rooms
        .iter()
        .find(|r| leads_zone(r) && whole_zone(&r.coordinator))
    {
        Some(r) => (*r, false),
        None => (
            *rooms
                .iter()
                .find(|r| !leads_zone(r))
                .or_else(|| rooms.first())?,
            true,
        ),
    };
    let joins = rooms
        .iter()
        .filter(|r| r.primary != lead.primary)
        .filter(|r| lead_leaves || r.coordinator != lead.coordinator)
        .map(|r| r.primary.clone())
        .collect();
    let mut zones: Vec<PlayerId> = Vec::new();
    for r in rooms {
        if !zones.contains(&r.coordinator) {
            zones.push(r.coordinator.clone());
        }
    }
    Some(GroupPlan {
        lead: lead.primary.clone(),
        lead_leaves,
        joins,
        zones,
    })
}

/// What an announcement did in one household.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HouseholdAnnouncement {
    /// The room that played the clip.
    pub lead: PlayerId,
    pub rooms: Vec<String>,
    /// The level each room announced at, after its cap.
    pub levels: Vec<(String, u8)>,
    /// How the clip ended, or why the announcement failed. The snapshots
    /// were restored either way, unless taking them failed.
    pub outcome: Result<Ending, String>,
    /// Each zone's restore, by coordinator.
    pub restored: Vec<(PlayerId, RestoreReport)>,
    /// Zones (or rooms leaving the temporary group) that could not be put
    /// back, and why.
    pub restore_failed: Vec<(PlayerId, String)>,
}

impl HouseholdAnnouncement {
    /// Played to the end, and everything put back as it was.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.outcome == Ok(Ending::Finished)
            && self.restore_failed.is_empty()
            && self.restored.iter().all(|(_, r)| r.skipped.is_empty())
    }
}

/// What an announcement did, per household.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceReport {
    pub households: Vec<HouseholdAnnouncement>,
}

#[derive(Debug, thiserror::Error)]
pub enum AnnounceError {
    #[error("no rooms to announce to")]
    NoTargets,
}

/// Turns per household: tickets handed out and the one being served.
#[derive(Debug, Default)]
struct Turns {
    next: u64,
    serving: u64,
}

/// Plays announcements, one per household at a time.
#[derive(Debug)]
pub struct Announcer {
    turns: Mutex<HashMap<String, Turns>>,
    turn_over: Condvar,
    poll: Duration,
    stop_grace: Duration,
}

impl Default for Announcer {
    fn default() -> Self {
        Self::new()
    }
}

/// A household's turn; the next waiting announcement goes when it drops.
struct Turn<'a> {
    announcer: &'a Announcer,
    key: String,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut turns = self
            .announcer
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = turns.get_mut(&self.key) {
            t.serving += 1;
            if t.serving == t.next {
                turns.remove(&self.key);
            }
        }
        self.announcer.turn_over.notify_all();
    }
}

impl Announcer {
    #[must_use]
    pub fn new() -> Self {
        Self::with_timing(POLL, STOP_GRACE)
    }

    /// Check the transport state every `poll` while a clip plays, and give
    /// up waiting `stop_grace` after its length.
    #[must_use]
    pub fn with_timing(poll: Duration, stop_grace: Duration) -> Self {
        Self {
            turns: Mutex::new(HashMap::new()),
            turn_over: Condvar::new(),
            poll,
            stop_grace,
        }
    }

    /// How many announcements are playing or waiting, over all households.
    #[must_use]
    pub fn pending(&self) -> u64 {
        let turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        turns.values().map(|t| t.next - t.serving).sum()
    }

    /// Wait for `key`'s turn.
    fn turn(&self, key: String) -> Turn<'_> {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = turns.entry(key.clone()).or_default();
        let ticket = entry.next;
        entry.next += 1;
        while turns.get(&key).is_some_and(|t| t.serving != ticket) {
            turns = self
                .turn_over
                .wait(turns)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Turn {
            announcer: self,
            key,
        }
    }

    /// Play `clip` in the `targets` rooms at `volume` (each room capped at
    /// `cap(room name)`), then put everything back. Blocks until done,
    /// including any wait for an earlier announcement in the same household.
    pub fn announce<T: Transport + Sync + ?Sized>(
        &self,
        t: &T,
        targets: &[ControlTarget<'_>],
        clip: &Clip,
        volume: u8,
        cap: &(dyn Fn(&str) -> u8 + Sync),
    ) -> Result<AnnounceReport, AnnounceError> {
        let mut by_household: Vec<(&HouseholdState, Vec<&Room>)> = Vec::new();
        for target in targets {
            match by_household
                .iter_mut()
                .find(|(h, _)| std::ptr::eq(*h, target.household))
            {
                Some((_, rooms)) => {
                    if !rooms.iter().any(|r| r.primary == target.room.primary) {
                        rooms.push(target.room);
                    }
                }
                None => by_household.push((target.household, vec![target.room])),
            }
        }
        if by_household.is_empty() {
            return Err(AnnounceError::NoTargets);
        }
        let households = std::thread::scope(|s| {
            let running: Vec<_> = by_household
                .iter()
                .map(|(household, rooms)| {
                    s.spawn(move || {
                        let _turn = self.turn(turn_key(household));
                        self.announce_in(t, household, rooms, clip, volume, cap)
                    })
                })
                .collect();
            running
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
                .collect()
        });
        Ok(AnnounceReport { households })
    }

    /// One household's announcement, start to finish.
    fn announce_in<T: Transport + ?Sized>(
        &self,
        t: &T,
        household: &HouseholdState,
        rooms: &[&Room],
        clip: &Clip,
        volume: u8,
        cap: &(dyn Fn(&str) -> u8 + Sync),
    ) -> HouseholdAnnouncement {
        let mut report = HouseholdAnnouncement {
            lead: rooms[0].primary.clone(),
            rooms: rooms.iter().map(|r| r.name.clone()).collect(),
            levels: rooms
                .iter()
                .map(|r| (r.name.clone(), volume.min(cap(&r.name))))
                .collect(),
            outcome: Err(String::new()),
            restored: Vec::new(),
            restore_failed: Vec::new(),
        };
        let Some(plan) = plan_group(household, rooms) else {
            report.outcome = Err("no rooms to announce to".into());
            return report;
        };
        report.lead.clone_from(&plan.lead);
        let hs = std::slice::from_ref(household);
        let mut snaps = Vec::new();
        for zone in &plan.zones {
            match snapshot::capture(t, hs, zone, unix_now()) {
                Ok(snap) => snaps.push(snap),
                Err(e) => {
                    report.outcome = Err(format!(
                        "could not snapshot {}'s zone, so nothing was played: {e}",
                        room_name(household, zone)
                    ));
                    return report;
                }
            }
        }
        report.outcome = self
            .play(t, hs, &plan, rooms, &report.levels, clip)
            .map_err(|e| e.to_string());
        put_back(t, household, rooms, &plan, &snaps, &mut report);
        report
    }

    /// Steps 2–4: group, set levels, play, and wait for the end.
    fn play<T: Transport + ?Sized>(
        &self,
        t: &T,
        hs: &[HouseholdState],
        plan: &GroupPlan,
        rooms: &[&Room],
        levels: &[(String, u8)],
        clip: &Clip,
    ) -> Result<Ending, CoreError> {
        if plan.lead_leaves {
            control::leave(t, hs, &plan.lead)?;
        }
        for room in &plan.joins {
            control::join(t, hs, room, &plan.lead)?;
        }
        for (room, (_, level)) in rooms.iter().zip(levels) {
            control::set_mute(t, hs, &room.primary, false)?;
            control::set_volume(t, hs, &room.primary, *level)?;
        }
        control::play_uri(t, hs, &plan.lead, &clip.url, &clip.didl())?;
        let host = control::locate(hs, &plan.lead)?.ip;
        let ending = wait_for_end(t, host, clip.duration + self.stop_grace, self.poll)?;
        if ending == Ending::Deadline {
            soap::stop(t, host)?;
        }
        Ok(ending)
    }
}

/// Wait until the player at `host` has played the clip and stopped, or
/// `deadline` has passed.
fn wait_for_end<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    deadline: Duration,
    poll: Duration,
) -> Result<Ending, CoreError> {
    let started = Instant::now();
    let mut began = false;
    loop {
        match soap::get_transport_info(t, host)?.state {
            TransportState::Playing | TransportState::Transitioning => began = true,
            // Stopped (or paused by someone) after playing: done.
            TransportState::Stopped | TransportState::Paused if began => {
                return Ok(Ending::Finished);
            }
            TransportState::Stopped if started.elapsed() >= START_GRACE => {
                return Ok(Ending::NeverStarted);
            }
            _ => {}
        }
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(Ending::Deadline);
        }
        std::thread::sleep(poll.min(left));
    }
}

/// Step 5: rooms leave the temporary group, then each zone is restored
/// against the topology as it is by then.
fn put_back<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    rooms: &[&Room],
    plan: &GroupPlan,
    snaps: &[ZoneSnapshot],
    report: &mut HouseholdAnnouncement,
) {
    let hs = std::slice::from_ref(household);
    for room in &plan.joins {
        if let Err(e) = control::leave(t, hs, room) {
            report.restore_failed.push((room.clone(), e.to_string()));
        }
    }
    for snap in snaps {
        let announced_to = rooms.iter().any(|r| r.players.contains(&snap.coordinator));
        let aspects: &[Aspect] = if announced_to {
            &Aspect::ALL
        } else {
            &[Aspect::Group, Aspect::Volume, Aspect::Mute]
        };
        let result = refreshed(t, household)
            .and_then(|now| snapshot::restore_only(t, std::slice::from_ref(&now), snap, aspects));
        match result {
            Ok(r) => report.restored.push((snap.coordinator.clone(), r)),
            Err(e) => report
                .restore_failed
                .push((snap.coordinator.clone(), e.to_string())),
        }
    }
}

/// `household` with its topology read afresh from the first player that
/// answers.
fn refreshed<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
) -> Result<HouseholdState, CoreError> {
    let mut last = CoreError::UnknownHousehold("a household with no players".into());
    for player in &household.players {
        match get_zone_group_state(t, player.ip) {
            Ok(zgs) => {
                let mut now = household.clone();
                now.apply_topology(&zgs);
                return Ok(now);
            }
            Err(e) => last = e.into(),
        }
    }
    Err(last)
}

/// Announcements queue per household id (or, without one, per first player).
fn turn_key(household: &HouseholdState) -> String {
    household.id.as_ref().map_or_else(
        || {
            household
                .players
                .first()
                .map_or_else(String::new, |p| p.id.0.clone())
        },
        |id| id.0.clone(),
    )
}

fn room_name(household: &HouseholdState, player: &PlayerId) -> String {
    household
        .rooms
        .iter()
        .find(|r| r.players.contains(player))
        .map_or_else(|| player.0.clone(), |r| r.name.clone())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::ZoneGroup;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn pid(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    /// Rooms named after their primaries, grouped as `groups` says
    /// (coordinator first).
    fn household(groups: &[&[&str]]) -> HouseholdState {
        let mut h = HouseholdState::default();
        for g in groups {
            h.groups.push(ZoneGroup {
                coordinator: pid(g[0]),
                members: g.iter().map(|m| pid(m)).collect(),
            });
            for m in *g {
                h.rooms.push(Room {
                    name: (*m).into(),
                    primary: pid(m),
                    players: vec![pid(m)],
                    missing: Vec::new(),
                    coordinator: pid(g[0]),
                });
            }
        }
        h
    }

    fn plan(h: &HouseholdState, names: &[&str]) -> GroupPlan {
        let rooms: Vec<&Room> = names
            .iter()
            .map(|n| h.rooms.iter().find(|r| r.name == *n).unwrap())
            .collect();
        plan_group(h, &rooms).unwrap()
    }

    fn ids(names: &[&str]) -> Vec<PlayerId> {
        names.iter().map(|n| pid(n)).collect()
    }

    #[test]
    fn the_lead_is_the_least_disruptive_room() {
        let h = household(&[&["A", "B"], &["C"], &["D", "E"]]);
        // A whole zone announced to: its coordinator leads in place.
        let p = plan(&h, &["C", "A", "B"]);
        assert_eq!((p.lead, p.lead_leaves), (pid("C"), false));
        assert_eq!(p.joins, ids(&["A", "B"]));
        assert_eq!(p.zones, ids(&["C", "A"]));
        let p = plan(&h, &["A", "B", "E"]);
        assert_eq!((p.lead.clone(), p.lead_leaves), (pid("A"), false));
        assert_eq!(p.joins, ids(&["E"]), "B is already with A");
        assert_eq!(p.zones, ids(&["A", "D"]));
        let p = plan(&h, &["E", "D"]);
        assert_eq!((p.lead, p.lead_leaves), (pid("D"), false));
        assert!(p.joins.is_empty());
        // Only part of a zone: a member leads, so its group plays on.
        let p = plan(&h, &["A", "E"]);
        assert_eq!((p.lead, p.lead_leaves), (pid("E"), true));
        assert_eq!(p.joins, ids(&["A"]));
        // Nothing better: the first room leaves its group.
        let p = plan(&h, &["A"]);
        assert_eq!((p.lead, p.lead_leaves), (pid("A"), true));
        assert!(p.joins.is_empty());
        assert_eq!(plan_group(&h, &[]), None);
    }

    #[test]
    fn clip_metadata_carries_the_title_and_length() {
        let clip = Clip {
            url: "http://192.0.2.9:3400/media/ab.wav?x=1&y=2".into(),
            title: "Dinner <now> & then".into(),
            duration: Duration::from_millis(3_723_456),
        };
        let didl = clip.didl();
        assert!(didl.contains("duration=\"1:02:03.456\""), "{didl}");
        assert!(didl.contains("Dinner &lt;now&gt; &amp; then"), "{didl}");
        assert!(didl.contains(">http://192.0.2.9:3400/media/ab.wav?x=1&amp;y=2</res>"));
        let parsed = fsonos_proto::didl::parse_didl(&didl).unwrap();
        let res = parsed[0].res.as_ref().unwrap();
        assert_eq!(res.duration_secs(), Some(3723));
        assert_eq!(parsed[0].title, "Dinner <now> & then");
    }

    #[test]
    fn turns_are_taken_in_order_per_household() {
        let announcer = Arc::new(Announcer::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = announcer.turn("H1".into());
        assert_eq!(announcer.pending(), 1);
        let started = Arc::new(AtomicUsize::new(0));
        let waiters: Vec<_> = (0..3u64)
            .map(|i| {
                let (a, o, s) = (announcer.clone(), order.clone(), started.clone());
                let w = std::thread::spawn(move || {
                    s.fetch_add(1, Ordering::SeqCst);
                    let _turn = a.turn("H1".into());
                    o.lock().unwrap().push(i);
                });
                // Each takes its ticket before the next one starts.
                while announcer.pending() < i + 2 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                w
            })
            .collect();
        // Another household is not held up.
        drop(announcer.turn("H2".into()));
        assert!(order.lock().unwrap().is_empty());
        drop(first);
        for w in waiters {
            w.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
        assert_eq!(started.load(Ordering::SeqCst), 3);
        assert_eq!(announcer.pending(), 0);
    }
}
