//! Search and play one owned selection, not a second lookup of its display ID.
//! A favorite's URI and DIDL belong to the household that supplied the hit.
//! Playback still uses the shared guarded path for caps, history, log and undo.

use fsonos_core::favorites::{self, Favorite};
use fsonos_core::policy::Client;
use fsonos_core::search::{self, HitSource};

use super::{Surface, resolve, room_view};
use crate::execute::OutcomeDto;
use crate::failure::{ErrorCode, Failure};
use crate::plan::{plan_play, plan_play_favorite};
use crate::request::{PlayFavoriteRequest, PlayRequest, SearchPlayRequest};

impl Surface {
    /// Search once, then play the selected result in the addressed player's
    /// group. A favorite list or room alias changing during the operation
    /// cannot silently substitute different music or another addressed room.
    pub fn search_and_play(
        &self,
        client: &Client,
        req: &SearchPlayRequest,
    ) -> Result<OutcomeDto, Failure> {
        let zone = req.zone()?;
        let query = req.query()?;
        let pick = req.pick()?;
        let guard = self.guard(client);
        guard.authorize("search_library", true)?;
        // Do not survey or read speakers for a caller with no playback
        // permission. The selected operation is authorized again by control;
        // permission for one kind of result never grants the other kind.
        if let (Err(denied), Err(_)) = (
            guard.authorize("play", false),
            guard.authorize("play_favorite", false),
        ) {
            self.record(
                client,
                "search_and_play".into(),
                format!("deny: {}", denied.detail),
                denied.detail.clone(),
                None,
            );
            return Err(denied);
        }
        let (library, playlists) = self
            .with_store(|store| Ok((store.library()?, store.playlists()?)))?
            .unwrap_or_default();
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|failure| self.explain(failure))?;
        let player = target.player.id.clone();
        let generation = target.player.generation;
        let household = households
            .iter()
            .find(|house| house.player(&player).is_some())
            .and_then(|house| house.id.clone());
        let favorites = favorites::list(&*self.transport, &households, &target.coordinator.id)
            .map_err(Failure::from)
            .inspect_err(|failure| self.notice(failure))?;
        let hits = search::search_all(&library, &playlists, &favorites, query, pick);
        let hit = hits.get(pick - 1).ok_or_else(|| {
            Failure::new(
                ErrorCode::NoMatch,
                format!(
                    "only {} result(s) matched {query:?}; cannot play result {pick}",
                    hits.len()
                ),
            )
            .with_hint(
                "Search the library to inspect the current matches, or choose a smaller pick.",
            )
        })?;

        match &hit.source {
            HitSource::Library { source_uri }
            | HitSource::Album { source_uri }
            | HitSource::Playlist { source_uri } => {
                let play = PlayRequest {
                    zone: player.0,
                    source_uri: source_uri.clone(),
                    title: Some(hit.title.clone()),
                };
                self.control(client, "play", |rooms| plan_play(rooms, &play))
            }
            HitSource::Favorite { id } => {
                let favorite = selected_favorite(&favorites, id)?;
                self.control(client, "play_favorite", |rooms| {
                    let current = resolve(rooms, &player.0)?;
                    let current_household = rooms
                        .households
                        .iter()
                        .find(|house| house.player(&player).is_some())
                        .and_then(|house| house.id.as_ref());
                    if current.player.generation != generation
                        || household.as_ref().is_some_and(|id| Some(id) != current_household)
                    {
                        return Err(Failure::new(
                            ErrorCode::NotReady,
                            "the room changed household after the favorite was selected",
                        )
                        .with_hint("Search again in the room's current household."));
                    }
                    // FV:2 IDs can be reused. Never fetch this ID again: the
                    // selected URI and metadata are already owned by this call.
                    plan_play_favorite(
                        rooms,
                        &PlayFavoriteRequest {
                            zone: player.0.clone(),
                            favorite: id.clone(),
                        },
                        std::slice::from_ref(favorite),
                    )
                })
            }
        }
    }
}

fn selected_favorite<'a>(favorites: &'a [Favorite], id: &str) -> Result<&'a Favorite, Failure> {
    let mut matching = favorites.iter().filter(|favorite| favorite.id == id);
    match (matching.next(), matching.next()) {
        (Some(favorite), None) => Ok(favorite),
        _ => Err(Failure::new(
            ErrorCode::NotReady,
            "the search observation has no unique favorite for the selected ID",
        )
        .with_hint("Read the household's favorites again before choosing one.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::favorites::FavoriteKind;

    #[test]
    fn default_pick_is_first_and_bounds_are_shared_with_search() {
        let best = SearchPlayRequest {
            zone: "Room".into(),
            query: "Goldberg".into(),
            pick: None,
        };
        assert_eq!(best.pick().unwrap(), 1);
        for pick in [0, crate::request::MAX_SEARCH_RESULTS + 1] {
            let request = SearchPlayRequest {
                pick: Some(pick),
                ..best.clone()
            };
            assert!(request.pick().is_err());
        }
    }

    fn favorite(uri: &str) -> Favorite {
        Favorite {
            id: "FV:2/1".into(),
            title: "Radio".into(),
            kind: FavoriteKind::Stream,
            uri: Some(uri.into()),
            metadata: "original metadata".into(),
            description: None,
            art_uri: None,
        }
    }

    #[test]
    fn selection_keeps_the_complete_favorite_not_just_its_title() {
        let entries = [favorite("x-rincon-mp3radio://example.invalid/original.mp3")];
        assert_eq!(selected_favorite(&entries, "FV:2/1").unwrap(), &entries[0]);
    }

    #[test]
    fn missing_or_duplicate_ids_cannot_substitute_a_favorite() {
        let entries = [favorite("first"), favorite("second")];
        assert!(selected_favorite(&entries, "FV:2/1").is_err());
        assert!(selected_favorite(&entries, "FV:2/2").is_err());
        assert!(selected_favorite(&[], "FV:2/1").is_err());
    }
}
