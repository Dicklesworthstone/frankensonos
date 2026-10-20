//! The audible tier of `fsonos selftest` (`--audible`): steps that make
//! sound in the rooms the owner names, run with the owner listening. Each
//! step says what it is about to do before it makes a sound, and no step
//! sets a level above the run's cap ([`MAX_VOLUME`] unless the owner raised
//! it); a room that joins music it was not playing is lowered to the cap
//! first. Every zone the rooms play in is snapshotted first ([`SNAPSHOT`])
//! and put back by the last step ([`RESTORE`]), which runs whatever happened
//! before it: a step that failed or timed out, or Ctrl-C, which ends the
//! steps early. It waits for a step still acting, pauses every group the run
//! touched before any level goes back (music whose source cannot come back
//! stays paused, never louder), and undoes a tone or a sleep timer a step
//! left behind; a sleep timer the owner had running is left alone.
//!
//! Each behaviour a step observes is a named fact in its evidence
//! (`group.join_ms`, `handover.method`, `sleep.remaining_secs`, ...), so the
//! simulator's assumptions can be checked against real players. A chime
//! over the playing favorite ([`ANNOUNCE`]) needs a clip server, which the
//! caller supplies ([`Plan::announce`]). Only a
//! radio or single-track favorite is played: an album or playlist would
//! replace the queue, which no snapshot puts back. Not steps yet: Spotify,
//! the DJ, and the queue-revision probes (measured live by hand,
//! 2026-10-17).

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use fsonos_proto::ProtoError;
use fsonos_proto::control as soap;
use fsonos_proto::net::Lan;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_types::{PlayerId, TransportState};
use serde_json::{Value, json};

use super::{Labeled, Shared, Snapshot, scrub_ip};
use crate::control::{self, Tone, ToneChange};
use crate::doctor::lan::LanProbe;
use crate::doctor::{Check, CheckContext, CheckId, CheckResult, Runner};
use crate::fade::{FadeOutcome, Fader, RampOutcome};
use crate::favorites::{self, FavoriteKind};
use crate::snapshot::{self, Aspect, ZoneSnapshot};
use crate::{HouseholdState, inventory, moving, resolve_room};

pub const SNAPSHOT: CheckId = CheckId("selftest.audible.snapshot");
pub const FAVORITE: CheckId = CheckId("selftest.audible.favorite");
pub const ANNOUNCE: CheckId = CheckId("selftest.audible.announce");
pub const VOLUME: CheckId = CheckId("selftest.audible.volume");
pub const FADE: CheckId = CheckId("selftest.audible.fade");
pub const TONE: CheckId = CheckId("selftest.audible.tone");
pub const SLEEP: CheckId = CheckId("selftest.audible.sleep");
pub const GROUP: CheckId = CheckId("selftest.audible.group");
pub const HANDOVER: CheckId = CheckId("selftest.audible.handover");
pub const RESTORE: CheckId = CheckId("selftest.audible.restore");

/// No step sets a level above this unless the owner raises the cap.
pub const MAX_VOLUME: u8 = 20;

/// Plays a chime in the room named, at the level given, and answers once
/// the announcement is over: whether it reported every zone put back.
pub type Announcer = Arc<dyn Fn(&str, u8) -> Result<bool, String> + Send + Sync>;

/// What the owner asked for.
#[derive(Clone)]
pub struct Plan {
    /// The rooms, by name: one, or two (two of one household also group,
    /// and hand the music over).
    pub rooms: Vec<String>,
    /// No step sets a level above this.
    pub max_volume: u8,
    /// The players to survey besides SSDP, and how long to listen: the
    /// steps that regroup plan against a fresh survey.
    pub seeds: Vec<IpAddr>,
    pub wait: Duration,
    /// How to make an announcement, when this run can serve a clip.
    pub announce: Option<Announcer>,
}

/// Register the audible tier on `runner`: `plan`'s rooms, reached over
/// `lan`. `stop` (set by Ctrl-C) ends the steps early, and `say` tells the
/// owner what each is about to do.
pub fn register(
    runner: &mut Runner,
    lan: Arc<Lan>,
    probe: &Arc<LanProbe>,
    plan: Plan,
    stop: Arc<AtomicBool>,
    say: Box<dyn Fn(&str) + Send + Sync>,
) {
    let run = Arc::new(Run {
        shared: Arc::new(Shared {
            lan,
            probe: Arc::clone(probe),
        }),
        plan,
        stop,
        say,
        state: Mutex::default(),
        active: AtomicUsize::new(0),
        told_stop: AtomicBool::new(false),
    });
    let steps: [(CheckId, &'static str, &'static [CheckId], u64, Body); 10] = [
        (
            SNAPSHOT,
            "The rooms are snapshotted",
            &[],
            60,
            snapshot_step,
        ),
        (FAVORITE, "A favorite plays", &[SNAPSHOT], 60, favorite_step),
        (
            ANNOUNCE,
            "A chime plays over the music, which comes back",
            &[FAVORITE],
            90,
            announce_step,
        ),
        (
            VOLUME,
            "Volume is set and reads back",
            &[SNAPSHOT],
            30,
            volume_step,
        ),
        (
            FADE,
            "A fade and a device ramp land",
            &[SNAPSHOT],
            60,
            fade_step,
        ),
        (
            TONE,
            "Tone changes and comes back",
            &[SNAPSHOT],
            30,
            tone_step,
        ),
        (
            SLEEP,
            "A sleep timer is set and cancelled",
            &[SNAPSHOT],
            30,
            sleep_step,
        ),
        (
            GROUP,
            "A room joins a group and leaves",
            &[SNAPSHOT],
            60,
            group_step,
        ),
        (
            HANDOVER,
            "The music moves to another room",
            &[GROUP, FAVORITE],
            60,
            handover_step,
        ),
        (RESTORE, "Every room is put back", &[], 300, restore_step),
    ];
    for (id, title, requires, secs, body) in steps {
        runner.register(Step {
            run: Arc::clone(&run),
            id,
            title,
            requires,
            timeout: Duration::from_secs(secs),
            body,
        });
    }
}

