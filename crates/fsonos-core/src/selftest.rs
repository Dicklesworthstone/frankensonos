//! `fsonos selftest`: live acceptance checks against the owner's own
//! players, shipped in the binary so that verifying real hardware needs no
//! compile.
//!
//! The simulator encodes assumptions about how real players behave, and some
//! of them were wrong (a group's coordinator joining itself does not leave
//! the group, on S1 or S2; found live 2026-10-14). These checks exercise the
//! real players the way the controller does. This is the inaudible tier:
//!
//! * [`HOUSE`]: the households and rooms the survey found.
//! * [`READS`]: every read the controller relies on, on every group and room.
//! * [`BROWSE`]: favorites and each group's queue browse.
//! * [`EVENTS`]: on one group's coordinator per household, each event
//!   service the daemon follows subscribes, sends its full-state event,
//!   renews and unsubscribes.
//! * [`INAUDIBLE_WRITES`]: writes that change nothing audible, so the
//!   control path is exercised: `SetVolume` to each room's current level,
//!   `SetPlayMode` to the current mode of each group playing its queue, and
//!   `Pause` on a group that is not playing. They run between two snapshots of everything they could
//!   touch; any difference fails the check, naming it. Nothing is put back
//!   automatically: a difference means a player misbehaved or someone used
//!   the house meanwhile, and only the owner can tell which.
//!
//! The report is scrubbed so the owner can share it: households are `H1`,
//! `H2` and rooms `R1..Rn`; no address, serial, household id, uuid or room name
//! appears in a summary or the evidence.

use crate::HouseholdState;
use crate::doctor::lan::LanProbe;
use crate::doctor::{Check, CheckContext, CheckId, CheckResult, Runner};
use fsonos_proto::content::{self, BrowseFlag, FAVORITES, QUEUE};
use fsonos_proto::net::Lan;
use fsonos_proto::play_mode;
use fsonos_proto::{ProtoError, control as soap};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod events;
pub use events::EVENTS;

/// The file in the data directory that keeps the last full run (`fsonos
/// selftest` writes it, `fsonos doctor` and `GET /health` read it).
pub const LAST_FILE: &str = "selftest-last.json";

/// The last full run kept in `data_dir`: when it ended (Unix seconds) and
/// whether it passed (exit code 0). `None` without a readable one.
#[must_use]
pub fn last_run(data_dir: &std::path::Path) -> Option<(u64, bool)> {
    let text = std::fs::read_to_string(data_dir.join(LAST_FILE)).ok()?;
    let kept: serde_json::Value = serde_json::from_str(&text).ok()?;
    let at = kept["at"].as_u64()?;
    let code = kept["report"]["exit_code"].as_u64()?;
    Some((at, code == 0))
}

pub const HOUSE: CheckId = CheckId("selftest.house");
pub const READS: CheckId = CheckId("selftest.reads");
pub const BROWSE: CheckId = CheckId("selftest.browse");
pub const INAUDIBLE_WRITES: CheckId = CheckId("selftest.inaudible_writes");

/// A read slower than this is a warning: the network or the player is
/// struggling (a congested Wi-Fi channel answered in 0.3-5 s, 2026-10-14).
pub const SLOW_READ: Duration = Duration::from_millis(1500);

/// Register the inaudible tier on `runner`: the households `probe` finds,
/// reached over `lan`.
pub fn register(runner: &mut Runner, lan: Arc<Lan>, probe: &Arc<LanProbe>) {
    let shared = Arc::new(Shared {
        lan,
        probe: Arc::clone(probe),
    });
    runner.register(HouseCheck(Arc::clone(&shared)));
    runner.register(ReadsCheck(Arc::clone(&shared)));
    runner.register(BrowseCheck(Arc::clone(&shared)));
    runner.register(events::EventsCheck(Arc::clone(&shared)));
    runner.register(WritesCheck(shared));
}

struct Shared {
    lan: Arc<Lan>,
    probe: Arc<LanProbe>,
}

impl Shared {
    /// The households, with their scrubbed labels.
    fn house(&self) -> Result<Labeled, Box<CheckResult>> {
        match self.probe.households() {
            Ok(houses) if houses.iter().any(|h| !h.rooms.is_empty()) => {
                Ok(Labeled::new(houses.to_vec()))
            }
            Ok(_) => Err(Box::new(CheckResult::fail(
                "no rooms found",
                "Run `fsonos doctor` to see why no player answered.",
            ))),
            Err(why) => Err(Box::new(CheckResult::fail(
                format!("the survey failed: {}", scrub_ip(why)),
                "Run `fsonos doctor` to see why no player answered.",
            ))),
        }
    }
}

