//! The LAN checks: can this host open its store, find the players, read them,
//! and hear their events?
//!
//! Discovery and eventing failures are the most common and most confusing
//! problems with local Sonos control: they look like "nothing happens". Each
//! check here names the failure and the fix. All of them are read-only; the
//! GENA round trip cancels its one subscription even when it times out.
//!
//! The checks share one [`LanProbe`] (a real [`Lan`] transport, so the daemon
//! can run them too), which discovers once and reads each player once. Every
//! verdict comes from a pure function of what was observed, so the mapping is
//! unit-tested without a network.

use super::{Check, CheckContext, CheckId, CheckResult, Runner};
use crate::HouseholdState;
use crate::inventory::{self, Survey, classify};
use crate::store::SqliteStore;
use fsonos_proto::description::{DeviceDescription, parse_device_description};
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::soap::RENDERING_CONTROL;
use fsonos_proto::ssdp::{Advert, description_url};
use fsonos_proto::topology::host_of_location;
use fsonos_proto::{Transport, control as soap};
use fsonos_types::{Generation, PlayerId};
use serde_json::json;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const STORE_OPEN: CheckId = CheckId("store.open");
pub const SSDP: CheckId = CheckId("lan.ssdp");
pub const SEEDS: CheckId = CheckId("lan.seeds");
pub const PLAYERS: CheckId = CheckId("lan.players");
pub const HOUSEHOLDS: CheckId = CheckId("lan.households");
pub const GENA_ROUNDTRIP: CheckId = CheckId("lan.gena_roundtrip");
pub const TOPOLOGY: CheckId = CheckId("lan.topology");

/// A player read slower than this is a warning.
pub const SLOW_PLAYER: Duration = Duration::from_millis(500);
/// How long the GENA round trip waits for the initial NOTIFY.
pub const NOTIFY_WAIT: Duration = Duration::from_secs(5);

const LOCAL_NETWORK_REMEDY: &str = "On macOS, allow Local Network access: System Settings > \
     Privacy & Security > Local Network > enable fsonos (or your terminal, or the launchd job's \
     program).";
const MULTICAST_REMEDY: &str = "Mesh Wi-Fi and some routers filter multicast: check IGMP \
     snooping / multicast settings, or list the players' addresses in a seeds file \
     (FSONOS_SEEDS).";

/// What the LAN looks like from here, discovered once and shared by the
/// checks.
pub struct LanProbe {
    lan: Arc<Lan>,
    seeds: Vec<IpAddr>,
    ssdp_wait: Duration,
    adverts: OnceLock<Result<Vec<Advert>, String>>,
    reads: Mutex<BTreeMap<IpAddr, Read>>,
    survey: OnceLock<Result<Survey, String>>,
}

/// One player's device description, read once.
#[derive(Debug, Clone)]
struct Read {
    took: Duration,
    description: Result<DeviceDescription, String>,
}

impl LanProbe {
    /// Probe through `lan`, trying `seeds` as well as SSDP.
    #[must_use]
    pub fn new(lan: Arc<Lan>, seeds: Vec<IpAddr>) -> Self {
        Self {
            lan,
            seeds,
            ssdp_wait: Duration::from_secs(3),
            adverts: OnceLock::new(),
            reads: Mutex::new(BTreeMap::new()),
            survey: OnceLock::new(),
        }
    }

    /// How long to listen for SSDP replies (default 3 s).
    #[must_use]
    pub fn with_ssdp_wait(mut self, wait: Duration) -> Self {
        self.ssdp_wait = wait;
        self
    }

    fn adverts(&self) -> &Result<Vec<Advert>, String> {
        self.adverts.get_or_init(|| {
            self.lan
                .ssdp_search(1, self.ssdp_wait)
                .map_err(|e| e.to_string())
        })
    }

    /// The device description of the player at `ip`, read once.
    fn read(&self, ip: IpAddr) -> Read {
        if let Some(read) = self.reads.lock().ok().and_then(|r| r.get(&ip).cloned()) {
            return read;
        }
        let started = Instant::now();
        let description = self
            .lan
            .http_get(&description_url(ip))
            .map_err(|e| e.to_string())
            .and_then(|body| parse_device_description(&body).map_err(|e| e.to_string()));
        let read = Read {
            took: started.elapsed(),
            description,
        };
        if let Ok(mut reads) = self.reads.lock() {
            reads.insert(ip, read.clone());
        }
        read
    }