type Body = fn(&Run, &CheckContext) -> CheckResult;

/// What the steps share.
struct Run {
    shared: Arc<Shared>,
    plan: Plan,
    stop: Arc<AtomicBool>,
    say: Box<dyn Fn(&str) + Send + Sync>,
    state: Mutex<State>,
    /// Steps running now (one the runner gave up on may still be acting).
    active: AtomicUsize,
    told_stop: AtomicBool,
}

#[derive(Default)]
struct State {
    /// The house at the start, with its scrubbed labels.
    labeled: Option<Arc<Labeled>>,
    rooms: Vec<Target>,
    snaps: Vec<ZoneSnapshot>,
    before: Option<Snapshot>,
    /// A sleep timer a step set and has not cancelled: its group's
    /// coordinator.
    sleep_set: Option<IpAddr>,
    /// A tone a step changed and has not put back: the room, as it was.
    tone_was: Option<(PlayerId, Tone)>,
}

/// One room of the run, as it was at the start.
#[derive(Debug, Clone)]
struct Target {
    /// Its scrubbed label (`R3`).
    label: String,
    /// Its name, for a fresh survey (never reported).
    name: String,
    household: usize,
    primary: PlayerId,
    coordinator: PlayerId,
}

impl Run {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The house at the start and the rooms, once [`SNAPSHOT`] has run.
    fn start(&self) -> Option<(Arc<Labeled>, Vec<Target>)> {
        let state = self.state();
        Some((Arc::clone(state.labeled.as_ref()?), state.rooms.clone()))
    }

    fn stopped(&self, ctx: &CheckContext) -> bool {
        self.stop.load(Ordering::Relaxed) || ctx.is_cancelled()
    }

    /// Poll `done` every 250 ms for up to `deadline`: how long it took, or
    /// `None` when it never came (or the run stopped).
    fn wait_for(
        &self,
        ctx: &CheckContext,
        deadline: Duration,
        mut done: impl FnMut() -> bool,
    ) -> Option<Duration> {
        let started = Instant::now();
        loop {
            if done() {
                return Some(started.elapsed());
            }
            if self.stopped(ctx) || started.elapsed() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Say once that the run is stopping, and what a second Ctrl-C does.
    fn tell_stop(&self) {
        if !self.told_stop.swap(true, Ordering::Relaxed) {
            (self.say)(
                "stopping: the rooms are put back now (Ctrl-C again quits at once and leaves \
                 them as they are)",
            );
        }
    }

    /// Wait up to `deadline` for every step to have finished.
    fn wait_idle(&self, deadline: Duration) {
        let started = Instant::now();
        while self.active.load(Ordering::Relaxed) > 0 && started.elapsed() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `result` with every room name and player id of the house replaced by
    /// its room's label, as in the rest of the report.
    fn scrubbed(&self, mut result: CheckResult) -> CheckResult {
        let Some(labeled) = self.state().labeled.clone() else {
            return result;
        };
        result.summary = labeled.scrub(&result.summary);
        result.detail = result.detail.map(|d| labeled.scrub(&d));
        result.remedy = result.remedy.map(|r| labeled.scrub(&r));
        result
    }

    /// The house as it is now: the steps that regroup plan against it.
    fn survey(&self) -> Result<Vec<HouseholdState>, String> {
        inventory::survey(&*self.shared.lan, &self.plan.seeds, self.plan.wait)
            .map(|s| s.households)
            .map_err(|e| scrub_ip(&e.to_string()))
    }
}

/// One step: run unless the run was stopped, except [`RESTORE`], which
/// always runs.
struct Step {
    run: Arc<Run>,
    id: CheckId,
    title: &'static str,
    requires: &'static [CheckId],
    timeout: Duration,
    body: Body,
}

impl Check for Step {
    fn id(&self) -> CheckId {
        self.id
    }

    fn title(&self) -> &'static str {
        self.title
    }

    fn requires(&self) -> &[CheckId] {
        self.requires
    }

    fn timeout(&self) -> Option<Duration> {
        Some(self.timeout)
    }

    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let run = &*self.run;
        if self.id == RESTORE {
            if run.stop.load(Ordering::Relaxed) {
                run.tell_stop();
            }
            run.wait_idle(Duration::from_secs(60));
            return run.scrubbed((self.body)(run, ctx));
        }
        if run.stopped(ctx) {
            if run.stop.load(Ordering::Relaxed) {
                run.tell_stop();
            }
            return CheckResult::skip("interrupted: the rooms are put back next");
        }
        run.active.fetch_add(1, Ordering::Relaxed);
        let _done = Done(&run.active);
        run.scrubbed((self.body)(run, ctx))
    }
}

/// Counts a step out of [`Run::active`] when it ends, even by a panic.
struct Done<'a>(&'a AtomicUsize);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A step that could not go on, after `what`.
fn broke(what: &str, e: &impl ToString) -> CheckResult {
    CheckResult::fail(
        format!("{what}: {}", scrub_ip(&e.to_string())),
        "The rooms are put back at the end. Run `fsonos doctor`, and report this output if \
         the players answer otherwise.",
    )
}