/// The households with stable, scrubbed names: `H1`, `H2` in survey order,
/// and rooms `R1..Rn` by household, then room name.
pub struct Labeled {
    pub houses: Vec<HouseholdState>,
    rooms: HashMap<PlayerId, String>,
}

impl Labeled {
    #[must_use]
    pub fn new(houses: Vec<HouseholdState>) -> Self {
        let mut rooms = HashMap::new();
        let mut n = 0;
        for h in &houses {
            let mut sorted: Vec<_> = h.rooms.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name));
            for room in sorted {
                n += 1;
                for p in &room.players {
                    rooms.insert(p.clone(), format!("R{n}"));
                }
            }
        }
        Self { houses, rooms }
    }

    /// The label of the room `player` belongs to.
    #[must_use]
    pub fn room(&self, player: &PlayerId) -> &str {
        self.rooms.get(player).map_or("R?", String::as_str)
    }

    /// Household `i`'s label.
    #[must_use]
    pub fn house(i: usize) -> String {
        format!("H{}", i + 1)
    }

    fn ip(&self, h: usize, player: &PlayerId) -> Option<IpAddr> {
        self.houses[h].player(player).map(|p| p.ip)
    }
}

/// `text` with every IPv4 address replaced by `<ip>`.
#[must_use]
pub fn scrub_ip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| c.is_ascii_digit()) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let len = tail
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(tail.len());
        let token = tail[..len].trim_end_matches('.');
        if token.parse::<std::net::Ipv4Addr>().is_ok() {
            out.push_str("<ip>");
            rest = &tail[token.len()..];
        } else {
            out.push_str(&tail[..len]);
            rest = &tail[len..];
        }
    }
    out.push_str(rest);
    out
}

/// The calls' outcomes, as the report shows them. A player that refuses a
/// call, or answers nonsense, fails the check; a call lost on the network
/// (a timeout, a refused connection) is the network's problem, a warning.
#[derive(Default)]
struct Tally {
    calls: usize,
    failed: Vec<String>,
    lost: Vec<String>,
    slowest: Duration,
    slow: Vec<String>,
}

impl Tally {
    /// Run `call` for `what`, recording its outcome and time.
    fn time<R>(&mut self, what: &str, call: impl FnOnce() -> Result<R, ProtoError>) -> Option<R> {
        self.calls += 1;
        let started = Instant::now();
        let out = call();
        let took = started.elapsed();
        self.slowest = self.slowest.max(took);
        if took > SLOW_READ {
            self.slow
                .push(format!("{what} took {} ms", took.as_millis()));
        }
        match out {
            Ok(r) => Some(r),
            Err(e) => {
                let line = format!("{what}: {}", scrub_ip(&e.to_string()));
                if matches!(e, ProtoError::Network { .. }) {
                    self.lost.push(line);
                } else {
                    self.failed.push(line);
                }
                None
            }
        }
    }

    fn verdict(self, what: &str, evidence: &serde_json::Value) -> CheckResult {
        let evidence = json!({
            "calls": self.calls,
            "failed": self.failed,
            "lost": self.lost,
            "slow": self.slow,
            "slowest_ms": u64::try_from(self.slowest.as_millis()).unwrap_or(u64::MAX),
            "detail": evidence,
        });
        if !self.failed.is_empty() {
            let mut detail = String::new();
            for f in &self.failed {
                let _ = writeln!(detail, "{f}");
            }
            return CheckResult::fail(
                format!("{} of {} {what} failed", self.failed.len(), self.calls),
                "Run `fsonos doctor`; a player that refuses a read the controller relies on \
                 needs looking at (or report it with this output).",
            )
            .with_detail(detail)
            .with_evidence(evidence);
        }
        if !self.lost.is_empty() {
            return CheckResult::warn(
                format!(
                    "{} of {} {what} were lost on the network; the players answered the rest",
                    self.lost.len(),
                    self.calls
                ),
                "The network path to the speakers is dropping requests (on Wi-Fi, check \
                 the channel's load and the signal). Re-run once it is steady.",
            )
            .with_detail(self.lost.join("\n"))
            .with_evidence(evidence);
        }
        if !self.slow.is_empty() {
            return CheckResult::warn(
                format!(
                    "all {} {what} answered, {} slowly (slowest {} ms)",
                    self.calls,
                    self.slow.len(),
                    self.slowest.as_millis()
                ),
                "Slow answers usually mean a congested or weak network path to the \
                 speakers (on Wi-Fi, check the channel's load).",
            )
            .with_evidence(evidence);
        }
        CheckResult::pass(format!(
            "all {} {what} answered (slowest {} ms)",
            self.calls,
            self.slowest.as_millis()
        ))
        .with_evidence(evidence)
    }
}

