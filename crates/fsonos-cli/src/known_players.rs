//! Discovery hints retained across CLI and daemon restarts. Cached players
//! identify which households to probe. Only a fresh survey supplies live rooms.

use fsonos_core::HouseholdState;
use fsonos_core::live::Surveyed;
use fsonos_core::store::{SqliteStore, Store};
use fsonos_types::HouseholdId;
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::GlobalArgs;
use crate::daemon::DB_FILE;

/// Household and member identities for a full discovery pass. Missing stores
/// are normal on the first run.
pub fn remembered_households(data_dir: Option<&Path>) -> Vec<HouseholdState> {
    let Some(db) = data_dir.map(|d| d.join(DB_FILE)) else {
        return Vec::new();
    };
    if !db.exists() {
        return Vec::new();
    }
    match SqliteStore::open(&db) {
        Ok(store) => cached_households(&store),
        Err(e) => {
            tracing::warn!("no remembered players: the store: {e}");
            Vec::new()
        }
    }
}

/// Known-first surveys try the current cached addresses without multicast.
pub fn remembered(data_dir: Option<&Path>) -> Vec<IpAddr> {
    addresses(remembered_households(data_dir))
}

fn addresses(households: Vec<HouseholdState>) -> Vec<IpAddr> {
    let mut found = Vec::new();
    for player in households.into_iter().flat_map(|h| h.players) {
        if !found.contains(&player.ip) {
            found.push(player.ip);
        }
    }
    found
}

fn cached_households(store: &dyn Store) -> Vec<HouseholdState> {
    let players = match store.cached_players() {
        Ok(players) => players,
        Err(e) => {
            tracing::warn!("no remembered players: {e}");
            return Vec::new();
        }
    };
    let mut latest = HashMap::new();
    for row in &players {
        let stamp = latest
            .entry(row.household.as_str())
            .or_insert(row.last_seen);
        *stamp = (*stamp).max(row.last_seen);
    }
    let mut identified = BTreeMap::<&str, HouseholdState>::new();
    let mut found = Vec::new();
    for row in &players {
        if row.household.is_empty() {
            // These players may belong to different households. A fresh sighting
            // of one UUID must not suppress probes for the others.
            found.push(HouseholdState {
                players: vec![row.player.clone()],
                ..HouseholdState::default()
            });
        } else if latest.get(row.household.as_str()) == Some(&row.last_seen) {
            let house = identified
                .entry(&row.household)
                .or_insert_with(|| HouseholdState {
                    id: Some(HouseholdId(row.household.clone())),
                    ..HouseholdState::default()
                });
            house.players.push(row.player.clone());
        }
    }
    found.extend(identified.into_values());
    found
}

/// Remember `households`' players and groups in `store`, stamped `now`. A
/// household found only through seeds has no id: known players retain their
/// cached household identity, and newly found players are kept under "".
/// Anonymous observations do not replace groups (two such households would
/// overwrite each other's). Explicit current identities remain authoritative.
pub fn remember(store: &mut dyn Store, households: &[HouseholdState], now: i64) {
    let mut identities = if households.iter().any(|h| h.id.is_none()) {
        match store.cached_players() {
            Ok(players) => players
                .into_iter()
                .map(|p| (p.player.id, p.household))
                .collect::<HashMap<_, _>>(),
            Err(e) => {
                tracing::warn!("the players found were not remembered: cached identities: {e}");
                return;
            }
        }
    } else {
        HashMap::new()
    };
    for household in households {
        if let Some(id) = &household.id {
            for player in &household.players {
                identities.insert(player.id.clone(), id.0.clone());
            }
        }
    }
    let saved = households.iter().try_for_each(|h| {
        if let Some(id) = &h.id {
            store.save_players(&id.0, &h.players, now)?;
            store.save_groups(&id.0, &h.groups, now)
        } else {
            // Keep writes batched by identity instead of one transaction
            // per player; a known household can contain newly found players.
            let mut by_household = BTreeMap::<&str, Vec<_>>::new();
            for player in &h.players {
                let id = identities.get(&player.id).map_or("", String::as_str);
                by_household.entry(id).or_default().push(player.clone());
            }
            by_household
                .into_iter()
                .try_for_each(|(id, players)| store.save_players(id, &players, now))
        }
    });
    if let Err(e) = saved {
        tracing::warn!("the players found were not remembered: {e}");
    }
}