fn not_snapshotted() -> CheckResult {
    CheckResult::skip("nothing was snapshotted")
}

fn snapshot_step(run: &Run, _: &CheckContext) -> CheckResult {
    let labeled = match run.shared.house() {
        Ok(labeled) => labeled,
        Err(failed) => return *failed,
    };
    let mut rooms: Vec<Target> = Vec::new();
    for (i, name) in run.plan.rooms.iter().enumerate() {
        let Ok(target) = resolve_room(&labeled.houses, name) else {
            return CheckResult::fail(
                format!("room {} of the run is not one the survey found", i + 1),
                "Give each --room as `fsonos zones` names it.",
            );
        };
        let household = labeled
            .houses
            .iter()
            .position(|h| std::ptr::eq(h, target.household))
            .unwrap_or_default();
        if rooms.iter().any(|r| r.primary == target.room.primary) {
            return CheckResult::fail(
                "the run names the same room twice",
                "Give two different rooms.",
            );
        }
        rooms.push(Target {
            label: labeled.room(&target.room.primary).to_owned(),
            name: target.room.name.clone(),
            household,
            primary: target.room.primary.clone(),
            coordinator: target.coordinator.id.clone(),
        });
    }
    let mut zones: Vec<&PlayerId> = rooms.iter().map(|r| &r.coordinator).collect();
    zones.sort_by(|a, b| a.0.cmp(&b.0));
    zones.dedup();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let mut snaps = Vec::new();
    for zone in zones {
        match snapshot::capture(&*run.shared.lan, &labeled.houses, zone, now) {
            Ok(snap) => snaps.push(snap),
            Err(e) => {
                return CheckResult::fail(
                    format!(
                        "could not snapshot {}'s group: {}",
                        labeled.room(zone),
                        scrub_ip(&e.to_string())
                    ),
                    "Nothing was changed. Run `fsonos doctor`.",
                );
            }
        }
    }
    let before = match Snapshot::take_retrying(&run.shared.lan, &labeled) {
        Ok(before) => before,
        Err(why) => {
            return CheckResult::fail(
                format!("could not read the house: {why}"),
                "Nothing was changed. Run `fsonos doctor`.",
            );
        }
    };
    let labels: Vec<&str> = rooms.iter().map(|r| r.label.as_str()).collect();
    let summary = format!(
        "{} snapshotted with {} group(s): put back at the end",
        labels.join(" and "),
        snaps.len()
    );
    let evidence = json!({
        "rooms": labels,
        "zones": snaps.len(),
        "one_household": rooms.iter().all(|r| r.household == rooms[0].household),
    });
    let mut state = run.state();
    state.labeled = Some(Arc::new(labeled));
    state.rooms = rooms;
    state.snaps = snaps;
    state.before = Some(before);
    CheckResult::pass(summary).with_evidence(evidence)
}