    /// Every player address SSDP or the seeds turned up, in order.
    fn addresses(&self) -> Vec<IpAddr> {
        let mut all: Vec<IpAddr> = Vec::new();
        if let Ok(adverts) = self.adverts() {
            all.extend(adverts.iter().filter_map(|a| host_of_location(&a.location)));
        }
        all.extend(self.seeds.iter().copied());
        let mut unique = Vec::new();
        for ip in all {
            if !unique.contains(&ip) {
                unique.push(ip);
            }
        }
        unique
    }

    fn survey(&self) -> &Result<Survey, String> {
        self.survey.get_or_init(|| {
            inventory::survey(&*self.lan, &self.seeds, self.ssdp_wait).map_err(|e| e.to_string())
        })
    }
}

/// Register every LAN check, with the store check for `data_dir`.
pub fn register(runner: &mut Runner, data_dir: PathBuf, probe: &Arc<LanProbe>) {
    runner.register(StoreOpenCheck { data_dir });
    for kind in [
        Kind::Ssdp,
        Kind::Seeds,
        Kind::Players,
        Kind::Households,
        Kind::GenaRoundtrip,
        Kind::Topology,
    ] {
        runner.register(LanCheck {
            kind,
            probe: Arc::clone(probe),
        });
    }
}

// ── store.open ───────────────────────────────────────────────────────────

struct StoreOpenCheck {
    data_dir: PathBuf,
}

impl Check for StoreOpenCheck {
    fn id(&self) -> CheckId {
        STORE_OPEN
    }
    fn title(&self) -> &'static str {
        "Local store"
    }
    fn run(&self, _ctx: &CheckContext) -> CheckResult {
        store_open(&self.data_dir)
    }
}

/// The data directory exists and is writable, and the store opens with its
/// schema current (opening applies any pending migrations, as the daemon
/// would).
fn store_open(data_dir: &std::path::Path) -> CheckResult {
    let remedy = "Set FSONOS_DATA_DIR to a directory this user can write, or fix its permissions.";
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        return CheckResult::fail(format!("cannot create {}: {e}", data_dir.display()), remedy);
    }
    let probe = data_dir.join(".fsonos-doctor-write-test");
    if let Err(e) = std::fs::write(&probe, b"ok") {
        return CheckResult::fail(
            format!("{} is not writable: {e}", data_dir.display()),
            remedy,
        );
    }
    let _ = std::fs::remove_file(&probe);
    let path = data_dir.join(crate::store::FILE_NAME);
    match SqliteStore::open(&path) {
        Ok(store) => {
            let versions = store.schema_versions().unwrap_or_default();
            let latest = versions.last().copied().unwrap_or(0);
            let _ = store.close();
            CheckResult::pass(format!("store open at schema version {latest}"))
                .with_evidence(json!({ "path": path, "schema_versions": versions }))
        }
        Err(e) => CheckResult::fail(
            format!("the store at {} does not open: {e}", path.display()),
            "Move the file aside (it is a cache plus history) and run again, or point \
             FSONOS_DATA_DIR elsewhere.",
        ),
    }
}

// ── the LAN checks ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Kind {
    Ssdp,
    Seeds,
    Players,
    Households,
    GenaRoundtrip,
    Topology,
}

struct LanCheck {
    kind: Kind,
    probe: Arc<LanProbe>,
}

impl Check for LanCheck {
    fn id(&self) -> CheckId {
        match self.kind {
            Kind::Ssdp => SSDP,
            Kind::Seeds => SEEDS,
            Kind::Players => PLAYERS,
            Kind::Households => HOUSEHOLDS,
            Kind::GenaRoundtrip => GENA_ROUNDTRIP,
            Kind::Topology => TOPOLOGY,
        }
    }

