//! The LAN checks: can this host open its store, find the players, read them,
//! and hear their events?
//!
//! Discovery and eventing failures are the most common and most confusing
//! problems with local Sonos control: they look like "nothing happens". Each
//! check here names the failure and the fix. All of them are read-only; the
//! GENA round trip and the update read (`lan.updates`) cancel their
//! subscriptions even when they time out.
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
use fsonos_proto::soap::{RENDERING_CONTROL, ZONE_GROUP_TOPOLOGY};
use fsonos_proto::ssdp::{Advert, description_url};
use fsonos_proto::topology::{
    UpdateOffer, host_of_location, is_older_version, parse_update_offer, parse_zone_group_state,
};
use fsonos_proto::{Transport, control as soap};
use fsonos_types::{Generation, PlayerId};
use serde_json::json;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

mod isolation;
pub use isolation::{Neighbors, system_neighbors};

pub const STORE_OPEN: CheckId = CheckId("store.open");
pub const SSDP: CheckId = CheckId("lan.ssdp");
pub const SEEDS: CheckId = CheckId("lan.seeds");
pub const PLAYERS: CheckId = CheckId("lan.players");
pub const HOUSEHOLDS: CheckId = CheckId("lan.households");
pub const GENA_ROUNDTRIP: CheckId = CheckId("lan.gena_roundtrip");
pub const TOPOLOGY: CheckId = CheckId("lan.topology");
pub const UPDATES: CheckId = CheckId("lan.updates");
pub const ISOLATION: CheckId = CheckId("lan.isolation");

/// A player read slower than this is a warning.
pub const SLOW_PLAYER: Duration = Duration::from_millis(500);
/// How long the GENA round trip waits for the initial NOTIFY.
pub const NOTIFY_WAIT: Duration = Duration::from_secs(5);

const LOCAL_NETWORK_REMEDY: &str = "On macOS, allow Local Network access: System Settings > \
     Privacy & Security > Local Network > enable fsonos (or your terminal, or the launchd job's \
     program).";
