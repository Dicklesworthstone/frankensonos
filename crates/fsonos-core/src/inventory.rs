//! Device inventory: turning raw discovery results into a [`HouseholdState`].
//!
//! SSDP (and the direct-seed fallback) yields device-description URLs; this
//! module classifies each description as S1 or S2 and folds it into the
//! model. [`survey`] runs the whole pass over a [`Transport`];
//! classification is pure.

use crate::{CoreError, HouseholdState};
use fsonos_proto::description::{DeviceDescription, parse_device_description};
use fsonos_proto::topology::{VanishedDevice, get_zone_group_state, host_of_location};
use fsonos_proto::{Discovered, Transport, ssdp};
use fsonos_types::{Generation, HouseholdId, Player, PlayerId};
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

/// How long [`survey`] collects SSDP replies by default.
pub const DISCOVERY_WAIT: Duration = Duration::from_secs(2);

/// What a [`survey`] of the LAN found.
#[derive(Debug, Default)]
pub struct Survey {
    /// One entry per household, S1 before S2.
    pub households: Vec<HouseholdState>,
    /// Players that were found but could not be read, with the reason.
    pub unreachable: Vec<(String, String)>,
    /// Set when SSDP itself failed and the survey continued anyway: the
    /// seeds were tried, or mDNS found the players.
    pub ssdp_error: Option<String>,
    /// Each player's `BootSeq` from the topology reads (it rises on every
    /// boot, so a reboot between surveys shows here).
    pub boot_seqs: Vec<(PlayerId, u32)>,
    /// Players each household's topology lists as vanished (powered off, or
    /// out of reach of the household), with their room names.
    pub vanished: Vec<VanishedDevice>,
}

/// Find every household on the LAN. SSDP (plus any direct `seeds`, for
/// networks that drop multicast) finds the players; each player's device
/// description classifies it; one ZoneGroupTopology read per household
/// supplies its groups and rooms. A player that does not answer is listed in
/// [`Survey::unreachable`] rather than failing the survey.
pub fn survey<T: Transport + ?Sized>(
    t: &T,
    seeds: &[IpAddr],
    wait: Duration,
) -> Result<Survey, CoreError> {
    survey_observations(t, seeds, &[], wait).map(Observations::finish)
}

fn survey_observations<T: Transport + ?Sized>(
    t: &T,
    seeds: &[IpAddr],
    known: &[HouseholdState],
    wait: Duration,
) -> Result<Observations, CoreError> {
    let mut out = Observations::default();
    let mut found: Vec<(IpAddr, Option<String>)> = Vec::new();
    // Both channels within one `wait` where the transport listens on both
    // at once (see `Transport::discover`).
    let Discovered { ssdp, mdns } = t.discover(1, wait);
    let ssdp_error = match ssdp {
        Ok(adverts) => {
            for advert in adverts {
                if let Some(ip) = host_of_location(&advert.location)
                    && !found.iter().any(|(seen, _)| *seen == ip)
                {
                    found.push((ip, advert.household));
                }
            }
            None
        }
        Err(e) => Some(e),
    };

    // The second channel: mDNS sightings of `_sonos._tcp.local`. It fills
    // addresses SSDP missed and refines household hints (the S2 TXT carries
    // `hhid=`, which SSDP's USN only implies). A failure here never fails
    // the survey — with multicast blocked both channels fall back to seeds.
    if let Ok(adverts) = mdns {
        for advert in adverts {
            let Some(addr) = advert.addr else { continue };
            let ip = IpAddr::V4(addr);
            match found.iter_mut().find(|(seen, _)| *seen == ip) {
                Some(entry) => {
                    if entry.1.is_none() {
                        entry.1 = advert.household;
                    }
                }
                None => found.push((ip, advert.household)),
            }
        }
    }

    // SSDP's failure only surfaces now that both channels ran: with nothing
    // from either (and no seeds to fall back on) it is the survey's error;
    // with seeds it is recorded for the caller to see.
    if let Some(e) = ssdp_error {
        if found.is_empty() && seeds.is_empty() {
            return Err(e.into());
        }
        if !seeds.is_empty() {
            out.report.ssdp_error = Some(e.to_string());
        }
    }
    for &ip in seeds {
        if !found.iter().any(|(seen, _)| *seen == ip) {
            found.push((ip, None));
        }
    }

    Ok(read_players(t, found, known, out))
}