    fn title(&self) -> &'static str {
        match self.kind {
            Kind::Ssdp => "SSDP discovery",
            Kind::Seeds => "Seed addresses",
            Kind::Players => "Players answer",
            Kind::Households => "Households (S1/S2)",
            Kind::GenaRoundtrip => "Event callbacks (GENA)",
            Kind::Topology => "Group topology",
        }
    }

    /// Checks that need some player found wait for discovery. Nothing waits
    /// for lan.players: one dead speaker must not hide the others' results.
    fn requires(&self) -> &[CheckId] {
        match self.kind {
            Kind::Ssdp | Kind::Seeds => &[],
            Kind::Players | Kind::Households | Kind::GenaRoundtrip | Kind::Topology => &[SSDP],
        }
    }

    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let p = &self.probe;
        match self.kind {
            Kind::Ssdp => {
                let seeds = seed_reads(p);
                ssdp_result(p.adverts(), &seeds, cfg!(target_os = "macos"))
            }
            Kind::Seeds => seeds_result(&seed_reads(p)),
            Kind::Players => {
                let reads: Vec<(IpAddr, Result<Duration, String>)> = p
                    .addresses()
                    .into_iter()
                    .map(|ip| {
                        let read = p.read(ip);
                        (ip, read.description.map(|_| read.took))
                    })
                    .collect();
                players_result(&reads)
            }
            Kind::Households => {
                let homes = household_of(p);
                let found: Vec<(Option<String>, DeviceDescription)> = p
                    .addresses()
                    .into_iter()
                    .filter_map(|ip| {
                        let d = p.read(ip).description.ok()?;
                        Some((homes.get(&ip).cloned().flatten(), d))
                    })
                    .collect();
                households_result(&found)
            }
            Kind::GenaRoundtrip => gena_result(gena_roundtrip(p, ctx)),
            Kind::Topology => match p.survey() {
                Ok(survey) => {
                    let reachable: Vec<(PlayerId, Result<(), String>)> = survey
                        .households
                        .iter()
                        .flat_map(|h| h.groups.iter().map(move |g| (h, &g.coordinator)))
                        // A Bridge leads a group of its own but plays nothing.
                        .filter(|(h, c)| h.player(c).is_some())
                        .map(|(h, c)| (c.clone(), coordinator_answers(&*p.lan, h, c)))
                        .collect();
                    topology_result(&survey.households, &reachable)
                }
                Err(e) => CheckResult::fail(
                    format!("the topology could not be read: {e}"),
                    "Fix discovery first (see lan.ssdp and lan.players).",
                ),
            },
        }
    }
}

fn seed_reads(p: &LanProbe) -> Vec<(IpAddr, Result<Duration, String>)> {
    p.seeds
        .iter()
        .map(|&ip| {
            let read = p.read(ip);
            (ip, read.description.map(|_| read.took))
        })
        .collect()
}

/// Each discovered address's household, from its SSDP reply.
fn household_of(p: &LanProbe) -> BTreeMap<IpAddr, Option<String>> {
    match p.adverts() {
        Ok(adverts) => adverts
            .iter()
            .filter_map(|a| Some((host_of_location(&a.location)?, a.household.clone())))
            .collect(),
        Err(_) => BTreeMap::new(),
    }
}

fn coordinator_answers<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    coordinator: &PlayerId,
) -> Result<(), String> {
    let ip = household
        .player(coordinator)
        .map(|p| p.ip)
        .ok_or_else(|| "no address known".to_string())?;
    soap::get_transport_info(t, ip)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// What the GENA round trip saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GenaOutcome {
    NoPlayer,
    Sink(String),
    Subscribe(String),
    /// Subscribed, but no NOTIFY within the wait.
    Silent {
        waited: Duration,
        sink: SocketAddr,
    },
    Notified {
        after: Duration,
        sink: SocketAddr,
    },
}