struct HouseCheck(Arc<Shared>);

impl Check for HouseCheck {
    fn id(&self) -> CheckId {
        HOUSE
    }
    fn title(&self) -> &'static str {
        "Households and rooms"
    }
    /// The survey reads every player of every household; on a slow
    /// network a dozen players take well over the runner's default.
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(60))
    }
    fn run(&self, _ctx: &CheckContext) -> CheckResult {
        let labeled = match self.0.house() {
            Ok(l) => l,
            Err(failed) => return *failed,
        };
        let houses: Vec<_> = labeled
            .houses
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let generation = h.players.first().map(|p| format!("{:?}", p.generation));
                json!({
                    "household": Labeled::house(i),
                    "generation": generation,
                    "rooms": h.rooms.len(),
                    "players": h.players.len(),
                    "groups": h.groups.len(),
                })
            })
            .collect();
        let summary = houses
            .iter()
            .map(|h| {
                format!(
                    "{} ({}): {} rooms, {} groups",
                    h["household"].as_str().unwrap_or("H?"),
                    h["generation"].as_str().unwrap_or("?"),
                    h["rooms"],
                    h["groups"]
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        CheckResult::pass(summary).with_evidence(json!({ "households": houses }))
    }
}

struct ReadsCheck(Arc<Shared>);

impl Check for ReadsCheck {
    fn id(&self) -> CheckId {
        READS
    }
    fn title(&self) -> &'static str {
        "Every read the controller relies on"
    }
    fn requires(&self) -> &[CheckId] {
        &[HOUSE]
    }
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(120))
    }
    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let labeled = match self.0.house() {
            Ok(l) => l,
            Err(failed) => return *failed,
        };
        let lan = &*self.0.lan;
        let mut tally = Tally::default();
        for (h, house) in labeled.houses.iter().enumerate() {
            for g in &house.groups {
                if ctx.is_cancelled() {
                    break;
                }
                let Some(ip) = labeled.ip(h, &g.coordinator) else {
                    continue;
                };
                let at = format!(
                    "{} {} group",
                    Labeled::house(h),
                    labeled.room(&g.coordinator)
                );
                tally.time(&format!("{at} GetTransportInfo"), || {
                    soap::get_transport_info(lan, ip)
                });
                tally.time(&format!("{at} GetPositionInfo"), || {
                    soap::get_position_info(lan, ip)
                });
                tally.time(&format!("{at} GetMediaInfo"), || {
                    soap::get_media_info(lan, ip)
                });
                tally.time(&format!("{at} GetTransportSettings"), || {
                    play_mode::get_play_mode(lan, ip)
                });
                tally.time(&format!("{at} GetGroupVolume"), || {
                    soap::get_group_volume(lan, ip)
                });
            }
            for room in &house.rooms {
                if ctx.is_cancelled() {
                    break;
                }
                let Some(ip) = labeled.ip(h, &room.primary) else {
                    continue;
                };
                let at = format!("{} {}", Labeled::house(h), labeled.room(&room.primary));
                tally.time(&format!("{at} GetVolume"), || soap::get_volume(lan, ip));
                tally.time(&format!("{at} GetMute"), || soap::get_mute(lan, ip));
                tally.time(&format!("{at} GetBass"), || soap::get_bass(lan, ip));
                tally.time(&format!("{at} GetTreble"), || soap::get_treble(lan, ip));
                tally.time(&format!("{at} GetLoudness"), || soap::get_loudness(lan, ip));
            }
        }
        tally.verdict("reads", &json!(null))
    }
}

struct BrowseCheck(Arc<Shared>);

impl Check for BrowseCheck {
    fn id(&self) -> CheckId {
        BROWSE
    }
    fn title(&self) -> &'static str {
        "Favorites and queues browse"
    }
    fn requires(&self) -> &[CheckId] {
        &[HOUSE]
    }
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(120))
    }
    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let labeled = match self.0.house() {
            Ok(l) => l,
            Err(failed) => return *failed,
        };
        let lan = &*self.0.lan;
        let mut tally = Tally::default();
        let mut counts = Vec::new();
        for (h, house) in labeled.houses.iter().enumerate() {
            // Favorites live on every player of the household; any group's
            // coordinator will do (bridges, which refuse Browse, are not in
            // the list).
            if let Some(ip) = house
                .groups
                .first()
                .and_then(|g| labeled.ip(h, &g.coordinator))
            {
                let at = format!("{} favorites", Labeled::house(h));
                if let Some(page) = tally.time(&at, || {
                    content::browse(lan, ip, FAVORITES, BrowseFlag::DirectChildren, 0, 100)
                }) {
                    counts.push(
                        json!({"household": Labeled::house(h), "favorites": page.total_matches}),
                    );
                }
            }
            for g in &house.groups {
                if ctx.is_cancelled() {
                    break;
                }
                let Some(ip) = labeled.ip(h, &g.coordinator) else {
                    continue;
                };
                let at = format!(
                    "{} {} queue",
                    Labeled::house(h),
                    labeled.room(&g.coordinator)
                );
                if let Some(page) = tally.time(&at, || {
                    content::browse(lan, ip, QUEUE, BrowseFlag::DirectChildren, 0, 20)
                }) {
                    counts.push(json!({
                        "group": labeled.room(&g.coordinator),
                        "queue": page.total_matches,
                    }));
                }
            }
        }
        tally.verdict("browses", &json!(counts))
    }
}