fn read_players<T: Transport + ?Sized>(
    t: &T,
    found: Vec<(IpAddr, Option<String>)>,
    known: &[HouseholdState],
    mut out: Observations,
) -> Observations {
    let first_new = out.described.len();
    for (ip, household) in found {
        let url = ssdp::description_url(ip);
        match t
            .http_get(&url)
            .and_then(|body| parse_device_description(&body))
        {
            Ok(desc) => out.described.push((ip, desc, household)),
            Err(e) => out.report.unreachable.push((ip.to_string(), e.to_string())),
        }
    }

    // Any player answers for its whole household; bridges answer too. A
    // stale first response can omit a discovered peer. Keep each response
    // coherent when that peer later supplies a newer, overlapping view.
    for (ip, desc, hint) in &out.described[first_new..] {
        if out
            .observations
            .iter()
            .any(|seen| seen.members.contains(&desc.udn))
        {
            continue;
        }
        let zgs = match get_zone_group_state(t, *ip) {
            Ok(zgs) => zgs,
            Err(e) => {
                out.report.unreachable.push((ip.to_string(), e.to_string()));
                continue;
            }
        };
        let members: HashSet<PlayerId> = zgs
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .flat_map(|m| std::iter::once(&m.uuid).chain(m.satellites.iter().map(|s| &s.uuid)))
            .cloned()
            .collect();
        let mut hints: HashSet<&String> = hint
            .iter()
            .chain(
                out.described
                    .iter()
                    .filter(|(_, player, _)| members.contains(&player.udn))
                    .filter_map(|(_, _, household)| household.as_ref()),
            )
            .collect();
        let identity_advertised = !hints.is_empty();
        // A current advertisement is authoritative. Without one, retain a
        // compatible remembered identity before adoption, so two partial
        // responses cannot acquire the same ID only after reconciliation.
        if hints.is_empty() {
            hints.extend(
                known
                    .iter()
                    .filter(|h| h.players.iter().any(|p| members.contains(&p.id)))
                    .filter_map(|h| h.id.as_ref().map(|id| &id.0)),
            );
        }
        if hints.len() > 1 {
            out.report.unreachable.push((ip.to_string(),
                "player answered, but its topology conflicts with household identity hints; this response was not adopted".into()));
            continue;
        }
        let mut state = HouseholdState::default();
        state.apply_topology(&zgs);
        state.id = hints.into_iter().next().cloned().map(HouseholdId);
        for (member_ip, member, _) in &out.described {
            if members.contains(&member.udn) {
                state.apply_description(member, *member_ip);
            }
        }
        state.players.sort_by(|a, b| a.room_name.cmp(&b.room_name));
        let observed = Observation {
            state,
            identity_advertised,
            members,
            boot_seqs: zgs.boot_seqs().map(|(p, seq)| (p.clone(), seq)).collect(),
            vanished: zgs.vanished,
        };
        if let Err(detail) = adopt_observation(&mut out.observations, observed) {
            out.report.unreachable.push((ip.to_string(), detail.into()));
        }
    }
    out
}

/// Retain per-response evidence until all discovery and fallback reads are
/// reconciled. Flattening it before fallback would lose which topology a
/// boot sequence or vanished-player report belongs to.
#[derive(Default)]
struct Observations {
    observations: Vec<Observation>,
    described: Vec<(IpAddr, DeviceDescription, Option<String>)>,
    report: Survey,
}

impl Observations {
    fn finish(mut self) -> Survey {
        let present: HashSet<_> = self
            .observations
            .iter()
            .flat_map(|seen| seen.members.iter().cloned())
            .collect();
        let mut vanished = HashSet::new();
        for observed in self.observations {
            self.report.households.push(observed.state);
            self.report.boot_seqs.extend(observed.boot_seqs);
            self.report.vanished.extend(
                observed
                    .vanished
                    .into_iter()
                    .filter(|v| !present.contains(&v.uuid) && vanished.insert(v.uuid.clone())),
            );
        }
        sort_households(&mut self.report.households);
        self.report
    }
}

