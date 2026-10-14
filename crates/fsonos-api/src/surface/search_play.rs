//! Search and immediately play one ranked result through the existing guarded
//! playback paths. Search remains read-only; the selected hit is then handled
//! by `play` or `play_favorite`, preserving policy, logging, undo, history,
//! coordinator resolution and household-specific Spotify rendering.

use fsonos_core::policy::Client;

use super::Surface;
use crate::execute::OutcomeDto;
use crate::failure::{ErrorCode, Failure};
use crate::plan::plan_play;
use crate::request::{PlayFavoriteRequest, PlayRequest, SearchPlayRequest, SearchRequest};

impl Surface {
    /// Search the owner's library plus the target household's favorites and
    /// play the requested ranked hit (1-based, default 1).
    pub fn search_and_play(
        &self,
        client: &Client,
        req: &SearchPlayRequest,
    ) -> Result<OutcomeDto, Failure> {
        let zone = req.zone()?.to_owned();
        let query = req.query()?.to_owned();
        let pick = req.pick()?;
        let hits = self.search_library(
            client,
            &SearchRequest {
                query: query.clone(),
                zone: Some(zone.clone()),
                limit: Some(pick),
            },
        )?;
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

        match (&hit.source_uri, &hit.favorite) {
            (Some(source_uri), _) => {
                let play = PlayRequest {
                    zone,
                    source_uri: source_uri.clone(),
                    title: Some(hit.title.clone()),
                };
                self.control(client, "play", |rooms| plan_play(rooms, &play))
            }
            (None, Some(favorite)) => self.play_favorite(
                client,
                &PlayFavoriteRequest {
                    zone,
                    favorite: favorite.clone(),
                },
            ),
            (None, None) => Err(Failure::new(
                ErrorCode::Internal,
                "the selected search result has nothing playable",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::request::SearchPlayRequest;

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
}