const LOCAL_NETWORK_DENIED_REMEDY: &str = "Allow fsonos in System Settings > Privacy & \
     Security > Local Network. A program that has never asked needs one run from the logged-in \
     GUI session to get the prompt; a terminal session already has its terminal's permission \
     (docs/DEPLOY.md).";
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
    /// Reads this host's neighbor table ([`system_neighbors`]).
    neighbors: fn() -> Option<Neighbors>,
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
            neighbors: system_neighbors,
        }
    }

    /// Read the neighbor table with `neighbors` (tests; the default reads
    /// this host's).
    #[must_use]
    pub fn with_neighbors(mut self, neighbors: fn() -> Option<Neighbors>) -> Self {
        self.neighbors = neighbors;
        self
    }

    /// How long to listen for SSDP replies (default 3 s).
    #[must_use]
    pub fn with_ssdp_wait(mut self, wait: Duration) -> Self {
        self.ssdp_wait = wait;
        self
    }

    /// The households the probe's survey found (surveying once if no check
    /// has yet), or why the survey failed.
    pub fn households(&self) -> Result<&[HouseholdState], &str> {
        self.survey()
            .as_ref()
            .map(|s| s.households.as_slice())
            .map_err(String::as_str)
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
        Kind::Updates,
        Kind::Isolation,
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
    Updates,
    Isolation,
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
            Kind::Updates => UPDATES,
            Kind::Isolation => ISOLATION,
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
            Kind::Updates => "Software updates",
            Kind::Isolation => "Client isolation",
        }
    }

    /// Checks that need some player found wait for discovery. Nothing waits
    /// for lan.players: one dead speaker must not hide the others' results.
    /// lan.isolation runs most of all when discovery found nothing.
    fn requires(&self) -> &[CheckId] {
        match self.kind {
            Kind::Ssdp | Kind::Seeds | Kind::Isolation => &[],
            Kind::Players | Kind::Households | Kind::GenaRoundtrip | Kind::Topology => &[SSDP],
            // The offer arrives only in a topology event.
            Kind::Updates => &[GENA_ROUNDTRIP],
        }
    }

    fn run(&self, ctx: &CheckContext) -> CheckResult {
        let p = &self.probe;
        match self.kind {
            Kind::Ssdp => {
                let seeds = seed_reads(p);
                ssdp_result(p.adverts(), &seeds, cfg!(target_os = "macos"))
            }
            Kind::Seeds => seeds_result(&seed_reads(p), cfg!(target_os = "macos")),
            Kind::Players => {
                let reads: Vec<(IpAddr, Result<Duration, String>)> = p
                    .addresses()
                    .into_iter()
                    .map(|ip| {
                        let read = p.read(ip);
                        (ip, read.description.map(|_| read.took))
                    })
                    .collect();
                players_result(&reads, cfg!(target_os = "macos"))
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
            Kind::Isolation => {
                let answer = p
                    .addresses()
                    .into_iter()
                    .any(|ip| p.read(ip).description.is_ok());
                let neighbors = if answer { None } else { (p.neighbors)() };
                isolation::result(answer, neighbors.as_ref())
            }
            Kind::Updates => match p.survey() {
                Ok(survey) => updates_result(&update_offers(p, &survey.households, ctx)),
                Err(e) => CheckResult::skip(format!("no household to ask: {e}")),
            },
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
                    topology_result(&survey.households, &reachable, &p.addresses())
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

/// One household's update offer, as its topology event told it.
#[derive(Debug)]
pub(crate) struct HouseholdOffer {
    /// S1, S2, or the household id.
    pub household: String,
    /// The offer (`None`: the event carried none), or why the event could
    /// not be read.
    pub read: Result<Option<Offered>, String>,
}

/// An update offer, and every player's room and software version.
#[derive(Debug)]
pub(crate) struct Offered {
    pub offer: UpdateOffer,
    pub players: Vec<(String, String)>,
}

/// Ask one player of each household for its topology event and read the
/// update offer in it. The subscriptions end before this returns.
fn update_offers(
    p: &LanProbe,
    households: &[HouseholdState],
    ctx: &CheckContext,
) -> Vec<HouseholdOffer> {
    let mut sink: Option<EventSink> = None;
    households
        .iter()
        .filter_map(|h| {
            let player = h.players.first()?;
            let household = match h.generation() {
                Some(Generation::S1) => "S1".to_string(),
                Some(Generation::S2) => "S2".to_string(),
                None => {
                    h.id.as_ref()
                        .map_or_else(|| "a household".to_string(), |id| id.0.clone())
                }
            };
            let read =
                topology_event(p, player.ip, &mut sink, ctx).and_then(|props| offer_in(&props));
            Some(HouseholdOffer { household, read })
        })
        .collect()
}

/// The properties of the first topology event the player at `ip` sends.
fn topology_event(
    p: &LanProbe,
    ip: IpAddr,
    sink: &mut Option<EventSink>,
    ctx: &CheckContext,
) -> Result<Vec<(String, String)>, String> {
    if sink.is_none() {
        let local = p.lan.local_address_toward(ip).map_err(|e| e.to_string())?;
        *sink = Some(EventSink::start(SocketAddr::new(local, 0)).map_err(|e| e.to_string())?);
    }
    let Some(sink) = sink.as_ref() else {
        return Err("no event listener".into());
    };
    let started = Instant::now();
    let sub = p
        .lan
        .subscribe(
            ip,
            ZONE_GROUP_TOPOLOGY.event_path,
            &sink.callback_url("doctor-updates"),
            60,
        )
        .map_err(|e| e.to_string())?;
    let wait = NOTIFY_WAIT.min(ctx.remaining());
    let mut props = None;
    while props.is_none() && started.elapsed() < wait && !ctx.is_cancelled() {
        if let Some(notify) = sink.recv_timeout(Duration::from_millis(100))
            && notify.sid == sub.sid
        {
            props = Some(notify.properties);
        }
    }
    // Always clean up, whatever happened.
    let _ = p
        .lan
        .unsubscribe(ip, ZONE_GROUP_TOPOLOGY.event_path, &sub.sid);
    props.ok_or_else(|| format!("no topology event within {} s", wait.as_secs()))
}

/// The update offer in a topology event, with each player's room and
/// software version from the same event.
fn offer_in(props: &[(String, String)]) -> Result<Option<Offered>, String> {
    let property = |name: &str| {
        props
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let Some(offer) = property("AvailableSoftwareUpdate").and_then(parse_update_offer) else {
        return Ok(None);
    };
    let state = property("ZoneGroupState").ok_or("the topology event has no ZoneGroupState")?;
    let state = parse_zone_group_state(state).map_err(|e| e.to_string())?;
    let players = state
        .groups
        .iter()
        .flat_map(|g| &g.members)
        .flat_map(|m| std::iter::once(m).chain(&m.satellites))
        // The offer is for one generation: an S1 offer says nothing of S2.
        .filter(|m| offer.sw_gen.is_none() || m.sw_gen.is_none() || m.sw_gen == offer.sw_gen)
        .filter_map(|m| Some((m.zone_name.clone(), m.software_version.clone()?)))
        .collect();
    Ok(Some(Offered { offer, players }))
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

/// Every one of `reads` failed the way macOS fails a process its Local
/// Network privacy has not allowed (a launchd job, most often): refused at
/// once with "No route to host" (EHOSTUNREACH, os error 65). A powered-off
/// player times out instead; a route down to the speakers' subnet is the
/// one other cause.
fn local_network_denied(reads: &[(IpAddr, Result<Duration, String>)], macos: bool) -> bool {
    macos
        && !reads.is_empty()
        && reads
            .iter()
            .all(|(_, r)| r.as_ref().is_err_and(|e| e.contains("os error 65")))
}

/// The verdict on `reads` that [`local_network_denied`] recognizes.
fn denied_result(reads: &[(IpAddr, Result<Duration, String>)], dead: &[String]) -> CheckResult {
    CheckResult::fail(
        format!(
            "macOS refused all {} connection(s) at once with \"No route to host\": Local \
             Network privacy has most likely not allowed this process (or there is no route \
             to the speakers' network)",
            reads.len()
        ),
        LOCAL_NETWORK_DENIED_REMEDY,
    )
    .with_detail(dead.join("; "))
}

/// `lan.seeds`: every configured seed answers.
pub(crate) fn seeds_result(
    seeds: &[(IpAddr, Result<Duration, String>)],
    macos: bool,
) -> CheckResult {
    if seeds.is_empty() {
        return CheckResult::skip("no seed addresses configured");
    }
    let dead: Vec<String> = seeds
        .iter()
        .filter_map(|(ip, r)| r.as_ref().err().map(|e| format!("{ip}: {e}")))
        .collect();
    if local_network_denied(seeds, macos) {
        return denied_result(seeds, &dead);
    }
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
pub(crate) fn players_result(
    reads: &[(IpAddr, Result<Duration, String>)],
    macos: bool,
) -> CheckResult {
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
    if local_network_denied(reads, macos) {
        return denied_result(reads, &dead).with_evidence(json!(evidence));
    }
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

/// `lan.topology`: every player in exactly one group, every coordinator up,
/// and every room's player at an address discovery (SSDP or a seed) found.
pub(crate) fn topology_result(
    households: &[HouseholdState],
    coordinators: &[(PlayerId, Result<(), String>)],
    discovered: &[IpAddr],
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
    // Rooms the survey only knows from another player's topology: on a
    // network SSDP does not cross (routed subnets), they are found only
    // while that player is up.
    let undiscovered: Vec<&str> = households
        .iter()
        .flat_map(|h| {
            h.rooms.iter().filter_map(move |r| {
                let primary = h.player(&r.primary)?;
                (!discovered.contains(&primary.ip)).then_some(r.name.as_str())
            })
        })
        .collect();
    let summary = format!(
        "{groups} group(s) in {} household(s), every coordinator answers",
        households.len()
    );
    if problems.is_empty() && undiscovered.is_empty() {
        CheckResult::pass(summary)
    } else if problems.is_empty() {
        CheckResult::warn(
            format!(
                "{summary}; not discovered from here: {}",
                undiscovered.join(", ")
            ),
            "SSDP does not reach these players from this host, so a survey finds them only \
             through another player's topology. Add their addresses to seeds.toml (or \
             FSONOS_SEEDS).",
        )
        .with_evidence(json!({ "undiscovered": undiscovered }))
    } else {
        CheckResult::fail(
            format!("{} topology problem(s)", problems.len()),
            "A player that just rebooted or changed address can leave a stale group: wait a \
             minute and re-run; if it persists, power-cycle the affected player.",
        )
        .with_detail(problems.join("; "))
    }
}

/// `lan.updates`: whether Sonos offers software newer than some player
/// runs. Only the offered version is read, never where it would come from:
/// FrankenSonos never installs updates.
pub(crate) fn updates_result(found: &[HouseholdOffer]) -> CheckResult {
    let mut behind: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut unread: Vec<String> = Vec::new();
    let mut evidence = Vec::new();
    for h in found {
        match &h.read {
            Ok(Some(Offered { offer, players })) => {
                let old: Vec<&(String, String)> = players
                    .iter()
                    .filter(|(_, version)| is_older_version(version, &offer.version))
                    .collect();
                let behind_rooms: Vec<serde_json::Value> = old
                    .iter()
                    .map(|(room, version)| json!({ "room": room, "version": version }))
                    .collect();
                evidence.push(json!({
                    "household": h.household,
                    "offered": offer.version,
                    "behind": behind_rooms,
                }));
                if old.is_empty() {
                    current.push(format!("{} runs {}", h.household, offer.version));
                } else {
                    let rooms: Vec<String> = old
                        .iter()
                        .map(|(room, version)| format!("{room} ({version})"))
                        .collect();
                    behind.push(format!(
                        "Sonos offers {} to the {} household; {} player(s) run older software: {}",
                        offer.version,
                        h.household,
                        old.len(),
                        rooms.join(", ")
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => unread.push(format!("{}: {e}", h.household)),
        }
    }
    let evidence = json!({ "households": evidence });
    if !behind.is_empty() {
        let mut result = CheckResult::warn(
            behind.join("; "),
            "FrankenSonos never installs updates. Update from the Sonos app when you choose: an \
             update interrupted at the wrong moment can crash a player (docs/ROBUSTNESS.md).",
        )
        .with_evidence(evidence);
        if !unread.is_empty() {
            result = result.with_detail(format!("not read: {}", unread.join("; ")));
        }
        result
    } else if !current.is_empty() {
        CheckResult::pass(format!(
            "every player runs the software Sonos offers ({})",
            current.join(", ")
        ))
        .with_evidence(evidence)
    } else if !unread.is_empty() {
        CheckResult::skip(format!(
            "could not read the update offers: {}",
            unread.join("; ")
        ))
    } else {
        CheckResult::skip("the players offer no software update")
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
        assert_eq!(seeds_result(&[], true).status, Status::Skip);
        let r = seeds_result(
            &[
                (ip("192.0.2.10"), Ok(Duration::from_millis(5))),
                (ip("192.0.2.99"), Err("connection refused".into())),
            ],
            true,
        );
        assert_eq!(r.status, Status::Fail);
        assert_eq!(r.detail.as_deref(), Some("192.0.2.99: connection refused"));

        assert_eq!(players_result(&[], true).status, Status::Fail);
        let r = players_result(
            &[
                (ip("192.0.2.10"), Ok(Duration::from_millis(5))),
                (ip("192.0.2.11"), Ok(Duration::from_millis(900))),
            ],
            true,
        );
        assert_eq!(r.status, Status::Warn);
        assert_eq!(r.detail.as_deref(), Some("192.0.2.11: 900 ms"));
        let r = players_result(&[(ip("192.0.2.12"), Err("timed out".into()))], true);
        assert_eq!(r.status, Status::Fail);
        assert!(!r.summary.contains("Local Network"), "{r:?}");
    }

    #[test]
    fn every_connection_refused_on_macos_names_the_local_network_denial() {
        let refused = || Err("network: No route to host (os error 65)".to_string());
        let reads = [(ip("192.0.2.10"), refused()), (ip("192.0.2.11"), refused())];
        for r in [seeds_result(&reads, true), players_result(&reads, true)] {
            assert_eq!(r.status, Status::Fail);
            assert!(r.summary.contains("Local Network privacy"), "{r:?}");
            assert!(r.summary.contains("all 2 connection(s)"), "{r:?}");
            assert!(r.remedy.as_deref().unwrap().contains("Privacy & Security"));
            assert!(r.detail.as_deref().unwrap().contains("192.0.2.11"));
        }
        // Not on macOS, or not every read refused that way: the usual verdict.
        for r in [seeds_result(&reads, false), players_result(&reads, false)] {
            assert!(!r.summary.contains("Local Network"), "{r:?}");
        }
        let mixed = [
            (ip("192.0.2.10"), refused()),
            (ip("192.0.2.11"), Err("timed out".into())),
        ];
        for r in [seeds_result(&mixed, true), players_result(&mixed, true)] {
            assert_eq!(r.status, Status::Fail);
            assert!(!r.summary.contains("Local Network"), "{r:?}");
        }
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
        let r = topology_result(std::slice::from_ref(&good), &[(pid("A"), Ok(()))], &[]);
        assert_eq!(r.status, Status::Pass);

        let mut bad = good;
        bad.players.push(player("C"));
        bad.groups.push(ZoneGroup {
            coordinator: pid("B"),
            members: vec![pid("B")],
        });
        let r = topology_result(
            &[bad],
            &[(pid("A"), Ok(())), (pid("B"), Err("503".into()))],
            &[],
        );
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
        assert_eq!(topology_result(&[], &[], &[]).status, Status::Skip);
        assert_eq!(
            topology_result(&[HouseholdState::default()], &[], &[]).status,
            Status::Skip
        );
    }

    #[test]
    fn a_room_discovery_never_found_warns_with_the_seeds_remedy() {
        let at = |id: &str, addr: &str| Player {
            ip: ip(addr),
            ..player(id)
        };
        let room = |id: &str| crate::Room {
            name: format!("Room {id}"),
            primary: pid(id),
            players: vec![pid(id)],
            missing: Vec::new(),
            coordinator: pid("A"),
        };
        let house = HouseholdState {
            id: None,
            players: vec![at("A", "192.0.2.10"), at("B", "192.0.2.11")],
            groups: vec![ZoneGroup {
                coordinator: pid("A"),
                members: vec![pid("A"), pid("B")],
            }],
            rooms: vec![room("A"), room("B")],
        };
        let up = [(pid("A"), Ok(()))];
        let r = topology_result(
            std::slice::from_ref(&house),
            &up,
            &[ip("192.0.2.10"), ip("192.0.2.11")],
        );
        assert_eq!(r.status, Status::Pass, "{r:?}");
        let r = topology_result(std::slice::from_ref(&house), &up, &[ip("192.0.2.10")]);
        assert_eq!(r.status, Status::Warn, "{r:?}");
        assert!(
            r.summary.ends_with("not discovered from here: Room B"),
            "{}",
            r.summary
        );
        assert!(r.remedy.as_deref().unwrap().contains("seeds.toml"));
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

    fn offer(household: &str, version: &str, players: &[(&str, &str)]) -> HouseholdOffer {
        HouseholdOffer {
            household: household.into(),
            read: Ok(Some(Offered {
                offer: UpdateOffer {
                    version: version.into(),
                    sw_gen: Some(2),
                },
                players: players
                    .iter()
                    .map(|(room, v)| ((*room).to_string(), (*v).to_string()))
                    .collect(),
            })),
        }
    }

    #[test]
    fn a_pending_update_warns_and_names_the_rooms_behind() {
        let r = updates_result(&[
            offer(
                "S2",
                "97.1-80312",
                &[("Kitchen", "86.10-80260"), ("Study", "97.1-80312")],
            ),
            HouseholdOffer {
                household: "S1".into(),
                read: Err("no topology event within 5 s".into()),
            },
        ]);
        assert_eq!(r.status, Status::Warn);
        assert_eq!(
            r.summary,
            "Sonos offers 97.1-80312 to the S2 household; 1 player(s) run older software: \
             Kitchen (86.10-80260)"
        );
        assert!(
            r.remedy
                .as_deref()
                .unwrap()
                .contains("never installs updates")
        );
        assert!(
            r.detail
                .as_deref()
                .unwrap()
                .contains("S1: no topology event")
        );
        assert_eq!(r.evidence["households"][0]["behind"][0]["room"], "Kitchen");
    }

    #[test]
    fn current_software_passes_and_no_offer_skips() {
        let r = updates_result(&[offer("S2", "97.1-80312", &[("Study", "97.1-80312")])]);
        assert_eq!(r.status, Status::Pass);
        assert!(r.summary.contains("S2 runs 97.1-80312"), "{}", r.summary);
        let none = HouseholdOffer {
            household: "S2".into(),
            read: Ok(None),
        };
        assert_eq!(updates_result(&[none]).status, Status::Skip);
        let unread = HouseholdOffer {
            household: "S1".into(),
            read: Err("refused".into()),
        };
        let r = updates_result(&[unread]);
        assert_eq!(r.status, Status::Skip);
        assert!(r.summary.contains("S1: refused"));
    }
}