/// The state and its evidence must come from the same accepted response.
/// Appending the old boot/vanished evidence after replacing its topology
/// would report an active player as missing or spuriously replay a reboot.
struct Observation {
    state: HouseholdState,
    /// Current discovery outranks a compatible identity retained from the
    /// previous model. Two current identities must never be collapsed.
    identity_advertised: bool,
    members: HashSet<PlayerId>,
    boot_seqs: Vec<(PlayerId, u32)>,
    vanished: Vec<VanishedDevice>,
}

fn adopt_observation(
    observations: &mut Vec<Observation>,
    mut fresh: Observation,
) -> Result<(), &'static str> {
    let overlaps: Vec<_> = observations
        .iter()
        .enumerate()
        .filter(|(_, old)| {
            fresh
                .state
                .id
                .as_ref()
                .is_some_and(|id| old.state.id.as_ref() == Some(id))
                || !fresh.members.is_disjoint(&old.members)
        })
        .map(|(index, _)| index)
        .collect();
    // A current advertisement can replace a stale cached identity. Resolve
    // all current identities first so the result does not depend on which
    // overlapping response happened to be inserted first.
    let advertised = fresh.identity_advertised
        || overlaps
            .iter()
            .any(|&index| observations[index].identity_advertised);
    let mut identity = None;
    for observed in std::iter::once(&fresh).chain(overlaps.iter().map(|&i| &observations[i])) {
        if advertised && !observed.identity_advertised {
            continue;
        }
        if let Some(id) = &observed.state.id {
            if identity.as_ref().is_some_and(|new| new != id) {
                return Err(
                    "player answered, but its topology overlaps distinct household identities; this conflicting response was not adopted",
                );
            }
            identity = Some(id.clone());
        }
    }
    fresh.state.id = identity;
    fresh.identity_advertised = advertised;
    let insert_at = overlaps.first().copied().unwrap_or(observations.len());
    let mut index = 0;
    observations.retain(|_| {
        let keep = !overlaps.contains(&index);
        index += 1;
        keep
    });
    observations.insert(insert_at, fresh);
    Ok(())
}

/// Survey a running model without mistaking a missed multicast reply for
/// a missing household. Only households absent from the fresh topology are
/// probed at their remembered addresses, once, without another discovery
/// window. A topology that does answer remains authoritative: its missing
/// members are not resurrected from the old model.
pub fn survey_known<T: Transport + ?Sized>(
    t: &T,
    seeds: &[IpAddr],
    known: &[HouseholdState],
    wait: Duration,
) -> Result<Survey, CoreError> {
    let first = survey_observations(t, seeds, known, wait);
    let mut attempted: HashSet<IpAddr> = seeds.iter().copied().collect();
    if let Ok(found) = &first {
        attempted.extend(
            found
                .observations
                .iter()
                .flat_map(|h| h.state.players.iter().map(|p| p.ip)),
        );
        attempted.extend(
            found
                .report
                .unreachable
                .iter()
                .filter_map(|(addr, _)| addr.parse::<IpAddr>().ok()),
        );
    }
    let mut fallback = Vec::new();
    for household in known {
        if first.as_ref().is_ok_and(|found| {
            found
                .observations
                .iter()
                .any(|h| same_household(&h.state, household))
        }) {
            continue;
        }
        for player in &household.players {
            if attempted.insert(player.ip) {
                fallback.push((player.ip, None));
            }
        }
    }
    if fallback.is_empty() {
        return first.map(Observations::finish);
    }
    let (out, error) = match first {
        Ok(found) => (found, None),
        Err(error) => (Observations::default(), Some(error)),
    };
    let mut out = read_players(t, fallback, known, out);
    if let Some(error) = error {
        if out.observations.is_empty() {
            return Err(error);
        }
        out.report.ssdp_error = Some(error.to_string());
    }
    Ok(out.finish())
}

fn same_household(a: &HouseholdState, b: &HouseholdState) -> bool {
    a.id.as_ref().is_some_and(|id| b.id.as_ref() == Some(id))
        || a.players.iter().any(|p| b.player(&p.id).is_some())
}

