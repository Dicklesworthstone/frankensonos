//! Genre tags where the Web API gives none (plan §12.1). [`crate::genres`]
//! reads each lead artist's tags, but many artists have none, and their
//! songs would be invisible to genre steering, balance and preferences. So
//! once that read is done, [`fill`] gives a song still without a tag:
//!
//! 1. the tags the owner's other songs by its lead artist carry;
//! 2. else the genres its album's or its own title names outright ("Original
//!    Motion Picture Soundtrack", "Jazz at Massey Hall");
//! 3. else `classical`, when it is a classical recording the DJ plays as one.
//!
//! The owner's own playlists name genres too ("Late Night Jazz"): a track
//! read from one carries the genres its name states ([`from_name`]). Only
//! whole words count, so "Rockabye" is not rock and "Blue" is not blues.

use std::collections::HashMap;

use crate::classical::is_classical;
use crate::library::LibraryItem;

/// Names that say a genre only when the owner named a playlist with them,
/// on top of [`TITLE_GENRES`]: in an album or song title they too often mean
/// something else ("Country Roads", "Soul Sister", "House of Pain").
const PLAYLIST_GENRES: &[(&str, &str)] = &[
    ("rock", "rock"),
    ("pop", "pop"),
    ("country", "country"),
    ("soul", "soul"),
    ("blues", "blues"),
    ("metal", "metal"),
    ("punk", "punk"),
    ("funk", "funk"),
    ("disco", "disco"),
    ("house", "house"),
    ("folk", "folk"),
    ("indie", "indie"),
    ("electronic", "electronic"),
    ("edm", "edm"),
    ("rap", "rap"),
    ("r&b", "r&b"),
    ("rnb", "r&b"),
    ("latin", "latin"),
    ("k-pop", "k-pop"),
    ("kpop", "k-pop"),
    ("dance", "dance"),
    ("classical", "classical"),
];

/// Names that say a genre wherever they appear, playlist or title.
const TITLE_GENRES: &[(&str, &str)] = &[
    ("original motion picture soundtrack", "soundtrack"),
    ("original soundtrack", "soundtrack"),
    ("motion picture soundtrack", "soundtrack"),
    ("soundtrack", "soundtrack"),
    ("original score", "soundtrack"),
    ("jazz", "jazz"),
    ("bossa nova", "bossa nova"),
    ("hip hop", "hip hop"),
    ("hip-hop", "hip hop"),
    ("reggae", "reggae"),
    ("techno", "techno"),
    ("ambient", "ambient"),
    ("bluegrass", "bluegrass"),
    ("gospel", "gospel"),
    ("lo-fi", "lo-fi"),
    ("lofi", "lo-fi"),
    ("christmas", "christmas"),
];

/// The genres a playlist's `name` states (none for "Road trip").
#[must_use]
pub fn from_name(name: &str) -> Vec<String> {
    named(name, &[TITLE_GENRES, PLAYLIST_GENRES])
}

/// The genres `text` names with whole words from `tables`, each once, in
/// the order found.
fn named(text: &str, tables: &[&[(&str, &str)]]) -> Vec<String> {
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '&' || c == '-'))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect();
    let mut found: Vec<String> = Vec::new();
    for (phrase, tag) in tables.iter().flat_map(|t| t.iter()) {
        let parts: Vec<&str> = phrase.split(' ').collect();
        let said = words
            .windows(parts.len())
            .any(|w| w.iter().eq(parts.iter()));
        if said && !found.iter().any(|f| f == tag) {
            found.push((*tag).to_owned());
        }
    }
    found
}

/// Give each item still without a genre the best guess there is (see the
/// module docs). Items that have tags keep them as they are.
pub fn fill(items: &mut [LibraryItem]) {
    let mut by_artist: HashMap<String, Vec<String>> = HashMap::new();
    for item in items.iter().filter(|i| !i.genres.is_empty()) {
        let Some(lead) = item.artists.first() else {
            continue;
        };
        let tags = by_artist.entry(lead.to_lowercase()).or_default();
        for genre in &item.genres {
            if !tags.contains(genre) {
                tags.push(genre.clone());
            }
        }
    }
    for item in items.iter_mut().filter(|i| i.genres.is_empty()) {
        if let Some(tags) = item
            .artists
            .first()
            .and_then(|lead| by_artist.get(&lead.to_lowercase()))
        {
            item.genres.clone_from(tags);
            continue;
        }
        let titled = item
            .album
            .as_deref()
            .map(|album| named(album, &[TITLE_GENRES]))
            .filter(|g| !g.is_empty())
            .unwrap_or_else(|| named(&item.title, &[TITLE_GENRES]));
        if !titled.is_empty() {
            item.genres = titled;
        } else if is_classical(item) {
            item.genres = vec!["classical".to_owned()];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(title: &str, artist: &str, album: &str, genres: &[&str]) -> LibraryItem {
        LibraryItem {
            source_uri: format!("spotify:track:{title}"),
            title: title.into(),
            artists: vec![artist.into()],
            album: Some(album.into()),
            genres: genres.iter().map(|g| (*g).to_owned()).collect(),
            ..LibraryItem::default()
        }
    }

    #[test]
    fn a_playlist_name_states_genres_in_whole_words_only() {
        assert_eq!(from_name("Late Night Jazz"), ["jazz"]);
        assert_eq!(
            from_name("Indie Rock & Hip-Hop"),
            ["hip hop", "rock", "indie"]
        );
        assert!(from_name("Rockabye baby").is_empty(), "not rock");
        assert!(from_name("Road trip").is_empty());
        assert_eq!(from_name("Blue Monday blues"), ["blues"]);
    }

    #[test]
    fn an_untagged_song_takes_its_artists_tags_then_its_titles_then_classical() {
        let mut items = vec![
            song("Sunrise", "The Quiet Ones", "Morning", &["dream pop"]),
            song("Sunset", "the quiet ones", "Evening", &[]),
            song(
                "Main Title",
                "A. Composer",
                "The Long Road (Original Motion Picture Soundtrack)",
                &[],
            ),
            song("Theme", "Trio", "Live", &[]),
            song(
                "Symphony No. 5 in C Minor, Op. 67: I. Allegro con brio",
                "Ludwig van Beethoven",
                "Beethoven: Symphonies Nos. 5 & 7",
                &[],
            ),
            song("Tagged", "Band", "Country Roads", &["folk"]),
        ];
        items[4].album_artists = vec!["Berliner Philharmoniker".into()];
        fill(&mut items);
        let tags: Vec<Vec<String>> = items.iter().map(|i| i.genres.clone()).collect();
        assert_eq!(tags[1], ["dream pop"], "the artist's other songs");
        assert_eq!(tags[2], ["soundtrack"], "the album says so");
        assert!(tags[3].is_empty(), "nothing to go on");
        assert_eq!(tags[4], ["classical"]);
        assert_eq!(tags[5], ["folk"], "a song with tags keeps them");
    }
}