fn gena_roundtrip(p: &LanProbe, ctx: &CheckContext) -> GenaOutcome {
    let Some(ip) = p
        .addresses()
        .into_iter()
        .find(|&ip| p.read(ip).description.is_ok_and(|d| d.is_renderer()))
    else {
        return GenaOutcome::NoPlayer;
    };
    let local = match p.lan.local_address_toward(ip) {
        Ok(local) => local,
        Err(e) => return GenaOutcome::Sink(e.to_string()),
    };
    let sink = match EventSink::start(SocketAddr::new(local, 0)) {
        Ok(sink) => sink,
        Err(e) => return GenaOutcome::Sink(e.to_string()),
    };
    let started = Instant::now();
    let sub = match p.lan.subscribe(
        ip,
        RENDERING_CONTROL.event_path,
        &sink.callback_url("doctor"),
        60,
    ) {
        Ok(sub) => sub,
        Err(e) => return GenaOutcome::Subscribe(e.to_string()),
    };
    let wait = NOTIFY_WAIT.min(ctx.remaining());
    let mut outcome = GenaOutcome::Silent {
        waited: wait,
        sink: sink.local_addr(),
    };
    while started.elapsed() < wait && !ctx.is_cancelled() {
        if let Some(notify) = sink.recv_timeout(Duration::from_millis(100))
            && notify.sid == sub.sid
        {
            outcome = GenaOutcome::Notified {
                after: started.elapsed(),
                sink: sink.local_addr(),
            };
            break;
        }
    }
    // Always clean up, whatever happened.
    let _ = p
        .lan
        .unsubscribe(ip, RENDERING_CONTROL.event_path, &sub.sid);
    outcome
}

// ── verdicts: pure functions of what was observed ────────────────────────

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// `lan.ssdp`: who answered the M-SEARCH, by household.
pub(crate) fn ssdp_result(
    adverts: &Result<Vec<Advert>, String>,
    seeds: &[(IpAddr, Result<Duration, String>)],
    macos: bool,
) -> CheckResult {
    let seeds_work = seeds.iter().any(|(_, r)| r.is_ok());
    let remedy = if macos {
        format!("{LOCAL_NETWORK_REMEDY} {MULTICAST_REMEDY}")
    } else {
        MULTICAST_REMEDY.to_string()
    };
    let adverts = match adverts {
        Ok(a) if !a.is_empty() => a,
        Ok(_) | Err(_) => {
            let why = match adverts {
                Err(e) => format!("SSDP failed: {e}"),
                Ok(_) => "no player answered the SSDP search".to_string(),
            };
            return if seeds_work {
                CheckResult::warn(format!("{why}; the seed addresses work"), remedy)
            } else {
                CheckResult::fail(why, remedy)
            };
        }
    };
    let mut by_home: BTreeMap<String, usize> = BTreeMap::new();
    for a in adverts {
        *by_home
            .entry(a.household.clone().unwrap_or_else(|| "unknown".into()))
            .or_default() += 1;
    }
    CheckResult::pass(format!(
        "{} player(s) answered in {} household(s)",
        adverts.len(),
        by_home.len()
    ))
    .with_evidence(json!({ "by_household": by_home }))
}

/// `lan.seeds`: every configured seed answers.
pub(crate) fn seeds_result(seeds: &[(IpAddr, Result<Duration, String>)]) -> CheckResult {
    if seeds.is_empty() {
        return CheckResult::skip("no seed addresses configured");
    }
    let dead: Vec<String> = seeds
        .iter()
        .filter_map(|(ip, r)| r.as_ref().err().map(|e| format!("{ip}: {e}")))
        .collect();
    if dead.is_empty() {
        CheckResult::pass(format!("all {} seed(s) answer", seeds.len()))
    } else {
        CheckResult::fail(
            format!("{} of {} seed(s) do not answer", dead.len(), seeds.len()),
            "Check the addresses in the seeds file (players keep their address only with a \
             DHCP reservation), and that the players are powered on.",
        )
        .with_detail(dead.join("; "))
    }
}