fn sort_households(households: &mut [HouseholdState]) {
    households.sort_by_key(|h| match h.generation() {
        Some(Generation::S1) => 0,
        Some(Generation::S2) => 1,
        None => 2,
    });
}

/// [`survey`], and when that finds no players, again with the
/// `remembered` addresses as seeds too: the players an earlier survey found,
/// which a network that drops multicast hides. The second try waits on no
/// SSDP or mDNS replies, it only reads those addresses. While discovery or
/// the seeds find the players, the remembered addresses are neither looked
/// up nor tried, so one that no longer answers costs nothing.
pub fn survey_or_remembered<T: Transport + ?Sized>(
    t: &T,
    seeds: &[IpAddr],
    remembered: impl FnOnce() -> Vec<IpAddr>,
    wait: Duration,
) -> Result<Survey, CoreError> {
    let first = survey(t, seeds, wait);
    if first.as_ref().is_ok_and(|s| !s.households.is_empty()) {
        return first;
    }
    let mut again = seeds.to_vec();
    for ip in remembered() {
        if !again.contains(&ip) {
            again.push(ip);
        }
    }
    if again.len() == seeds.len() {
        return first;
    }
    match survey(t, &again, Duration::ZERO) {
        Ok(found) if !found.households.is_empty() => Ok(found),
        _ => first,
    }
}

/// [`survey_or_remembered`], asking the players already known (the seeds
/// and the remembered ones) first, with no discovery window: a household's
/// topology names every member, so when every known player answers the
/// survey is done at once, and a command without the daemon answers in a
/// moment rather than after the SSDP wait. With none known, or one that no
/// longer answers (it moved, or is off), discovery runs as
/// [`survey_or_remembered`] does, since only it may find that player.
pub fn survey_known_first<T: Transport + ?Sized>(
    t: &T,
    seeds: &[IpAddr],
    remembered: impl FnOnce() -> Vec<IpAddr>,
    wait: Duration,
) -> Result<Survey, CoreError> {
    let mut known = seeds.to_vec();
    for ip in remembered() {
        if !known.contains(&ip) {
            known.push(ip);
        }
    }
    if !known.is_empty()
        && let Ok(found) = survey(t, &known, Duration::ZERO)
        && !found.households.is_empty()
        && found.unreachable.is_empty()
    {
        return Ok(found);
    }
    survey_or_remembered(t, seeds, || known, wait)
}

/// The generation a player's `swGen` (device description) or `SWGen`
/// (topology) value names.
#[must_use]
pub fn generation_for_sw_gen(sw_gen: u8) -> Option<Generation> {
    match sw_gen {
        1 => Some(Generation::S1),
        2 => Some(Generation::S2),
        _ => None,
    }
}

/// Hardware that can only ever run S1, by model number (not marketing name:
/// a Play:5 Gen 1 is `S5` but reports the same "Sonos Play:5" name as Gen 2).
#[must_use]
pub fn is_s1_only_hardware(model_number: &str) -> bool {
    const S1_ONLY: [&str; 10] = [
        "ZP80", "ZP90", "ZP100", "ZP120", // first-generation Connect / Connect:Amp
        "ZB100", "BR100", // Bridge
        "CR100", "CR200", // controllers
        "WD100", // iPod dock
        "S5",    // Play:5 Gen 1
    ];
    S1_ONLY
        .iter()
        .any(|m| m.eq_ignore_ascii_case(model_number.trim()))
}

/// Classify a player's generation. `swGen` is authoritative (S2-capable
/// hardware can still run S1, e.g. a Play:1 left in an S1 household); without
/// it, S1-only hardware is S1 and anything else is assumed S2.
#[must_use]
pub fn classify(desc: &DeviceDescription) -> Generation {
    desc.sw_gen.and_then(generation_for_sw_gen).unwrap_or(
        if is_s1_only_hardware(&desc.model_number) {
            Generation::S1
        } else {
            Generation::S2
        },
    )
}

