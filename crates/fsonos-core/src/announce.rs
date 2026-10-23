//! Announcements and chimes that put the music back afterwards.
//!
//! [`Announcer::announce`] plays a clip in the target rooms, then restores
//! every zone it touched as it was (see [`crate::snapshot`]): grouping,
//! source, track and position, volumes, mute, and whether it was playing.
//! Per household:
//!
//! 1. refresh topology after taking the household turn, then snapshot;
//! 2. cap every target room before grouping or unmuting can make it audible;
//! 3. group the targets under one of them and unmute at those levels;
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
//! The daemon follows the clip's accepted GENA events, with bounded SOAP
//! checks when callbacks are missing. Direct callers watch over SOAP.
//! [`clip`] makes and stores the clips; the daemon serves them
//! read-only at `/media/<id>.wav` on its GENA listener. A newer source,
//! pause, or group change prevents restoring an obsolete snapshot; changed
//! room levels are preserved and reported instead of overwritten.
//!
//! When a target room leads a partially announced group, an untargeted room
//! keeps its live transport and queue. The targets rejoin that live group,
//! then coordination returns natively: continued progress, queue edits and
//! paused playback survive without replaying an old queue bookmark.

pub mod clip;
mod continuity;
mod recovery;
pub mod speech;
mod watch;

use recovery::Effects;

use crate::control;
use crate::live::Live;
use crate::snapshot::{self, RestoreReport};
use crate::{ControlTarget, CoreError, HouseholdState, Room};
use fsonos_proto::Transport;
use fsonos_proto::control as soap;
use fsonos_proto::didl::xml_escape;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The level rooms announce at unless asked otherwise (before policy caps).
pub const DEFAULT_VOLUME: u8 = 35;

/// How long past the clip's length to wait for the player to stop.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often the transport state is checked while the clip plays.
const POLL: Duration = Duration::from_millis(250);

/// How long putting the house back keeps trying players that stopped
/// answering (a Wi-Fi drop on a real house) before it reports what it could
/// not put back.
pub const PUT_BACK_PATIENCE: Duration = Duration::from_secs(180);

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
    patience: Duration,
}

impl Default for Announcer {
    fn default() -> Self {
        Self::new()
    }
}