/// `lan.players`: every found player serves its description, quickly.
pub(crate) fn players_result(reads: &[(IpAddr, Result<Duration, String>)]) -> CheckResult {
    if reads.is_empty() {
        return CheckResult::fail(
            "no players were found to read",
            "Fix discovery first (see lan.ssdp), or add seed addresses.",
        );
    }
    let evidence: Vec<_> = reads
        .iter()
        .map(|(ip, r)| match r {
            Ok(took) => json!({ "ip": ip, "ms": ms(*took) }),
            Err(e) => json!({ "ip": ip, "error": e }),
        })
        .collect();
    let dead: Vec<String> = reads
        .iter()
        .filter_map(|(ip, r)| r.as_ref().err().map(|e| format!("{ip}: {e}")))
        .collect();
    if !dead.is_empty() {
        return CheckResult::fail(
            format!("{} of {} player(s) do not answer", dead.len(), reads.len()),
            "Power-cycle the players that do not answer, and check that this host and the \
             players are on the same network (a guest network or VPN can isolate them).",
        )
        .with_detail(dead.join("; "))
        .with_evidence(json!(evidence));
    }
    let slow: Vec<String> = reads
        .iter()
        .filter_map(|(ip, r)| match r {
            Ok(took) if *took > SLOW_PLAYER => Some(format!("{ip}: {} ms", ms(*took))),
            _ => None,
        })
        .collect();
    if slow.is_empty() {
        CheckResult::pass(format!("all {} player(s) answer", reads.len()))
            .with_evidence(json!(evidence))
    } else {
        CheckResult::warn(
            format!("{} player(s) answer slowly", slow.len()),
            "Slow answers usually mean weak Wi-Fi to that player: move it closer to an access \
             point or wire one player (SonosNet on S1).",
        )
        .with_detail(slow.join("; "))
        .with_evidence(json!(evidence))
    }
}

/// `lan.households`: S1/S2 per household; a generation guessed from the
/// model (no `swGen` and not S1-only hardware) is a warning.
pub(crate) fn households_result(found: &[(Option<String>, DeviceDescription)]) -> CheckResult {
    let mut summary: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut guessed = Vec::new();
    for (home, d) in found.iter().filter(|(_, d)| d.is_renderer()) {
        let generation = classify(d);
        let entry = summary
            .entry(home.clone().unwrap_or_else(|| "unknown".into()))
            .or_default();
        match generation {
            Generation::S1 => entry.0 += 1,
            Generation::S2 => entry.1 += 1,
        }
        if d.sw_gen.is_none() && generation == Generation::S2 {
            guessed.push(d.room_name.clone());
        }
    }
    if summary.is_empty() {
        return CheckResult::skip("no players could be read (see lan.players)");
    }
    let text: Vec<String> = summary
        .iter()
        .map(|(home, (s1, s2))| format!("{home}: {s1} S1, {s2} S2"))
        .collect();
    let evidence = json!({ "households": summary.iter().map(|(h, (s1, s2))| json!({"household": h, "s1": s1, "s2": s2})).collect::<Vec<_>>() });
    if guessed.is_empty() {
        CheckResult::pass(text.join("; ")).with_evidence(evidence)
    } else {
        CheckResult::warn(
            format!("generation assumed S2 for {}", guessed.join(", ")),
            "These players did not report swGen; update them in the Sonos app so S1/S2 \
             handling is certain.",
        )
        .with_detail(text.join("; "))
        .with_evidence(evidence)
    }
}

/// `lan.gena_roundtrip`: a player's NOTIFY reaches this host.
pub(crate) fn gena_result(outcome: GenaOutcome) -> CheckResult {
    const FIREWALL: &str = "Allow incoming connections for fsonos (macOS: System Settings > \
         Network > Firewall > Options); on a multi-homed host make sure the callback uses the \
         interface on the speakers' subnet; a VPN can route the players' replies away.";
    match outcome {
        GenaOutcome::NoPlayer => CheckResult::skip("no player answered to subscribe to"),
        GenaOutcome::Sink(e) => {
            CheckResult::fail(format!("cannot listen for events: {e}"), FIREWALL)
        }
        GenaOutcome::Subscribe(e) => CheckResult::fail(
            format!("the player refused the subscription: {e}"),
            "Check that the player is up (lan.players) and not overloaded; retry.",
        ),
        GenaOutcome::Silent { waited, sink } => CheckResult::fail(
            format!(
                "subscribed, but no event reached {sink} within {} s",
                waited.as_secs()
            ),
            FIREWALL,
        )
        .with_evidence(json!({ "sink": sink.to_string() })),
        GenaOutcome::Notified { after, sink } => {
            CheckResult::pass(format!("events arrive ({} ms)", ms(after)))
                .with_evidence(json!({ "sink": sink.to_string(), "ms": ms(after) }))
        }
    }
}

