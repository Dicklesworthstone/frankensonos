//! Device inventory: turning raw discovery results into a [`HouseholdState`].
//!
//! SSDP (and the direct-seed fallback) yields device-description URLs; this
//! module classifies each description as S1 or S2 and folds it into the
//! model. [`survey`] runs the whole pass over a [`Transport`];
//! classification is pure.

use crate::{CoreError, HouseholdState};
use fsonos_proto::description::{DeviceDescription, parse_device_description};
use fsonos_proto::topology::{get_zone_group_state, host_of_location};
use fsonos_proto::{Transport, ssdp};
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
    /// Set when SSDP itself failed and only the seeds were tried.
    pub ssdp_error: Option<String>,
    /// Each player's `BootSeq` from the topology reads (it rises on every
    /// boot, so a reboot between surveys shows here).
    pub boot_seqs: Vec<(PlayerId, u32)>,
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
    let mut out = Survey::default();
    let mut found: Vec<(IpAddr, Option<String>)> = Vec::new();
    match t.ssdp_search(1, wait) {
        Ok(adverts) => {
            for advert in adverts {
                if let Some(ip) = host_of_location(&advert.location)
                    && !found.iter().any(|(seen, _)| *seen == ip)
                {
                    found.push((ip, advert.household));
                }
            }
        }
        Err(e) if !seeds.is_empty() => out.ssdp_error = Some(e.to_string()),
        Err(e) => return Err(e.into()),
    }
    for &ip in seeds {
        if !found.iter().any(|(seen, _)| *seen == ip) {
            found.push((ip, None));
        }
    }

    let mut described: Vec<(IpAddr, DeviceDescription, Option<String>)> = Vec::new();
    for (ip, household) in found {
        let url = ssdp::description_url(ip);
        match t
            .http_get(&url)
            .and_then(|body| parse_device_description(&body))
        {
            Ok(desc) => described.push((ip, desc, household)),
            Err(e) => out.unreachable.push((ip.to_string(), e.to_string())),
        }
    }

    // Any player answers ZoneGroupTopology for its whole household, so ask
    // one player per household; bridges answer too.
    let mut covered: HashSet<PlayerId> = HashSet::new();
    for (ip, desc, _) in &described {
        if covered.contains(&desc.udn) {
            continue;
        }
        let zgs = match get_zone_group_state(t, *ip) {
            Ok(zgs) => zgs,
            Err(e) => {
                out.unreachable.push((ip.to_string(), e.to_string()));
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
        out.boot_seqs
            .extend(zgs.boot_seqs().map(|(p, seq)| (p.clone(), seq)));
        let mut state = HouseholdState::default();
        state.apply_topology(&zgs);
        for (member_ip, member, household) in &described {
            if members.contains(&member.udn) {
                state.apply_description(member, *member_ip);
                if state.id.is_none() {
                    state.id = household.clone().map(HouseholdId);
                }
            }
        }
        covered.extend(members);
        covered.insert(desc.udn.clone());
        state.players.sort_by(|a, b| a.room_name.cmp(&b.room_name));
        out.households.push(state);
    }
    out.households.sort_by_key(|h| match h.generation() {
        Some(Generation::S1) => 0,
        Some(Generation::S2) => 1,
        None => 2,
    });
    Ok(out)
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
}