/// Everything an inaudible write could touch, per room and per group.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    rooms: BTreeMap<String, (u8, bool)>,
    groups: BTreeMap<String, (TransportState, PlayMode, Vec<String>)>,
}

impl Snapshot {
    /// Read it, or why a read failed.
    fn take(lan: &Lan, labeled: &Labeled) -> Result<Self, String> {
        let mut rooms = BTreeMap::new();
        let mut groups = BTreeMap::new();
        let fail = |what: &str, e: ProtoError| format!("{what}: {}", scrub_ip(&e.to_string()));
        for (h, house) in labeled.houses.iter().enumerate() {
            for room in &house.rooms {
                let Some(ip) = labeled.ip(h, &room.primary) else {
                    continue;
                };
                let name = labeled.room(&room.primary).to_string();
                let volume = soap::get_volume(lan, ip).map_err(|e| fail(&name, e))?;
                let mute = soap::get_mute(lan, ip).map_err(|e| fail(&name, e))?;
                rooms.insert(name, (volume, mute));
            }
            for g in &house.groups {
                let Some(ip) = labeled.ip(h, &g.coordinator) else {
                    continue;
                };
                let name = labeled.room(&g.coordinator).to_string();
                let state = soap::get_transport_info(lan, ip)
                    .map_err(|e| fail(&name, e))?
                    .state;
                let mode = play_mode::get_play_mode(lan, ip).map_err(|e| fail(&name, e))?;
                let mut members: Vec<String> = g
                    .members
                    .iter()
                    .map(|m| labeled.room(m).to_string())
                    .collect();
                members.sort();
                members.dedup();
                groups.insert(name, (state, mode, members));
            }
        }
        Ok(Self { rooms, groups })
    }

    /// [`Self::take`], tried up to three times a second apart: one read lost
    /// on a busy network (seen live, 2026-10-14) must not fail the run.
    fn take_retrying(lan: &Lan, labeled: &Labeled) -> Result<Self, String> {
        let mut why = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            match Self::take(lan, labeled) {
                Ok(snapshot) => return Ok(snapshot),
                Err(e) => why = e,
            }
        }
        Err(why)
    }

    /// What differs between `self` (before) and `after`, one line each.
    fn diff(&self, after: &Self) -> Vec<String> {
        let mut out = Vec::new();
        for (room, before) in &self.rooms {
            match after.rooms.get(room) {
                Some(now) if now == before => {}
                Some(now) => out.push(format!(
                    "{room}: volume/mute {}/{} became {}/{}",
                    before.0, before.1, now.0, now.1
                )),
                None => out.push(format!("{room}: no longer answers")),
            }
        }
        for (group, before) in &self.groups {
            match after.groups.get(group) {
                Some(now) if now == before => {}
                Some(now) => out.push(format!(
                    "group {group}: {:?} {} {:?} became {:?} {} {:?}",
                    before.0,
                    before.1.as_str(),
                    before.2,
                    now.0,
                    now.1.as_str(),
                    now.2
                )),
                None => out.push(format!("group {group}: gone")),
            }
        }
        out
    }
}

struct WritesCheck(Arc<Shared>);

