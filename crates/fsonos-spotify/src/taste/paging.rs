//! Bounded taste reads keep useful prefixes, but never mistake a page budget
//! or a broken continuation for a complete account snapshot.

use super::{MAX_PAGES, Reader};
use crate::SpotifyError;
use crate::client::{Artist, Endpoints, FollowedArtists, parse_query};
use std::collections::HashSet;

mod coverage;

use coverage::{Coverage, Page};

struct Chain {
    path: String,
    progress: Progress,
    // Cursor pagination must not silently switch the followed resource type.
    resource_type: Option<String>,
}

enum Progress {
    Offset(u32),
    Cursor(HashSet<String>),
}

fn path(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

fn parameter(url: &str, name: &str) -> Result<String, SpotifyError> {
    let query = url.split_once('?').map_or("", |(_, query)| query);
    let mut found = None;
    for (key, value) in parse_query(query)? {
        if key == name && found.replace(value).is_some() {
            return Err(SpotifyError::Decode(
                "duplicate taste paging position".into(),
            ));
        }
    }
    found
        .filter(|value| !value.is_empty())
        .ok_or_else(|| SpotifyError::Decode("taste paging link has no position".into()))
}

impl Chain {
    fn new(url: &str, progress: Progress) -> Self {
        Self {
            path: path(url).to_owned(),
            resource_type: if matches!(progress, Progress::Cursor(_)) {
                parameter(url, "type").ok()
            } else {
                None
            },
            progress,
        }
    }

    fn follow(&mut self, endpoints: &Endpoints, url: &str) -> Result<(), SpotifyError> {
        if !endpoints.is_api_url(url) || path(url) != self.path || url.contains('#') {
            return Err(SpotifyError::Decode(
                "taste paging link changed collection".into(),
            ));
        }
        match &mut self.progress {
            Progress::Offset(previous) => {
                let next = parameter(url, "offset")?
                    .parse::<u32>()
                    .map_err(|_| SpotifyError::Decode("invalid taste paging offset".into()))?;
                if next <= *previous {
                    return Err(SpotifyError::Decode("taste paging did not advance".into()));
                }
                *previous = next;
            }
            Progress::Cursor(seen) => {
                if self.resource_type.as_deref() != parameter(url, "type").ok().as_deref() {
                    return Err(SpotifyError::Decode(
                        "taste cursor changed the followed resource type".into(),
                    ));
                }
                if !seen.insert(parameter(url, "after")?) {
                    return Err(SpotifyError::Decode(
                        "taste paging repeated a cursor".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Reader<'_> {
    fn continuation(
        &mut self,
        chain: &mut Chain,
        next: Option<String>,
        what: &str,
    ) -> Option<String> {
        let url = next?;
        if let Err(error) = chain.follow(self.session.endpoints(), &url) {
            self.read.failed.push((what.to_owned(), error.to_string()));
            return None;
        }
        Some(url)
    }

    fn exhausted(&mut self, has_next: bool, what: &str) {
        if has_next {
            self.read.failed.push((
                what.to_owned(),
                "taste page budget reached before the collection ended".into(),
            ));
        }
    }

    /// Keep accepted pages after a failure, but report the read as incomplete.
    /// Every production caller starts at offset zero, including top lists
    /// whose initial URL leaves the default offset implicit.
    pub(super) async fn page<T: serde::de::DeserializeOwned>(
        &mut self,
        url: &str,
        what: &str,
        pages: usize,
    ) -> Vec<T> {
        let mut items = Vec::new();
        let mut coverage = Coverage::default();
        let mut chain = Chain::new(url, Progress::Offset(0));
        let mut next = Some(url.to_owned());
        for _ in 0..pages {
            let Some(url) = next.take() else {
                break;
            };
            let Some(page) = self.get::<Page<T>>(&url, what).await else {
                break;
            };
            if let Err(error) = coverage.accept(&page) {
                self.read.failed.push((what.to_owned(), error.to_string()));
                break;
            }
            items.extend(page.items);
            next = self.continuation(&mut chain, page.next, what);
        }
        self.exhausted(next.is_some(), what);
        items
    }

    pub(super) async fn followed(&mut self) -> Vec<Artist> {
        let mut artists = Vec::new();
        let first = self.session.endpoints().followed_artists();
        let mut chain = Chain::new(&first, Progress::Cursor(HashSet::new()));
        let mut next = Some(first);
        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else {
                break;
            };
            let Some(page) = self.get::<FollowedArtists>(&url, "followed artists").await else {
                break;
            };
            artists.extend(page.artists.items);
            next = self.continuation(&mut chain, page.artists.next, "followed artists");
        }
        self.exhausted(next.is_some(), "followed artists");
        artists
    }
}

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;

#[cfg(test)]
mod cursor_identity_tests {
    use super::*;

    #[test]
    fn followed_cursor_cannot_switch_resource_type() {
        let endpoints = Endpoints::default();
        let first = endpoints.followed_artists();
        let mut chain = Chain::new(&first, Progress::Cursor(HashSet::new()));
        let base = path(&first);
        assert!(
            chain
                .follow(&endpoints, &format!("{base}?type=user&after=one"))
                .is_err()
        );
        assert!(
            chain
                .follow(&endpoints, &format!("{base}?after=one"))
                .is_err()
        );
        assert!(
            chain
                .follow(&endpoints, &format!("{base}?type=artist&after=one"))
                .is_ok()
        );
        assert!(
            chain
                .follow(&endpoints, &format!("{base}?type=artist&after=one"))
                .is_err()
        );
    }
}
