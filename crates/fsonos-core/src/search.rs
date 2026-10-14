//! Library search: rank the owner's cached Spotify library (its tracks,
//! saved albums and playlists) and the Sonos favorites against what a person
//! or agent typed ("goldberg gould", "bwv 988", "dvorak 9", "abbey road",
//! "sunday morning").
//!
//! Every query word must match somewhere in a result (title, artists, album,
//! or a favorite's description). A word matches a field word exactly, as a
//! prefix, or, for words of five letters or more, with one typo. Matches in
//! the title count most, then the artists (the composer is usually first),
//! then the album. A catalog number the query names (BWV 988, Op. 67, K. 525)
//! that appears in the title is a strong boost. Case, accents and punctuation
//! are ignored. A saved album is a result of its own (its title, then its
//! artists), after the tracks and favorites it ties with, so a query naming
//! an album rather than a track plays the album; so is each playlist in the
//! owner's list (by its name), after everything else it ties with. Pure:
//! the caller supplies the library, playlists and favorites.

use crate::favorites::Favorite;
use crate::store::{LibraryEntry, LibraryOrigin};
use fsonos_types::text::normalize;
use std::collections::HashSet;

/// Where a result came from, and how to play it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitSource {
    /// A library track: its `spotify:track:<id>` URI.
    Library { source_uri: String },
    /// A Sonos favorite: its `FV:2/<n>` id.
    Favorite { id: String },
    /// An album the owner saved: its `spotify:album:<id>` URI.
    Album { source_uri: String },
    /// A playlist in the owner's list: its `spotify:playlist:<id>` URI.
    Playlist { source_uri: String },
}

/// One ranked result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub source: HitSource,
    pub title: String,
    /// Artists for a track or album, the description for a favorite.
    pub subtitle: Option<String>,
    pub score: u32,
}

/// Catalog prefixes whose following number identifies a work.
const CATALOG: [&str; 14] = [
    "bwv", "op", "k", "kv", "hob", "d", "rv", "hwv", "wwv", "bb", "sz", "woo", "l", "s",
];

struct Doc<'a> {
    source: HitSource,
    title: &'a str,
    subtitle: Option<&'a str>,
    /// (normalized field text, weight)
    fields: Vec<(String, u32)>,
}

/// The best `limit` results for `query`, best first. Ties keep library
/// order, then favorites.
#[must_use]
pub fn search(
    library: &[LibraryEntry],
    favorites: &[Favorite],
    query: &str,
    limit: usize,
) -> Vec<Hit> {
    search_all(library, &[], favorites, query, limit)
}

/// [`search`] over the owner's playlists too (`spotify:playlist:<id>`,
/// name), which come last in a tie.
#[must_use]
pub fn search_all(
    library: &[LibraryEntry],
    playlists: &[(String, String)],
    favorites: &[Favorite],
    query: &str,
    limit: usize,
) -> Vec<Hit> {
    let q = normalize(query);
    let words: Vec<&str> = q.split(' ').filter(|w| !w.is_empty()).collect();
    if words.is_empty() || limit == 0 {
        return Vec::new();
    }
    let catalog = catalog_pairs(&words);
    let mut albums_seen = HashSet::new();
    let albums = library.iter().filter_map(|e| {
        // Retired rows (un-saved albums) still carry the SavedAlbum origin;
        // they must not resurface as saved-album hits. Legacy rows
        // (candidate == None) are unaffected.
        let saved = matches!(e.origin, LibraryOrigin::SavedAlbum | LibraryOrigin::Both)
            && e.candidate != Some(false);
        let (uri, title) = (e.album_uri.as_deref()?, e.track.album.as_deref()?);
        (saved && albums_seen.insert(uri)).then(|| Doc {
            source: HitSource::Album {
                source_uri: uri.to_string(),
            },
            title,
            subtitle: e.album_artists.as_deref(),
            fields: vec![
                (normalize(title), 4),
                (normalize(e.album_artists.as_deref().unwrap_or("")), 3),
            ],
        })
    });
    let docs = library
        .iter()
        .map(|e| Doc {
            source: HitSource::Library {
                source_uri: e.track.source_uri.clone(),
            },
            title: &e.track.title,
            subtitle: e.track.artist.as_deref(),
            fields: vec![
                (normalize(&e.track.title), 4),
                (normalize(e.track.artist.as_deref().unwrap_or("")), 3),
                (normalize(e.album_artists.as_deref().unwrap_or("")), 2),
                (normalize(e.track.album.as_deref().unwrap_or("")), 2),
            ],
        })
        .chain(favorites.iter().map(|f| Doc {
            source: HitSource::Favorite { id: f.id.clone() },
            title: &f.title,
            subtitle: f.description.as_deref(),
            fields: vec![
                (normalize(&f.title), 4),
                (normalize(f.description.as_deref().unwrap_or("")), 2),
            ],
        }))
        .chain(albums)
        .chain(playlists.iter().map(|(uri, name)| Doc {
            source: HitSource::Playlist {
                source_uri: uri.clone(),
            },
            title: name,
            subtitle: None,
            fields: vec![(normalize(name), 4)],
        }));
    let mut hits: Vec<Hit> = docs
        .filter_map(|doc| {
            let mut score = 0;
            for word in &words {
                score += best_match(word, &doc.fields)?;
            }
            for (marker, number) in &catalog {
                if has_pair(&doc.fields[0].0, marker, number) {
                    score += 40;
                }
            }
            Some(Hit {
                source: doc.source,
                title: doc.title.to_string(),
                subtitle: doc.subtitle.map(str::to_string),
                score,
            })
        })
        .collect();
    hits.sort_by_key(|h| std::cmp::Reverse(h.score));
    hits.truncate(limit);
    hits
}

/// The best weighted score `word` earns in any field, or `None` if it
/// matches nothing.
fn best_match(word: &str, fields: &[(String, u32)]) -> Option<u32> {
    fields
        .iter()
        .filter_map(|(text, weight)| {
            text.split(' ')
                .filter_map(|token| word_score(word, token))
                .max()
                .map(|s| s * weight)
        })
        .max()
}

/// 10 for an exact word, 7 for a prefix (two letters or more), 5 for one
/// typo in a word of five letters or more.
fn word_score(word: &str, token: &str) -> Option<u32> {
    if token == word {
        Some(10)
    } else if word.len() >= 2 && token.starts_with(word) {
        Some(7)
    } else if word.len() >= 5 && token.len() >= 5 && within_one_edit(word, token) {
        Some(5)
    } else {
        None
    }
}

/// Whether `a` and `b` differ by at most one insertion, deletion, or
/// substitution.
fn within_one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }
    let prefix = short
        .iter()
        .zip(long.iter())
        .take_while(|(x, y)| x == y)
        .count();
    if short.len() == long.len() {
        prefix == short.len() || short[prefix + 1..] == long[prefix + 1..]
    } else {
        short[prefix..] == long[prefix + 1..]
    }
}

/// The (catalog marker, number) pairs a query names, e.g. `bwv 988`.
fn catalog_pairs<'q>(words: &[&'q str]) -> Vec<(&'q str, &'q str)> {
    words
        .windows(2)
        .filter(|w| CATALOG.contains(&w[0]) && w[1].chars().all(|c| c.is_ascii_digit()))
        .map(|w| (w[0], w[1]))
        .collect()
}

fn has_pair(text: &str, marker: &str, number: &str) -> bool {
    let tokens: Vec<&str> = text.split(' ').collect();
    tokens.windows(2).any(|w| w[0] == marker && w[1] == number)
}