impl Check for WritesCheck {
    fn id(&self) -> CheckId {
        INAUDIBLE_WRITES
    }
    fn title(&self) -> &'static str {
        "Inaudible writes change nothing"
    }
    fn requires(&self) -> &[CheckId] {
        &[READS]
    }
    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(180))
    }
    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let labeled = match self.0.house() {
            Ok(l) => l,
            Err(failed) => return *failed,
        };
        let lan = &*self.0.lan;
        let before = match Snapshot::take_retrying(lan, &labeled) {
            Ok(s) => s,
            Err(why) => {
                return CheckResult::fail(
                    format!("could not read the house before writing: {why}"),
                    "Nothing was written. Run `fsonos doctor`.",
                );
            }
        };
        let mut tally = Tally::default();
        let (mut paused, mut not_on_queue) = (0, 0);
        for (h, house) in labeled.houses.iter().enumerate() {
            for room in &house.rooms {
                if ctx.is_cancelled() {
                    break;
                }
                let name = labeled.room(&room.primary).to_string();
                let (Some(ip), Some(&(level, _))) =
                    (labeled.ip(h, &room.primary), before.rooms.get(&name))
                else {
                    continue;
                };
                let at = format!(
                    "{} {name} SetVolume({level}) to its own level",
                    Labeled::house(h)
                );
                tally.time(&at, || soap::set_volume(lan, ip, level));
            }
            for g in &house.groups {
                if ctx.is_cancelled() {
                    break;
                }
                let name = labeled.room(&g.coordinator).to_string();
                let (Some(ip), Some((state, mode, _))) =
                    (labeled.ip(h, &g.coordinator), before.groups.get(&name))
                else {
                    continue;
                };
                // Play modes belong to a queue: a group on a stream or line-in
                // may refuse the call, so only a group playing its own queue
                // is asked.
                let on_queue = soap::get_media_info(lan, ip)
                    .is_ok_and(|m| m.uri.starts_with("x-rincon-queue:"));
                if on_queue {
                    let at = format!(
                        "{} group {name} SetPlayMode({}) to its own mode",
                        Labeled::house(h),
                        mode.as_str()
                    );
                    tally.time(&at, || play_mode::set_play_mode(lan, ip, *mode));
                } else {
                    not_on_queue += 1;
                }
                if matches!(state, TransportState::Paused | TransportState::Stopped) {
                    // Refused (UPnP 701) on a group with nothing to pause:
                    // the request still went through the control path.
                    let at = format!("{} group {name} Pause while {state:?}", Labeled::house(h));
                    let outcome = tally.time(&at, || match soap::pause(lan, ip) {
                        Err(ProtoError::SoapFault { code: 701, .. }) | Ok(()) => Ok(()),
                        Err(e) => Err(e),
                    });
                    paused += usize::from(outcome.is_some());
                }
            }
        }
        // Give the players a moment to report any change the writes made.
        std::thread::sleep(Duration::from_millis(500));
        let after = match Snapshot::take_retrying(lan, &labeled) {
            Ok(s) => s,
            Err(why) => {
                return CheckResult::fail(
                    format!("could not read the house after writing: {why}"),
                    "Check the house in the Sonos app: the writes were no-ops by construction.",
                );
            }
        };
        let changed = before.diff(&after);
        if !changed.is_empty() {
            return CheckResult::fail(
                format!(
                    "{} thing(s) changed after writes that should change nothing",
                    changed.len()
                ),
                "Check the house now. Either a player applied a no-op write as a change \
                 (report it with this output), or someone used the house during the test.",
            )
            .with_detail(changed.join("\n"))
            .with_evidence(json!({ "changed": changed }));
        }
        let summary = json!({
            "rooms": before.rooms.len(),
            "groups": before.groups.len(),
            "paused_idle_groups": paused,
            "play_mode_skipped_not_on_queue": not_on_queue,
        });
        tally.verdict("inaudible writes (nothing changed)", &summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_scrubbed_and_other_numbers_kept() {
        assert_eq!(
            scrub_ip("http://192.0.2.10:1400/xml failed: soap fault 402 after 1.5 s"),
            "http://<ip>:1400/xml failed: soap fault 402 after 1.5 s"
        );
        assert_eq!(scrub_ip("198.51.100.7."), "<ip>.");
        assert_eq!(scrub_ip("no address here 1.2.3"), "no address here 1.2.3");
    }

    #[test]
    fn a_snapshot_diff_names_each_change() {
        let mut before = Snapshot {
            rooms: BTreeMap::from([("R1".to_string(), (20, false))]),
            groups: BTreeMap::from([(
                "R1".to_string(),
                (
                    TransportState::Paused,
                    PlayMode::Normal,
                    vec!["R1".to_string()],
                ),
            )]),
        };
        assert!(before.diff(&before.clone()).is_empty());
        let mut after = before.clone();
        after.rooms.insert("R1".into(), (21, false));
        let changed = before.diff(&after);
        assert_eq!(changed, ["R1: volume/mute 20/false became 21/false"]);
        before.rooms.insert("R2".into(), (5, true));
        assert!(
            before
                .diff(&after)
                .iter()
                .any(|c| c == "R2: no longer answers")
        );
    }
}