/// Remember `households` in the data directory's store.
pub fn remember_in(data_dir: &Path, households: &[HouseholdState]) {
    match open(data_dir) {
        Ok(mut store) => remember(&mut store, households, now()),
        Err(e) => tracing::warn!("the players found were not remembered: the store: {e}"),
    }
}

/// What the live model calls after each survey that found the households
/// (see `fsonos_core::live::LiveConfig::surveyed`): it remembers them in the
/// data directory's store, kept open. `None` without a store.
pub fn remembering(global: &GlobalArgs) -> Option<Surveyed> {
    let store: Box<dyn Store + Send> = match open(&global.data_dir()?) {
        Ok(store) => Box::new(store),
        Err(e) => {
            tracing::warn!("the players found will not be remembered: the store: {e}");
            return None;
        }
    };
    let store = Mutex::new(store);
    Some(Arc::new(move |households: &[HouseholdState]| {
        let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
        remember(&mut **store, households, now());
    }))
}

/// The data directory's store, the directory made first if need be.
fn open(data_dir: &Path) -> Result<SqliteStore, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    SqliteStore::open(&data_dir.join(DB_FILE)).map_err(|e| e.to_string())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::store::MemStore;
    use fsonos_types::{Generation, HouseholdId, Player, PlayerId, ZoneGroup};

    fn player(id: &str, ip: &str) -> Player {
        Player {
            id: PlayerId(id.into()),
            room_name: "Living Room".into(),
            ip: ip.parse().unwrap(),
            model: "Sonos One".into(),
            generation: Generation::S2,
        }
    }

    #[test]
    fn the_players_a_survey_found_are_remembered_with_their_grouping() {
        let mut store = MemStore::default();
        let house = HouseholdState {
            id: Some(HouseholdId("Sonos_example".into())),
            players: vec![
                player("RINCON_000000000000001400", "192.0.2.10"),
                player("RINCON_000000000000011400", "192.0.2.11"),
            ],
            groups: vec![ZoneGroup {
                coordinator: PlayerId("RINCON_000000000000001400".into()),
                members: vec![PlayerId("RINCON_000000000000011400".into())],
            }],
            ..HouseholdState::default()
        };
        remember(&mut store, std::slice::from_ref(&house), 100);
        assert_eq!(
            store.cached_groups("Sonos_example").unwrap(),
            house.groups,
            "the grouping too"
        );
        assert_eq!(
            addresses(cached_households(&store)),
            ["192.0.2.10", "192.0.2.11"].map(|a| a.parse::<IpAddr>().unwrap())
        );

        // The next survey finds one player at a new address and not the
        // other: only what it found is tried from then on.
        let moved = HouseholdState {
            players: vec![player("RINCON_000000000000001400", "192.0.2.20")],
            ..house.clone()
        };
        remember(&mut store, &[moved], 200);
        assert_eq!(
            addresses(cached_households(&store)),
            ["192.0.2.20".parse::<IpAddr>().unwrap()]
        );

        // Two households found only through seeds (no id): both remembered,
        // and no group of either saved under "".
        let seeded = |id: &str, ip: &str| HouseholdState {
            players: vec![player(id, ip)],
            groups: vec![ZoneGroup {
                coordinator: PlayerId(id.into()),
                members: Vec::new(),
            }],
            ..HouseholdState::default()
        };
        let mut store = MemStore::default();
        remember(
            &mut store,
            &[
                seeded("RINCON_000000000000021400", "192.0.2.30"),
                seeded("RINCON_000000000000031400", "192.0.2.31"),
            ],
            300,
        );
        assert_eq!(addresses(cached_households(&store)).len(), 2);
        assert!(store.cached_groups("").unwrap().is_empty());
    }

    #[test]
    fn seed_only_addresses_survive_a_store_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "fsonos-remembered-seeds-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let houses = [
            HouseholdState {
                players: vec![player("seed-one", "192.0.2.30")],
                ..HouseholdState::default()
            },
            HouseholdState {
                players: vec![player("seed-two", "192.0.2.31")],
                ..HouseholdState::default()
            },
        ];
        let mut store = open(&dir).unwrap();
        remember(&mut store, &houses, 100);
        drop(store);
        assert_eq!(
            remembered(Some(&dir)),
            ["192.0.2.30", "192.0.2.31"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn anonymous_refresh_updates_addresses_without_erasing_known_household_ids() {
        let mut store = MemStore::default();
        let first = HouseholdState {
            id: Some(HouseholdId("household-one".into())),
            players: vec![player("one", "192.0.2.10"), player("two", "192.0.2.11")],
            groups: vec![ZoneGroup {
                coordinator: PlayerId("one".into()),
                members: vec![PlayerId("one".into()), PlayerId("two".into())],
            }],
            ..HouseholdState::default()
        };
        let second = HouseholdState {
            id: Some(HouseholdId("household-two".into())),
            players: vec![player("three", "192.0.2.20")],
            ..HouseholdState::default()
        };
        remember(&mut store, &[first.clone(), second], 100);
        let anonymous = [
            HouseholdState {
                players: vec![player("one", "192.0.2.12"), player("new", "192.0.2.30")],
                ..HouseholdState::default()
            },
            HouseholdState {
                players: vec![player("three", "192.0.2.21")],
                ..HouseholdState::default()
            },
        ];
        remember(&mut store, &anonymous, 200);
        let rows = store.cached_players().unwrap();
        for (id, household, ip) in [
            ("one", "household-one", "192.0.2.12"),
            ("three", "household-two", "192.0.2.21"),
            ("new", "", "192.0.2.30"),
        ] {
            let cached = rows.iter().find(|p| p.player.id.0 == id).unwrap();
            assert_eq!(cached.household, household);
            assert_eq!(cached.player.ip, ip.parse::<IpAddr>().unwrap());
            assert_eq!(cached.last_seen, 200);
        }
        assert_eq!(store.cached_groups("household-one").unwrap(), first.groups);
        let remembered = addresses(cached_households(&store));
        assert_eq!(
            remembered.len(),
            3,
            "the missing old address is not retried"
        );
        assert!(!remembered.contains(&"192.0.2.11".parse().unwrap()));

        let reassigned = HouseholdState {
            id: Some(HouseholdId("household-new".into())),
            players: vec![player("one", "192.0.2.13")],
            ..HouseholdState::default()
        };
        remember(&mut store, &[reassigned], 300);
        let rows = store.cached_players().unwrap();
        let moved = rows.iter().find(|p| p.player.id.0 == "one").unwrap();
        assert_eq!(moved.household, "household-new", "an explicit ID wins");
        assert_eq!(moved.player.ip, "192.0.2.13".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn a_partial_anonymous_survey_keeps_other_households_retryable() {
        let mut store = MemStore::default();
        let first = HouseholdState {
            players: vec![player("anonymous-one", "192.0.2.30")],
            ..HouseholdState::default()
        };
        let second = HouseholdState {
            players: vec![player("anonymous-two", "192.0.2.31")],
            ..HouseholdState::default()
        };
        let identified = HouseholdState {
            id: Some(HouseholdId("known-household".into())),
            players: vec![
                player("known-one", "192.0.2.40"),
                player("known-two", "192.0.2.41"),
            ],
            ..HouseholdState::default()
        };
        remember(&mut store, &[first, second, identified], 100);

        // One anonymous household does not answer. The other answers at a
        // new address; the identified household also loses one member.
        let refreshed = [
            HouseholdState {
                players: vec![player("anonymous-one", "192.0.2.32")],
                ..HouseholdState::default()
            },
            HouseholdState {
                id: Some(HouseholdId("known-household".into())),
                players: vec![player("known-one", "192.0.2.42")],
                ..HouseholdState::default()
            },
        ];
        remember(&mut store, &refreshed, 200);
        let mut kept = addresses(cached_households(&store));
        kept.sort();
        assert_eq!(
            kept,
            ["192.0.2.31", "192.0.2.32", "192.0.2.42"].map(|ip| ip.parse::<IpAddr>().unwrap()),
            "an absent anonymous household stays discoverable; a known household's old member does not"
        );

        // The other anonymous household can be found on the next run,
        // without forgetting the first or retaining its own old address.
        let returned = HouseholdState {
            players: vec![player("anonymous-two", "192.0.2.33")],
            ..HouseholdState::default()
        };
        remember(&mut store, &[returned], 300);
        let mut kept = addresses(cached_households(&store));
        kept.sort();
        assert_eq!(
            kept,
            ["192.0.2.32", "192.0.2.33", "192.0.2.42"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
}
