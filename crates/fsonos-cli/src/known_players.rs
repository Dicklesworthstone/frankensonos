//! The players a survey found, remembered in the store for the next run:
//! when discovery and the seeds find no players (a network that filters
//! multicast), the survey tries the addresses each household's latest survey
//! found (see `fsonos_core::inventory::survey_or_remembered`). Only then is
//! the store read, and an address that no longer answers tried.

use fsonos_core::HouseholdState;
use fsonos_core::live::Surveyed;
use fsonos_core::store::{SqliteStore, Store};
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
/// household found only through seeds has no id: its players are kept under
/// "", its groups not at all (two such households would overwrite each
/// other's).
pub fn remember(store: &mut dyn Store, households: &[HouseholdState], now: i64) {
    let saved = households.iter().try_for_each(|h| match &h.id {
        Some(id) => {
            store.save_players(&id.0, &h.players, now)?;
            store.save_groups(&id.0, &h.groups, now)
        }
        None => store.save_players("", &h.players, now),
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
    let store = match open(&global.data_dir()?) {
        Ok(store) => Mutex::new(store),
        Err(e) => {
            tracing::warn!("the players found will not be remembered: the store: {e}");
            return None;
        }
    };
    Some(Arc::new(move |households: &[HouseholdState]| {
        let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
        remember(&mut *store, households, now());
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
}