impl HouseholdState {
    /// Fold a fetched device description (from the player at `ip`) into the
    /// model, filling in its model name and generation. Zone bridges are not
    /// renderers and are ignored. Returns whether the model changed.
    pub fn apply_description(&mut self, desc: &DeviceDescription, ip: IpAddr) -> bool {
        if !desc.is_renderer() {
            return false;
        }
        let generation = classify(desc);
        if let Some(p) = self.players.iter_mut().find(|p| p.id == desc.udn) {
            let before = p.clone();
            p.model.clone_from(&desc.model_name);
            p.generation = generation;
            p.ip = ip;
            if p.room_name.is_empty() {
                p.room_name.clone_from(&desc.room_name);
            }
            return *p != before;
        }
        self.players.push(Player {
            id: desc.udn.clone(),
            room_name: desc.room_name.clone(),
            ip,
            model: desc.model_name.clone(),
            generation,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(model_number: &str, sw_gen: Option<u8>) -> DeviceDescription {
        DeviceDescription {
            udn: fsonos_types::PlayerId("RINCON_X".into()),
            room_name: "Room".into(),
            display_name: None,
            model_name: "Sonos Thing".into(),
            model_number: model_number.into(),
            software_version: None,
            display_version: None,
            sw_gen,
            services: Vec::new(),
        }
    }

    #[test]
    fn sw_gen_is_authoritative() {
        assert_eq!(classify(&desc("S1", Some(1))), Generation::S1);
        assert_eq!(classify(&desc("S5", Some(2))), Generation::S2);
        assert_eq!(generation_for_sw_gen(3), None);
    }

    #[test]
    fn falls_back_to_s1_only_hardware() {
        assert_eq!(classify(&desc("S5", None)), Generation::S1);
        assert_eq!(classify(&desc("zb100", None)), Generation::S1);
        // Play:1 reports model number "S1" and is S2-capable.
        assert_eq!(classify(&desc("S1", None)), Generation::S2);
        assert_eq!(classify(&desc("S6", None)), Generation::S2);
        assert_eq!(classify(&desc("S50", None)), Generation::S2);
    }

    #[test]
    fn bridges_are_not_added() {
        let mut st = HouseholdState::default();
        assert!(!st.apply_description(&desc("ZB100", Some(1)), "192.0.2.1".parse().unwrap()));
        assert_eq!(st.players.len(), 0);
    }

    fn observation(identity: Option<&str>, members: &[&str]) -> Observation {
        Observation {
            state: HouseholdState {
                id: identity.map(|id| HouseholdId(id.into())),
                ..HouseholdState::default()
            },
            identity_advertised: false,
            members: members.iter().map(|id| PlayerId((*id).into())).collect(),
            boot_seqs: Vec::new(),
            vanished: Vec::new(),
        }
    }

    #[test]
    fn an_anonymous_overlap_does_not_bridge_distinct_known_households() {
        let mut observations = vec![
            observation(Some("household-one"), &["one", "shared-one"]),
            observation(Some("household-two"), &["two", "shared-two"]),
        ];
        let error = adopt_observation(
            &mut observations,
            observation(None, &["shared-one", "shared-two", "three"]),
        )
        .unwrap_err();
        assert!(error.contains("overlaps distinct household identities"));
        assert_eq!(observations.len(), 2);
        assert_eq!(
            observations[0].state.id,
            Some(HouseholdId("household-one".into()))
        );
        assert_eq!(
            observations[1].state.id,
            Some(HouseholdId("household-two".into()))
        );
        assert!(
            observations
                .iter()
                .all(|o| !o.members.contains(&PlayerId("three".into())))
        );
    }

    #[test]
    fn one_known_identity_survives_a_newer_anonymous_full_observation() {
        let mut observations = vec![
            observation(Some("household-one"), &["one"]),
            observation(None, &["two"]),
        ];
        adopt_observation(
            &mut observations,
            observation(None, &["one", "two", "three"]),
        )
        .unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].state.id,
            Some(HouseholdId("household-one".into()))
        );
        assert_eq!(observations[0].members.len(), 3);
    }

    #[test]
    fn disjoint_anonymous_observations_are_not_assumed_to_be_one_household() {
        let mut observations = Vec::new();
        adopt_observation(&mut observations, observation(None, &["one"])).unwrap();
        adopt_observation(&mut observations, observation(None, &["two"])).unwrap();
        assert_eq!(observations.len(), 2);
    }
}
