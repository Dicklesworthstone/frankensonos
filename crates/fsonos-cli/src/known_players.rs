//! The players a survey found, remembered in the store for the next run:
//! when discovery and the seeds find no players (a network that filters
//! multicast), the survey tries the addresses each household's latest survey
//! found (see `fsonos_core::inventory::survey_or_remembered`). Only then is
//! the store read, and an address that no longer answers tried.

use fsonos_core::HouseholdState;
use fsonos_core::live::Surveyed;
use fsonos_core::store::{SqliteStore, Store};
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::GlobalArgs;
use crate::daemon::DB_FILE;

/// The players remembered in `data_dir`'s store (none when there is no
/// store yet).
pub fn remembered(data_dir: Option<&Path>) -> Vec<IpAddr> {
    let Some(db) = data_dir.map(|d| d.join(DB_FILE)) else {
        return Vec::new();
    };
    if !db.exists() {
        return Vec::new();
    }
    match SqliteStore::open(&db) {
        Ok(store) => addresses(&store),
        Err(e) => {
            tracing::warn!("no remembered players: the store: {e}");
            Vec::new()
        }
    }
}

/// The addresses of the players each household's latest survey found, without
/// duplicates. A player that survey missed (moved to another address, or
/// gone) keeps its row, but it is not tried again.
fn addresses(store: &dyn Store) -> Vec<IpAddr> {
    let players = match store.cached_players() {
        Ok(players) => players,
        Err(e) => {
            tracing::warn!("no remembered players: {e}");
            return Vec::new();
        }
    };
    let latest = |household: &str| {
        players
            .iter()
            .filter(|p| p.household == household)
            .map(|p| p.last_seen)
            .max()
    };
    let mut found: Vec<IpAddr> = Vec::new();
    for p in &players {
        if Some(p.last_seen) == latest(&p.household) && !found.contains(&p.player.ip) {
            found.push(p.player.ip);
        }
    }
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
    let saved = households.iter().try_for_each(|h| match &h.id {
        Some(id) => {
            store.save_players(&id.0, &h.players, now)?;
            store.save_groups(&id.0, &h.groups, now)
        }
        None => {
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
            addresses(&store),
            ["192.0.2.10", "192.0.2.11"].map(|a| a.parse::<IpAddr>().unwrap())
        );

        // The next survey finds one player at a new address and not the
        // other: only what it found is tried from then on.
        let moved = HouseholdState {
            players: vec![player("RINCON_000000000000001400", "192.0.2.20")],
            ..house.clone()
        };
        remember(&mut store, &[moved], 200);
        assert_eq!(addresses(&store), ["192.0.2.20".parse::<IpAddr>().unwrap()]);

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
        assert_eq!(addresses(&store).len(), 2);
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
        let remembered = addresses(&store);
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
}