/// Lower every room of the group `coordinator` leads to `cap` where it is
/// louder, before the group plays something it was not.
fn cap_group(
    lan: &Lan,
    houses: &[HouseholdState],
    coordinator: &PlayerId,
    cap: u8,
) -> Result<(), String> {
    let members: Vec<PlayerId> = houses
        .iter()
        .flat_map(|h| &h.groups)
        .filter(|g| g.coordinator == *coordinator)
        .flat_map(|g| g.members.clone())
        .collect();
    for member in std::iter::once(coordinator).chain(&members) {
        let level = control::volume(lan, houses, member).map_err(|e| e.to_string())?;
        if level > cap {
            control::set_volume(lan, houses, member, cap).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn favorite_step(run: &Run, ctx: &CheckContext) -> CheckResult {
    let Some((labeled, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let lan = &*run.shared.lan;
    let room = &rooms[0];
    let found = match favorites::list(lan, &labeled.houses, &room.coordinator) {
        Ok(found) => found,
        Err(e) => return broke("reading the favorites", &e),
    };
    // A radio first; never an album or playlist, which replaces the queue.
    let Some(favorite) = found
        .iter()
        .find(|f| f.kind == FavoriteKind::Stream)
        .or_else(|| found.iter().find(|f| f.kind == FavoriteKind::Track))
    else {
        return CheckResult::skip(
            "the household has no radio or single-track favorite to play (add one in the \
             Sonos app)",
        );
    };
    let cap = run.plan.max_volume;
    (run.say)(&format!(
        "{}'s group: a favorite plays at volume {cap} at most",
        room.label
    ));
    if let Err(e) = cap_group(lan, &labeled.houses, &room.coordinator, cap) {
        return broke("lowering the group to the cap", &e);
    }
    if let Err(e) = favorites::play(lan, &labeled.houses, &room.coordinator, favorite) {
        return broke("playing the favorite", &e);
    }
    let kind = format!("{:?}", favorite.kind).to_lowercase();
    let Some(ip) = labeled.ip(room.household, &room.coordinator) else {
        return CheckResult::fail("the group's coordinator has no address", "Re-run.");
    };
    let playing =
        || soap::get_transport_info(lan, ip).is_ok_and(|i| i.state == TransportState::Playing);
    match run.wait_for(ctx, Duration::from_secs(15), playing) {
        Some(took) => CheckResult::pass(format!(
            "{} plays a {kind} favorite after {} ms",
            room.label,
            took.as_millis()
        ))
        .with_evidence(json!({
            "favorite.kind": kind,
            "favorite.started_ms": ms(took),
        })),
        None => CheckResult::fail(
            format!("{} did not play a {kind} favorite within 15 s", room.label),
            "Play the favorite in the Sonos app: if it plays there, report this output.",
        )
        .with_evidence(json!({ "favorite.kind": kind })),
    }
}

/// What a group plays, how, and how loud `room` is: what an announcement
/// over it must leave as it found.
fn playing(lan: &Lan, coordinator: IpAddr, room: IpAddr) -> Option<(String, TransportState, u8)> {
    let uri = soap::get_media_info(lan, coordinator).ok()?.uri;
    let state = soap::get_transport_info(lan, coordinator).ok()?.state;
    let volume = soap::get_volume(lan, room).ok()?;
    Some((uri, state, volume))
}

fn announce_step(run: &Run, ctx: &CheckContext) -> CheckResult {
    let Some(announce) = run.plan.announce.clone() else {
        return CheckResult::skip("this run cannot serve a chime");
    };
    let Some((labeled, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let lan = &*run.shared.lan;
    let room = &rooms[0];
    let (Some(lead), Some(own)) = (
        labeled.ip(room.household, &room.coordinator),
        labeled.ip(room.household, &room.primary),
    ) else {
        return CheckResult::fail("the room has no address", "Re-run.");
    };
    let Some(before) = playing(lan, lead, own) else {
        return CheckResult::fail(
            "could not read what the group plays before the chime",
            "Re-run.",
        );
    };
    let cap = run.plan.max_volume;
    (run.say)(&format!(
        "{}: a chime at volume {cap} at most, over the music, which then comes back",
        room.label
    ));
    let started = Instant::now();
    let clean = match announce(&room.name, cap) {
        Ok(clean) => clean,
        Err(why) => return broke("announcing", &why),
    };
    let back = run.wait_for(ctx, Duration::from_secs(20), || {
        playing(lan, lead, own).is_some_and(|now| now == before)
    });
    let after = playing(lan, lead, own);
    let evidence = json!({
        "announce.clean": clean,
        "announce.back_ms": back.map(|_| ms(started.elapsed())),
        "announce.source_back": after.as_ref().is_some_and(|a| a.0 == before.0),
        "announce.state_back": after.as_ref().is_some_and(|a| a.1 == before.1),
        "announce.volume_back": after.as_ref().is_some_and(|a| a.2 == before.2),
    });
    if clean && back.is_some() {
        CheckResult::pass(format!(
            "{}: the chime played and the music came back in {} ms",
            room.label,
            ms(started.elapsed())
        ))
        .with_evidence(evidence)
    } else {
        CheckResult::fail(
            format!(
                "{}: after the chime the group is not as it was (the announcement {} itself \
                 clean)",
                room.label,
                if clean { "reported" } else { "did not report" }
            ),
            "Check the room in the Sonos app; the evidence says which of source, state and \
             volume did not come back.",
        )
        .with_evidence(evidence)
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn volume_step(run: &Run, _: &CheckContext) -> CheckResult {
    let Some((labeled, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let lan = &*run.shared.lan;
    let room = &rooms[0];
    let Some(ip) = labeled.ip(room.household, &room.primary) else {
        return CheckResult::fail("the room has no address", "Re-run.");
    };
    let cap = run.plan.max_volume;
    let half = cap / 2;
    (run.say)(&format!("{}: volume {half}, then {cap}", room.label));
    let mut read = Vec::new();
    for level in [half, cap] {
        if let Err(e) = control::set_volume(lan, &labeled.houses, &room.primary, level) {
            return broke("setting the volume", &e);
        }
        match soap::get_volume(lan, ip) {
            Ok(now) => read.push(now),
            Err(e) => return broke("reading the volume back", &e),
        }
    }
    let exact = read == [half, cap];
    let evidence =
        json!({ "volume.set": [half, cap], "volume.read": read, "volume.readback_exact": exact });
    if exact {
        CheckResult::pass(format!("{} read back {half}, then {cap}", room.label))
            .with_evidence(evidence)
    } else {
        CheckResult::fail(
            format!(
                "{} read back {read:?} after setting {half}, then {cap}",
                room.label
            ),
            "A player that keeps another level than it was set to needs looking at: report \
             this output.",
        )
        .with_evidence(evidence)
    }
}

fn fade_step(run: &Run, ctx: &CheckContext) -> CheckResult {
    let Some((labeled, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let lan = &*run.shared.lan;
    let room = &rooms[0];
    let Some(ip) = labeled.ip(room.household, &room.primary) else {
        return CheckResult::fail("the room has no address", "Re-run.");
    };
    let cap = run.plan.max_volume;
    let half = cap / 2;
    (run.say)(&format!(
        "{}: a 5 s fade to {half}, then a ramp at the player's own speed to {cap}",
        room.label
    ));
    let volumes = Fader::default();
    let started = Instant::now();
    let faded = match volumes.fade(
        lan,
        &labeled.houses,
        &room.primary,
        half,
        Duration::from_secs(5),
    ) {
        Ok(outcome) => outcome,
        Err(e) => return broke("fading", &e),
    };
    let fade_ms = ms(started.elapsed());
    let landed = soap::get_volume(lan, ip).ok();
    let ramp = match volumes.ramp_at_device_speed(lan, &labeled.houses, &room.primary, cap) {
        Ok(ramp) => ramp,
        Err(e) => return broke("ramping", &e),
    };
    // RampToVolume answers at once; the player ramps on its own.
    let at_cap = || soap::get_volume(lan, ip).is_ok_and(|v| v == cap);
    let ramped = run.wait_for(ctx, Duration::from_secs(30), at_cap);
    let (ramp_kind, ramp_secs) = match ramp {
        RampOutcome::Device { secs } => ("device", Some(secs)),
        RampOutcome::Stepped(_) => ("stepped", None),
    };
    let evidence = json!({
        "fade.reached": faded == FadeOutcome::Reached,
        "fade.ms": fade_ms,
        "fade.landed": landed,
        "ramp.kind": ramp_kind,
        "ramp.secs": ramp_secs,
        "ramp.reached_ms": ramped.map(ms),
    });
    if faded == FadeOutcome::Reached && landed == Some(half) && ramped.is_some() {
        CheckResult::pass(format!(
            "{} faded to {half} in {fade_ms} ms and ramped ({ramp_kind}) to {cap}",
            room.label
        ))
        .with_evidence(evidence)
    } else {
        CheckResult::fail(
            format!(
                "{}: the fade ended at {landed:?} (wanted {half}), or the ramp to {cap} did not \
                 land within 30 s",
                room.label
            ),
            "Report this output: a fade that misses its level is the controller's to fix.",
        )
        .with_evidence(evidence)
    }
}

fn tone_step(run: &Run, _: &CheckContext) -> CheckResult {
    let Some((labeled, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let lan = &*run.shared.lan;
    let room = &rooms[0];
    let houses = &labeled.houses;
    let was = match control::tone(lan, houses, &room.primary) {
        Ok(tone) => tone,
        Err(e) => return broke("reading the tone", &e),
    };
    // Three steps toward the middle of -10..=10, so each part is a change.
    let nudge = |level: i8| if level > 0 { level - 3 } else { level + 3 };
    let wanted = ToneChange {
        bass: Some(nudge(was.bass)),
        treble: Some(nudge(was.treble)),
        loudness: Some(!was.loudness),
    };
    (run.say)(&format!(
        "{}: bass, treble and loudness change for a moment, then come back",
        room.label
    ));
    run.state().tone_was = Some((room.primary.clone(), was));
    let changed = control::set_tone(lan, houses, &room.primary, wanted)
        .and_then(|()| control::tone(lan, houses, &room.primary));
    let back = control::set_tone(lan, houses, &room.primary, ToneChange::all(was))
        .and_then(|()| control::tone(lan, houses, &room.primary));
    let (changed, back) = match (changed, back) {
        (Ok(changed), Ok(back)) => (changed, back),
        (Err(e), _) | (_, Err(e)) => return broke("changing the tone", &e),
    };
    if back == was {
        run.state().tone_was = None;
    }
    let read_as_set = Some(changed.bass) == wanted.bass
        && Some(changed.treble) == wanted.treble
        && Some(changed.loudness) == wanted.loudness;
    let evidence = json!({ "tone.readback_exact": read_as_set, "tone.restored": back == was });
    if read_as_set && back == was {
        CheckResult::pass(format!("{}'s tone changed and came back", room.label))
            .with_evidence(evidence)
    } else {
        CheckResult::fail(
            format!(
                "{}'s tone did not read back as set, or did not come back",
                room.label
            ),
            "Check the room's EQ in the Sonos app, and report this output.",
        )
        .with_evidence(evidence)
    }
}

fn sleep_step(run: &Run, _: &CheckContext) -> CheckResult {
    let Some((labeled, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let lan = &*run.shared.lan;
    let room = &rooms[0];
    let Some(ip) = labeled.ip(room.household, &room.coordinator) else {
        return CheckResult::fail("the group's coordinator has no address", "Re-run.");
    };
    match soap::get_remaining_sleep_timer(lan, ip) {
        Ok(Some(_)) => {
            return CheckResult::skip("the group has a sleep timer running: left alone")
                .with_evidence(json!({ "sleep.existing": true }));
        }
        Ok(None) => {}
        Err(e) => return broke("reading the sleep timer", &e),
    }
    (run.say)(&format!(
        "{}'s group: a 1-minute sleep timer, cancelled at once",
        room.label
    ));
    run.state().sleep_set = Some(ip);
    let set = soap::configure_sleep_timer(lan, ip, Some(60))
        .and_then(|()| soap::get_remaining_sleep_timer(lan, ip));
    let cancelled = soap::configure_sleep_timer(lan, ip, None)
        .and_then(|()| soap::get_remaining_sleep_timer(lan, ip));
    let (remaining, after) = match (set, cancelled) {
        (Ok(remaining), Ok(after)) => (remaining, after),
        (Err(e), _) | (_, Err(e)) => return broke("the sleep timer", &e),
    };
    if after.is_none() {
        run.state().sleep_set = None;
    }
    let evidence = json!({ "sleep.remaining_secs": remaining, "sleep.cancelled": after.is_none() });
    if remaining.is_some_and(|secs| (50..=60).contains(&secs)) && after.is_none() {
        CheckResult::pass(format!(
            "{}'s group counted down from {}s, then cancelled",
            room.label,
            remaining.unwrap_or_default()
        ))
        .with_evidence(evidence)
    } else {
        CheckResult::fail(
            format!(
                "{}'s group reported {remaining:?} s left, then {after:?} after the cancel",
                room.label
            ),
            "Check the group's sleep timer in the Sonos app, and report this output.",
        )
        .with_evidence(evidence)
    }
}

/// Whether the topology `ip` serves has `member` in the group `coordinator`
/// leads.
fn grouped(lan: &Lan, ip: IpAddr, coordinator: &PlayerId, member: &PlayerId) -> bool {
    get_zone_group_state(lan, ip).is_ok_and(|state| {
        state
            .groups
            .iter()
            .any(|g| g.coordinator == *coordinator && g.members.iter().any(|m| m.uuid == *member))
    })
}

/// The two rooms of one household, or why the step is skipped.
fn pair(rooms: &[Target]) -> Result<(&Target, &Target), Box<CheckResult>> {
    match rooms {
        [lead, member] if lead.household == member.household => Ok((lead, member)),
        [_, _] => Err(Box::new(
            CheckResult::skip(
                "the two rooms are in different households (S1 and S2), which never group",
            )
            .with_evidence(json!({ "group.cross_household": true })),
        )),
        _ => Err(Box::new(CheckResult::skip(
            "give a second --room of the same household to group and hand over",
        ))),
    }
}

fn group_step(run: &Run, ctx: &CheckContext) -> CheckResult {
    let Some((labeled, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let (lead, member) = match pair(&rooms) {
        Ok(pair) => pair,
        Err(skip) => return *skip,
    };
    let lan = &*run.shared.lan;
    let houses = &labeled.houses;
    let Some(ip) = labeled.ip(lead.household, &lead.coordinator) else {
        return CheckResult::fail("the group's coordinator has no address", "Re-run.");
    };
    (run.say)(&format!(
        "{} joins {}'s group, then leaves it",
        member.label, lead.label
    ));
    if let Err(e) = cap_group(lan, houses, &member.primary, run.plan.max_volume) {
        return broke("lowering the joining room to the cap", &e);
    }
    if let Err(e) = control::join(lan, houses, &member.primary, &lead.coordinator) {
        return broke("joining the group", &e);
    }
    let joined = || grouped(lan, ip, &lead.coordinator, &member.primary);
    let Some(join) = run.wait_for(ctx, Duration::from_secs(15), joined) else {
        return CheckResult::fail(
            format!(
                "{} did not show in {}'s group within 15 s",
                member.label, lead.label
            ),
            "Group the two in the Sonos app: if that works, report this output.",
        );
    };
    let now = match run.survey() {
        Ok(now) => now,
        Err(why) => return broke("surveying the house", &why),
    };
    let started = Instant::now();
    if let Err(e) = control::leave(lan, &now, &member.primary) {
        return broke("leaving the group", &e);
    }
    let leave = started.elapsed();
    CheckResult::pass(format!(
        "{} joined {}'s group in {} ms and left in {} ms",
        member.label,
        lead.label,
        join.as_millis(),
        leave.as_millis()
    ))
    .with_evidence(json!({ "group.join_ms": ms(join), "group.leave_ms": ms(leave) }))
}

fn handover_step(run: &Run, ctx: &CheckContext) -> CheckResult {
    let Some((_, rooms)) = run.start() else {
        return not_snapshotted();
    };
    let (lead, member) = match pair(&rooms) {
        Ok(pair) => pair,
        Err(skip) => return *skip,
    };
    let lan = &*run.shared.lan;
    let now = match run.survey() {
        Ok(now) => now,
        Err(why) => return broke("surveying the house", &why),
    };
    let (Ok(from), Ok(to)) = (
        resolve_room(&now, &lead.name),
        resolve_room(&now, &member.name),
    ) else {
        return CheckResult::fail("a room of the run is gone from the survey", "Re-run.");
    };
    let before = match soap::get_position_info(lan, from.coordinator.ip) {
        Ok(before) => before,
        Err(e) => return broke("reading what plays", &e),
    };
    if let Err(e) = cap_group(lan, &now, &to.coordinator.id, run.plan.max_volume) {
        return broke("lowering the receiving room to the cap", &e);
    }
    (run.say)(&format!(
        "the music moves from {} to {}",
        lead.label, member.label
    ));
    let report = match moving::move_playback(lan, &now, &from, &to) {
        Ok(report) => report,
        Err(e) => return broke("moving the music", &e),
    };
    let method = format!("{:?}", report.method).to_lowercase();
    let Some(ip) = now
        .iter()
        .find_map(|h| h.player(&report.new_coordinator))
        .map(|p| p.ip)
    else {
        return CheckResult::fail("the new coordinator is not in the survey", "Re-run.");
    };
    let playing =
        || soap::get_transport_info(lan, ip).is_ok_and(|i| i.state == TransportState::Playing);
    let resumed = run.wait_for(ctx, Duration::from_secs(15), playing);
    let after = soap::get_position_info(lan, ip).ok();
    let carried = after.as_ref().is_some_and(|a| a.uri == before.uri);
    let position_kept = match (&after, before.position_secs) {
        (Some(after), Some(was)) => after.position_secs.map(|now| now >= was),
        _ => None,
    };
    let evidence: Value = json!({
        "handover.method": method,
        "handover.track_carried": carried,
        "handover.position_carried": position_kept,
        "handover.playing_ms": resumed.map(ms),
    });
    if carried && resumed.is_some() {
        CheckResult::pass(format!(
            "the music moved from {} to {} ({method})",
            lead.label, member.label
        ))
        .with_evidence(evidence)
    } else {
        CheckResult::fail(
            format!(
                "after the move ({method}), {} does not play what {} played",
                member.label, lead.label
            ),
            "Move the music in the Sonos app: if that works, report this output.",
        )
        .with_evidence(evidence)
    }
}

/// Put back what a step left behind (a sleep timer it set, a tone it
/// changed), clearing it from the run; what could not be, one line each.
fn undo_leftovers(run: &Run) -> Vec<String> {
    let (start, sleep_set, tone_was) = {
        let mut state = run.state();
        (
            state.labeled.clone(),
            state.sleep_set.take(),
            state.tone_was.take(),
        )
    };
    let lan = &*run.shared.lan;
    let mut failed = Vec::new();
    if let Some(ip) = sleep_set
        && let Err(e) = soap::configure_sleep_timer(lan, ip, None)
    {
        failed.push(format!(
            "the sleep timer the run set: {}",
            scrub_ip(&e.to_string())
        ));
    }
    if let (Some((player, tone)), Some(start)) = (tone_was, &start)
        && let Err(e) = control::set_tone(lan, &start.houses, &player, ToneChange::all(tone))
    {
        failed.push(format!(
            "the tone of {}: {}",
            start.room(&player),
            scrub_ip(&e.to_string())
        ));
    }
    failed
}

/// Pause every group of `labeled` that has one of `touched` in it: the run's
/// music stops before any level goes back, and stays stopped where its
/// source cannot come back. Nothing to pause (UPnP 701) is as quiet.
/// Returns the players of the groups that could not be paused: their
/// levels are not put back, so they stay at the run's (within the cap).
fn quiet_groups(lan: &Lan, labeled: &Labeled, touched: &[&PlayerId]) -> Vec<PlayerId> {
    let mut loud = Vec::new();
    for (h, house) in labeled.houses.iter().enumerate() {
        for g in &house.groups {
            let ours =
                touched.contains(&&g.coordinator) || g.members.iter().any(|m| touched.contains(&m));
            let Some(ip) = labeled.ip(h, &g.coordinator).filter(|_| ours) else {
                continue;
            };
            match soap::pause(lan, ip) {
                Ok(()) | Err(ProtoError::SoapFault { code: 701, .. }) => {}
                Err(_) => {
                    loud.push(g.coordinator.clone());
                    loud.extend(g.members.iter().cloned());
                }
            }
        }
    }
    loud
}

fn restore_step(run: &Run, _: &CheckContext) -> CheckResult {
    let mut failed = undo_leftovers(run);
    let (snaps, before, start, rooms) = {
        let state = run.state();
        (
            state.snaps.clone(),
            state.before.clone(),
            state.labeled.clone(),
            state.rooms.clone(),
        )
    };
    let lan = &*run.shared.lan;
    let (Some(before), Some(start)) = (before.filter(|_| !snaps.is_empty()), start) else {
        return CheckResult::skip("nothing was snapshotted, so nothing was changed");
    };
    (run.say)("putting the rooms back as they were");
    let now = match survey_retrying(run) {
        Ok(now) => now,
        Err(why) => {
            failed.push(format!(
                "a fresh survey failed ({why}): put back by the one at the start"
            ));
            start.houses.clone()
        }
    };
    let labeled = Labeled::new(now);
    let touched: Vec<&PlayerId> = snaps
        .iter()
        .flat_map(|s| &s.members)
        .chain(rooms.iter().map(|r| &r.primary))
        .collect();
    let loud = quiet_groups(lan, &labeled, &touched);
    for snap in &snaps {
        if run.stop.load(Ordering::Relaxed) {
            run.tell_stop();
        }
        let zone = labeled.room(&snap.coordinator).to_owned();
        // Levels only go back under a group that is quiet.
        let unpaused = snap
            .members
            .iter()
            .chain([&snap.coordinator])
            .any(|p| loud.contains(p));
        let aspects: Vec<Aspect> = Aspect::ALL
            .into_iter()
            .filter(|a| !(unpaused && *a == Aspect::Volume))
            .collect();
        if unpaused {
            failed.push(format!(
                "{zone}: could not be paused, so its volume stays at the run's"
            ));
        }
        match snapshot::restore_only(lan, &labeled.houses, snap, &aspects) {
            Ok(report) => failed.extend(
                report
                    .skipped
                    .iter()
                    .map(|(aspect, why)| format!("{zone} {aspect:?}: {}", scrub_ip(why))),
            ),
            Err(e) => failed.push(format!("{zone}: {}", scrub_ip(&e.to_string()))),
        }
    }
    // Let the players settle, then compare against the house at the start.
    std::thread::sleep(Duration::from_secs(1));
    let after = survey_retrying(run)
        .map(Labeled::new)
        .and_then(|labeled| Snapshot::take_retrying(lan, &labeled));
    let changed = match after {
        Ok(after) => quiet(&before).diff(&quiet(&after)),
        Err(why) => vec![format!("could not read the house back: {why}")],
    };
    if failed.is_empty() && changed.is_empty() {
        return CheckResult::pass(format!(
            "{} group(s) put back; the house is as it was",
            snaps.len()
        ));
    }
    // Restore's own reasons name players by id: label them.
    let failed: Vec<String> = failed.iter().map(|line| start.scrub(line)).collect();
    let mut detail = failed.clone();
    detail.extend(changed.iter().cloned());
    CheckResult::fail(
        "the rooms are not all as they were",
        "Check them in the Sonos app; each line says what differs.",
    )
    .with_detail(detail.join("\n"))
    .with_evidence(json!({ "not_restored": failed, "changed": changed }))
}

/// [`Run::survey`], tried three times a second apart: the put-back must not
/// rest on one lost read.
fn survey_retrying(run: &Run) -> Result<Vec<HouseholdState>, String> {
    let mut why = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        match run.survey() {
            Ok(houses) if !houses.is_empty() => return Ok(houses),
            Ok(_) => "no household answered".clone_into(&mut why),
            Err(e) => why = e,
        }
    }
    Err(why)
}

/// `snapshot` with paused groups counted as stopped: a source put back
/// leaves its group stopped, which is as quiet as the pause it was in.
fn quiet(snapshot: &Snapshot) -> Snapshot {
    let mut quiet = snapshot.clone();
    for (state, _, _) in quiet.groups.values_mut() {
        if *state == TransportState::Paused {
            *state = TransportState::Stopped;
        }
    }
    quiet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(household: usize, name: &str) -> Target {
        Target {
            label: name.to_owned(),
            name: name.to_owned(),
            household,
            primary: PlayerId(format!("RINCON_{name}")),
            coordinator: PlayerId(format!("RINCON_{name}")),
        }
    }

    #[test]
    fn only_two_rooms_of_one_household_group() {
        assert!(pair(&[target(0, "R1"), target(0, "R2")]).is_ok());
        let across = pair(&[target(0, "R1"), target(1, "R3")]).unwrap_err();
        assert_eq!(across.evidence["group.cross_household"], true);
        assert!(pair(&[target(0, "R1")]).is_err());
    }
}