/// `lan.topology`: every player in exactly one group, every coordinator up.
pub(crate) fn topology_result(
    households: &[HouseholdState],
    coordinators: &[(PlayerId, Result<(), String>)],
) -> CheckResult {
    let mut problems = Vec::new();
    for h in households {
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for g in &h.groups {
            for m in &g.members {
                *seen.entry(m.0.as_str()).or_default() += 1;
            }
        }
        for (id, n) in &seen {
            if *n > 1 {
                problems.push(format!("{id} is in {n} groups"));
            }
        }
        for p in &h.players {
            if !seen.contains_key(p.id.0.as_str()) {
                problems.push(format!("{} ({}) is in no group", p.room_name, p.id.0));
            }
        }
    }
    for (id, r) in coordinators {
        if let Err(e) = r {
            problems.push(format!("coordinator {} does not answer: {e}", id.0));
        }
    }
    let groups: usize = households.iter().map(|h| h.groups.len()).sum();
    if groups == 0 && problems.is_empty() {
        return CheckResult::skip("no household topology was read");
    }
    if problems.is_empty() {
        CheckResult::pass(format!(
            "{groups} group(s) in {} household(s), every coordinator answers",
            households.len()
        ))
    } else {
        CheckResult::fail(
            format!("{} topology problem(s)", problems.len()),
            "A player that just rebooted or changed address can leave a stale group: wait a \
             minute and re-run; if it persists, power-cycle the affected player.",
        )
        .with_detail(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;
    use fsonos_types::{Player, ZoneGroup};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn advert(host: &str, household: &str) -> Advert {
        Advert {
            location: format!("http://{host}:1400/xml/device_description.xml"),
            st: "urn:schemas-upnp-org:device:ZonePlayer:1".into(),
            usn: None,
            household: Some(household.into()),
            boot_seq: None,
        }
    }

    #[test]
    fn ssdp_passes_warns_or_fails() {
        let r = ssdp_result(
            &Ok(vec![
                advert("192.0.2.10", "Sonos_S1"),
                advert("192.0.2.11", "Sonos_S1"),
                advert("192.0.2.20", "Sonos_S2"),
            ]),
            &[],
            false,
        );
        assert_eq!(r.status, Status::Pass);
        assert_eq!(r.summary, "3 player(s) answered in 2 household(s)");

        let seed_ok = [(ip("192.0.2.10"), Ok(Duration::from_millis(20)))];
        let r = ssdp_result(&Ok(vec![]), &seed_ok, true);
        assert_eq!(r.status, Status::Warn);
        assert!(r.remedy.as_deref().unwrap().contains("Local Network"));

        let r = ssdp_result(&Err("socket error".into()), &[], false);
        assert_eq!(r.status, Status::Fail);
        assert!(!r.remedy.as_deref().unwrap().contains("Local Network"));
        assert!(r.remedy.as_deref().unwrap().contains("seeds file"));
    }

    #[test]
    fn seeds_and_players_name_who_failed() {
        assert_eq!(seeds_result(&[]).status, Status::Skip);
        let r = seeds_result(&[
            (ip("192.0.2.10"), Ok(Duration::from_millis(5))),
            (ip("192.0.2.99"), Err("connection refused".into())),
        ]);
        assert_eq!(r.status, Status::Fail);
        assert_eq!(r.detail.as_deref(), Some("192.0.2.99: connection refused"));

        assert_eq!(players_result(&[]).status, Status::Fail);
        let r = players_result(&[
            (ip("192.0.2.10"), Ok(Duration::from_millis(5))),
            (ip("192.0.2.11"), Ok(Duration::from_millis(900))),
        ]);
        assert_eq!(r.status, Status::Warn);
        assert_eq!(r.detail.as_deref(), Some("192.0.2.11: 900 ms"));
        let r = players_result(&[(ip("192.0.2.12"), Err("timed out".into()))]);
        assert_eq!(r.status, Status::Fail);
    }

    fn desc(room: &str, model: &str, sw_gen: Option<u8>) -> DeviceDescription {
        let mut d = parse_device_description(include_str!(
            "../../../fsonos-proto/tests/fixtures/device_description_s2_one.xml"
        ))
        .unwrap();
        d.room_name = room.into();
        d.model_number = model.into();
        d.sw_gen = sw_gen;
        d
    }

    #[test]
    fn households_warn_when_the_generation_is_a_guess() {
        let found = [
            (Some("Sonos_S1".into()), desc("Kitchen", "S5", Some(1))),
            (Some("Sonos_S2".into()), desc("Den", "S13", Some(2))),
        ];
        let r = households_result(&found);
        assert_eq!(r.status, Status::Pass);
        assert_eq!(r.summary, "Sonos_S1: 1 S1, 0 S2; Sonos_S2: 0 S1, 1 S2");
        let guess = [(Some("Sonos_S2".into()), desc("Den", "S13", None))];
        let r = households_result(&guess);
        assert_eq!(r.status, Status::Warn);
        assert_eq!(r.summary, "generation assumed S2 for Den");
        // S1-only hardware without swGen is certain.
        let certain = [(None, desc("Kitchen", "S5", None))];
        assert_eq!(households_result(&certain).status, Status::Pass);
        assert_eq!(households_result(&[]).status, Status::Skip);
    }

    #[test]
    fn gena_outcomes_map_to_remedies() {
        let sink: SocketAddr = "192.0.2.5:40000".parse().unwrap();
        assert_eq!(gena_result(GenaOutcome::NoPlayer).status, Status::Skip);
        let r = gena_result(GenaOutcome::Notified {
            after: Duration::from_millis(42),
            sink,
        });
        assert_eq!(
            (r.status, r.summary.as_str()),
            (Status::Pass, "events arrive (42 ms)")
        );
        let r = gena_result(GenaOutcome::Silent {
            waited: NOTIFY_WAIT,
            sink,
        });
        assert_eq!(r.status, Status::Fail);
        assert!(r.remedy.as_deref().unwrap().contains("Firewall"));
        assert_eq!(
            gena_result(GenaOutcome::Subscribe("412".into())).status,
            Status::Fail
        );
    }

    fn pid(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    fn player(id: &str) -> Player {
        Player {
            id: pid(id),
            room_name: id.into(),
            ip: ip("192.0.2.10"),
            model: "One".into(),
            generation: Generation::S2,
        }
    }

    #[test]
    fn topology_finds_orphans_duplicates_and_dead_coordinators() {
        let good = HouseholdState {
            id: None,
            players: vec![player("A"), player("B")],
            groups: vec![ZoneGroup {
                coordinator: pid("A"),
                members: vec![pid("A"), pid("B")],
            }],
            rooms: Vec::new(),
        };
        let r = topology_result(std::slice::from_ref(&good), &[(pid("A"), Ok(()))]);
        assert_eq!(r.status, Status::Pass);

        let mut bad = good;
        bad.players.push(player("C"));
        bad.groups.push(ZoneGroup {
            coordinator: pid("B"),
            members: vec![pid("B")],
        });
        let r = topology_result(&[bad], &[(pid("A"), Ok(())), (pid("B"), Err("503".into()))]);
        assert_eq!(r.status, Status::Fail);
        let detail = r.detail.unwrap();
        assert!(detail.contains("B is in 2 groups"), "{detail}");
        assert!(detail.contains("C (C) is in no group"), "{detail}");
        assert!(
            detail.contains("coordinator B does not answer: 503"),
            "{detail}"
        );
    }

    #[test]
    fn an_empty_topology_is_skipped_not_passed() {
        assert_eq!(topology_result(&[], &[]).status, Status::Skip);
        assert_eq!(
            topology_result(&[HouseholdState::default()], &[]).status,
            Status::Skip
        );
    }

    #[test]
    fn store_opens_in_a_fresh_dir_and_fails_on_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let r = store_open(&dir.path().join("data"));
        assert_eq!(r.status, Status::Pass, "{r:?}");
        assert!(r.summary.starts_with("store open at schema version "));
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let r = store_open(&file);
        assert_eq!(r.status, Status::Fail);
        assert!(r.remedy.as_deref().unwrap().contains("FSONOS_DATA_DIR"));
    }
}