/// The clip and optional live observer shared by one announcement.
#[derive(Clone, Copy)]
struct Playback<'a> {
    clip: &'a Clip,
    live: Option<&'a Live>,
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
        Self::with_timing(POLL, STOP_GRACE).with_patience(PUT_BACK_PATIENCE)
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
            patience: Duration::ZERO,
        }
    }

    /// Keep trying players that stopped answering for up to `patience` while
    /// putting the house back ([`PUT_BACK_PATIENCE`] from [`Self::new`];
    /// none from [`Self::with_timing`]).
    #[must_use]
    pub fn with_patience(mut self, patience: Duration) -> Self {
        self.patience = patience;
        self
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
    /// `cap(room)`), then put everything back. Blocks until done,
    /// including any wait for an earlier announcement in the same household.
    pub fn announce<T: Transport + Sync + ?Sized>(
        &self,
        t: &T,
        targets: &[ControlTarget<'_>],
        clip: &Clip,
        volume: u8,
        cap: &(dyn Fn(&Room) -> u8 + Sync),
    ) -> Result<AnnounceReport, AnnounceError> {
        self.announce_with_live(t, targets, clip, volume, cap, None)
    }

    /// Announce using the daemon's accepted playback events when available.
    /// The observer is captured before installing each household's clip;
    /// missing callbacks fall back to bounded reads. Restoration still checks
    /// current source and grouping before changing the speakers.
    pub fn announce_with_live<T: Transport + Sync + ?Sized>(
        &self,
        t: &T,
        targets: &[ControlTarget<'_>],
        clip: &Clip,
        volume: u8,
        cap: &(dyn Fn(&Room) -> u8 + Sync),
        live: Option<&Live>,
    ) -> Result<AnnounceReport, AnnounceError> {
        let playback = Playback { clip, live };
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
                        self.announce_in(t, household, rooms, &playback, volume, cap)
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
        playback: &Playback<'_>,
        volume: u8,
        cap: &(dyn Fn(&Room) -> u8 + Sync),
    ) -> HouseholdAnnouncement {
        let mut report = HouseholdAnnouncement {
            lead: rooms[0].primary.clone(),
            rooms: rooms.iter().map(|r| r.name.clone()).collect(),
            levels: Vec::new(),
            outcome: Err(String::new()),
            restored: Vec::new(),
            restore_failed: Vec::new(),
        };
        // Targets may have waited behind another announcement or speech
        // synthesis. Keep their identities, never their old group layout.
        let fresh = match refreshed(t, household) {
            Ok(fresh) => fresh,
            Err(e) => {
                report.outcome = Err(format!(
                    "could not refresh topology, so nothing was played: {e}"
                ));
                return report;
            }
        };
        let selected: Option<Vec<&Room>> = rooms
            .iter()
            .map(|old| fresh.rooms.iter().find(|r| r.primary == old.primary))
            .collect();
        let Some(selected) = selected else {
            report.outcome = Err("a target room changed or disappeared; nothing was played".into());
            return report;
        };
        let (household, rooms) = (&fresh, selected.as_slice());
        report.rooms = rooms.iter().map(|r| r.name.clone()).collect();
        report.levels = rooms
            .iter()
            .map(|r| (r.name.clone(), volume.min(cap(r))))
            .collect();
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
        let (outcome, effects) = self.play(t, hs, &plan, rooms, &report.levels, playback);
        report.outcome = outcome;
        let give_up = std::time::Instant::now() + self.patience;
        recovery::put_back(t, household, &plan, &snaps, &effects, give_up, &mut report);
        report
    }

    /// Steps 2–4. Record attempted writes too: a lost reply may have applied.
    fn play<T: Transport + ?Sized>(
        &self,
        t: &T,
        hs: &[HouseholdState],
        plan: &GroupPlan,
        rooms: &[&Room],
        levels: &[(String, u8)],
        playback: &Playback<'_>,
    ) -> (Result<Ending, String>, Effects) {
        let clip = playback.clip;
        let mut effects = Effects::default();
        let mut events = None;
        let prepared: Result<(), CoreError> = (|| {
            // A join can start audio immediately. Bound all levels before
            // regrouping, and never unmute a room at its old, possibly loud level.
            for (room, (_, level)) in rooms.iter().zip(levels) {
                effects.volumes.push((room.primary.clone(), *level));
                control::set_volume(t, hs, &room.primary, *level)?;
            }
            continuity::prepare(t, &hs[0], rooms, &mut effects)?;
            if plan.lead_leaves && !effects.groups.contains(&plan.lead) {
                effects.groups.push(plan.lead.clone());
                effects.sources.push(plan.lead.clone());
                let now = refreshed(t, &hs[0])?;
                control::leave(t, std::slice::from_ref(&now), &plan.lead)?;
            }
            for room in &plan.joins {
                effects.groups.push(room.clone());
                effects.sources.push(room.clone());
                let now = refreshed(t, &hs[0])?;
                control::join(t, std::slice::from_ref(&now), room, &plan.lead)?;
            }
            recovery::wait_for_group(t, &hs[0], plan)?;
            for room in rooms {
                effects.mutes.push(room.primary.clone());
                control::set_mute(t, hs, &room.primary, false)?;
            }
            let host = control::locate(hs, &plan.lead)?.ip;
            effects.before_clip = Some(soap::get_media_info(t, host)?.uri);
            effects.sources.push(plan.lead.clone());
            effects.clip = Some(clip.url.clone());
            events = playback.live.map(Live::subscribe);
            control::play_uri(t, hs, &plan.lead, &clip.url, &clip.didl())?;
            effects.installed = true;
            Ok(())
        })();
        let outcome = prepared.map_err(|e| e.to_string()).and_then(|()| {
            let result = watch::wait(
                t,
                &hs[0],
                plan,
                clip,
                clip.duration + self.stop_grace,
                self.poll,
                events.as_ref(),
            );
            result.map_err(|interrupted| match interrupted {
                watch::Interrupted::LeaveAlone(why) => {
                    effects.preserve = Some(why.clone());
                    why
                }
                // Put back once the players answer again, if still owned.
                watch::Interrupted::Unanswered(why) => why,
            })
        });
        (outcome, effects)
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
