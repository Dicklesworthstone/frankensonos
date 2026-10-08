//! Whole works: a piece's movements, grouped and in playing order.
//!
//! Spotify lists a classical recording as one track per movement. A listener
//! wants the symphony, not its third movement followed by an unrelated aria,
//! so the DJ's unit of selection is the [`Work`]: the pool tracks that share
//! an album and a `work_key` (see [`crate::classical::split_title`]), ordered
//! by disc and track number, or by the movement's numeral when the numbers
//! are missing. The same work on two albums is two works sharing a
//! `work_key`, and two recordings of it on one album split where the movement
//! numbering starts over. A song (a track not judged classical) is a work of
//! its own, so the demo and the album take of one song never play as one
//! work. Pure: no I/O.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::classical::{ClassicalTrack, Period};
use crate::library::Origin;

/// Whether a work holds every one of its movements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Completeness {
    /// Nothing is missing: a single-movement piece, or a work from a saved
    /// album (whose whole track list was read) numbered I, II, … unbroken.
    Complete,
    /// Movements are provably missing: the numbering skips or starts late.
    Partial,
    /// Built from liked tracks only, so trailing movements may be missing
    /// until the album's track list is read.
    Unknown,
}

/// A work as the DJ plays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Work {
    pub work_key: String,
    pub composer: String,
    /// The work's title (the track title minus its movement part).
    pub title: String,
    pub period: Period,
    pub album_key: String,
    pub album_uri: Option<String>,
    /// In playing order.
    pub movements: Vec<ClassicalTrack>,
    /// Sum of the known movement durations.
    pub total_secs: u32,
    pub completeness: Completeness,
}

impl Work {
    /// Partial and Unknown works want their album's track list read.
    #[must_use]
    pub fn needs_expansion(&self) -> bool {
        self.completeness != Completeness::Complete
    }

    /// The composer's grouping key (shared by every movement).
    #[must_use]
    pub fn composer_key(&self) -> &str {
        self.movements
            .first()
            .map_or("", |m| m.composer_key.as_str())
    }

    /// The work's energy: its first movement's, what the listener hears
    /// first.
    #[must_use]
    pub fn energy(&self) -> u8 {
        self.movements.first().map_or(50, |m| m.energy)
    }

    /// Whether it is a classical work (analysed into movements), not a song.
    #[must_use]
    pub fn is_classical(&self) -> bool {
        self.movements.first().is_some_and(|m| m.classical)
    }

    /// Its genre tags: those its tracks carry (album or artist genres, when
    /// the library read gave any), and `"classical"` for a classical work.
    #[must_use]
    pub fn genres(&self) -> Vec<&str> {
        let mut tags: Vec<&str> = Vec::new();
        for genre in self.movements.iter().flat_map(|m| &m.genres) {
            if !tags.contains(&genre.as_str()) {
                tags.push(genre);
            }
        }
        if self.is_classical() && !tags.contains(&"classical") {
            tags.push("classical");
        }
        tags
    }

    /// The genre it is balanced under: "classical" for a classical work,
    /// else its first tag (none for an untagged song).
    #[must_use]
    pub fn genre(&self) -> Option<&str> {
        if self.is_classical() {
            return Some("classical");
        }
        self.movements
            .first()
            .and_then(|m| m.genres.first())
            .map(String::as_str)
    }

    /// Its album's release year, when known.
    #[must_use]
    pub fn year(&self) -> Option<u16> {
        self.movements.first().and_then(|m| m.year)
    }

    /// Whether the owner individually liked any of its movements.
    #[must_use]
    pub fn is_liked(&self) -> bool {
        self.movements.iter().any(|m| m.origin.is_liked())
    }
}

/// Group tracks into works, in order of each work's first track. Every
/// track lands in exactly one work; a song is alone in its own.
#[must_use]
pub fn group_works(tracks: &[ClassicalTrack]) -> Vec<Work> {
    let mut groups: Vec<Vec<&ClassicalTrack>> = Vec::new();
    let mut index: HashMap<(&str, &str), usize> = HashMap::new();
    for track in tracks {
        let within = if track.classical {
            track.work_key.as_str()
        } else {
            track.track.source_uri.as_str()
        };
        let key = (track.album_key.as_str(), within);
        let slot = *index.entry(key).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[slot].push(track);
    }
    groups
        .into_iter()
        .flat_map(split_recordings)
        .map(|movements| build(&movements))
        .collect()
}

/// Order a group's movements, then split it wherever the numbering starts
/// over (two recordings of one work on one album). Repeated numerals are
/// sections of one movement (`III. Sequentia: No. 1 …`, `III. … No. 2 …`).
fn split_recordings(mut group: Vec<&ClassicalTrack>) -> Vec<Vec<&ClassicalTrack>> {
    let positioned = group.iter().all(|t| t.track_number.is_some());
    if positioned {
        group.sort_by_key(|t| (t.disc_number.unwrap_or(1), t.track_number));
    } else {
        // No album positions: order by numeral; unnumbered keep their order.
        group.sort_by_key(|t| numeral(t).unwrap_or(u32::MAX));
        return vec![group];
    }
    let mut works: Vec<Vec<&ClassicalTrack>> = Vec::new();
    let mut last: Option<u32> = None;
    for track in group {
        let n = numeral(track);
        let restart = matches!((last, n), (Some(prev), Some(now)) if now < prev);
        if restart || works.is_empty() {
            works.push(Vec::new());
        }
        if n.is_some() {
            last = n;
        }
        works.last_mut().expect("a work was started").push(track);
    }
    works
}

/// A work from movements already in playing order.
pub(crate) fn build(movements: &[&ClassicalTrack]) -> Work {
    let first = movements[0];
    Work {
        work_key: first.work_key.clone(),
        composer: first.composer.clone(),
        title: first.work.clone(),
        period: first.period,
        album_key: first.album_key.clone(),
        album_uri: first.album_uri.clone(),
        total_secs: movements.iter().filter_map(|t| t.track.duration_secs).sum(),
        completeness: completeness(movements),
        movements: movements.iter().map(|&t| t.clone()).collect(),
    }
}

fn completeness(movements: &[&ClassicalTrack]) -> Completeness {
    let numerals: BTreeSet<u32> = movements.iter().filter_map(|t| numeral(t)).collect();
    let unbroken = numerals
        .iter()
        .copied()
        .eq(1..=u32::try_from(numerals.len()).unwrap_or(0));
    if !unbroken {
        return Completeness::Partial;
    }
    if movements.len() == 1 && movements[0].movement.is_none() {
        return Completeness::Complete;
    }
    let album_read = movements
        .iter()
        .any(|t| matches!(t.origin, Origin::SavedAlbum | Origin::Both));
    if album_read {
        Completeness::Complete
    } else {
        Completeness::Unknown
    }
}

fn numeral(track: &ClassicalTrack) -> Option<u32> {
    track.movement.as_deref().and_then(movement_number)
}

/// The number a movement title starts with: a roman numeral (`"IV. Presto"`),
/// an arabic one (`"2. Andante"`), or a set number (`"No. 4 in E Minor"`).
#[must_use]
pub fn movement_number(movement: &str) -> Option<u32> {
    const TRIM: [char; 4] = ['.', ':', ')', ','];
    let mut words = movement.split_whitespace();
    let mut first = words.next()?.trim_end_matches(TRIM);
    if matches!(first, "No" | "Nr" | "Nos" | "N") {
        first = words.next()?.trim_end_matches(TRIM);
    }
    if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) {
        return first.parse().ok().filter(|&n| n > 0);
    }
    roman(first)
}

fn roman(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 8 {
        return None;
    }
    let digits: Vec<i64> = s
        .chars()
        .map(|c| match c {
            'I' => Some(1),
            'V' => Some(5),
            'X' => Some(10),
            'L' => Some(50),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let total: i64 = digits
        .iter()
        .enumerate()
        .map(|(i, &d)| {
            if digits.get(i + 1).is_some_and(|&next| next > d) {
                -d
            } else {
                d
            }
        })
        .sum();
    u32::try_from(total).ok().filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::classical::CandidatePool;
    use crate::client::{Paging, SavedAlbum};
    use crate::library::LibraryItem;

    /// One album of the synthetic corpus: (album, composer, origin, tracks as
    /// (title, disc, track)). Titles follow Spotify's classical conventions.
    type AlbumSpec = (
        &'static str,
        &'static str,
        Origin,
        Vec<(String, Option<u32>, Option<u32>)>,
    );

    fn numbered(titles: &[&str]) -> Vec<(String, Option<u32>, Option<u32>)> {
        (1u32..)
            .zip(titles)
            .map(|(n, t)| ((*t).to_owned(), Some(1), Some(n)))
            .collect()
    }

    #[allow(clippy::too_many_lines)] // a data table of titles, not logic
    fn corpus() -> Vec<AlbumSpec> {
        let mut albums: Vec<AlbumSpec> = Vec::new();
        // Three sonatas on one disc must split at the work boundaries.
        albums.push((
            "Beethoven: Piano Sonatas Nos. 8, 14 & 23",
            "Ludwig van Beethoven",
            Origin::SavedAlbum,
            numbered(&[
                "Piano Sonata No. 8 in C Minor, Op. 13 \"Pathétique\": I. Grave - Allegro di molto e con brio",
                "Piano Sonata No. 8 in C Minor, Op. 13 \"Pathétique\": II. Adagio cantabile",
                "Piano Sonata No. 8 in C Minor, Op. 13 \"Pathétique\": III. Rondo. Allegro",
                "Piano Sonata No. 14 in C-Sharp Minor, Op. 27 No. 2 \"Moonlight\": I. Adagio sostenuto - Remastered 2015",
                "Piano Sonata No. 14 in C-Sharp Minor, Op. 27 No. 2 \"Moonlight\": II. Allegretto - Remastered 2015",
                "Piano Sonata No. 14 in C-Sharp Minor, Op. 27 No. 2 \"Moonlight\": III. Presto agitato - Remastered 2015",
                "Piano Sonata No. 23 in F Minor, Op. 57 \"Appassionata\": I. Allegro assai",
                "Piano Sonata No. 23 in F Minor, Op. 57 \"Appassionata\": II. Andante con moto",
                "Piano Sonata No. 23 in F Minor, Op. 57 \"Appassionata\": III. Allegro ma non troppo - Presto",
            ]),
        ));
        // Two discs, listed out of order: (disc, track) decides.
        albums.push((
            "Mahler: Symphony No. 2 \"Resurrection\"",
            "Gustav Mahler",
            Origin::SavedAlbum,
            vec![
                (
                    "Symphony No. 2 in C Minor \"Resurrection\": V. Im Tempo des Scherzos".into(),
                    Some(2),
                    Some(3),
                ),
                (
                    "Symphony No. 2 in C Minor \"Resurrection\": I. Allegro maestoso".into(),
                    Some(1),
                    Some(1),
                ),
                (
                    "Symphony No. 2 in C Minor \"Resurrection\": III. In ruhig fließender Bewegung"
                        .into(),
                    Some(2),
                    Some(1),
                ),
                (
                    "Symphony No. 2 in C Minor \"Resurrection\": II. Andante moderato".into(),
                    Some(1),
                    Some(2),
                ),
                (
                    "Symphony No. 2 in C Minor \"Resurrection\": IV. Urlicht".into(),
                    Some(2),
                    Some(2),
                ),
            ],
        ));
        // Thirty-two unnumbered parts of one work.
        let mut goldberg = vec!["Goldberg Variations, BWV 988: Aria".to_owned()];
        goldberg.extend((1..=30).map(|n| {
            let manuals = if n % 3 == 0 { "Canone" } else { "a 1 Clav." };
            format!("Goldberg Variations, BWV 988: Variatio {n}. {manuals}")
        }));
        goldberg.push("Goldberg Variations, BWV 988: Aria da capo".into());
        let goldberg: Vec<&str> = goldberg.iter().map(String::as_str).collect();
        albums.push((
            "Bach: Goldberg Variations, BWV 988",
            "Johann Sebastian Bach",
            Origin::SavedAlbum,
            numbered(&goldberg),
        ));
        // Opera: acts collapse into one work.
        albums.push((
            "Verdi: La traviata",
            "Giuseppe Verdi",
            Origin::SavedAlbum,
            numbered(&[
                "La traviata, Act 1: Libiamo ne' lieti calici",
                "La traviata, Act 1: È strano!... Ah, fors'è lui",
                "La traviata, Act 1: Sempre libera",
                "La traviata, Act 2: Lunge da lei... De' miei bollenti spiriti",
                "La traviata, Act 3: Addio del passato",
            ]),
        ));
        // Repeated numerals are sections of one movement, not a restart.
        albums.push((
            "Mozart: Requiem",
            "Wolfgang Amadeus Mozart",
            Origin::SavedAlbum,
            numbered(&[
                "Requiem in D Minor, K. 626: I. Introitus: Requiem aeternam",
                "Requiem in D Minor, K. 626: II. Kyrie",
                "Requiem in D Minor, K. 626: III. Sequentia: No. 1, Dies irae",
                "Requiem in D Minor, K. 626: III. Sequentia: No. 2, Tuba mirum",
                "Requiem in D Minor, K. 626: III. Sequentia: No. 3, Rex tremendae",
                "Requiem in D Minor, K. 626: III. Sequentia: No. 6, Lacrimosa",
                "Requiem in D Minor, K. 626: IV. Offertorium: No. 1, Domine Jesu",
                "Requiem in D Minor, K. 626: IV. Offertorium: No. 2, Hostias",
                "Requiem in D Minor, K. 626: V. Sanctus",
                "Requiem in D Minor, K. 626: VI. Benedictus",
                "Requiem in D Minor, K. 626: VII. Agnus Dei",
                "Requiem in D Minor, K. 626: VIII. Communio: Lux aeterna",
            ]),
        ));
        // Two recordings of one work on one album.
        albums.push((
            "Vivaldi: The Four Seasons - Two Recordings",
            "Antonio Vivaldi",
            Origin::SavedAlbum,
            numbered(&[
                "Violin Concerto in E Major, RV 269 \"Spring\": I. Allegro",
                "Violin Concerto in E Major, RV 269 \"Spring\": II. Largo",
                "Violin Concerto in E Major, RV 269 \"Spring\": III. Allegro",
                "Violin Concerto in E Major, RV 269 \"Spring\": I. Allegro",
                "Violin Concerto in E Major, RV 269 \"Spring\": II. Largo",
                "Violin Concerto in E Major, RV 269 \"Spring\": III. Allegro",
            ]),
        ));
        // Six quartets × four movements.
        let haydn: Vec<String> = (1..=6)
            .flat_map(|q| {
                [
                    "I. Allegro con spirito",
                    "II. Adagio",
                    "III. Menuetto. Allegro",
                    "IV. Finale. Presto",
                ]
                .map(|m| format!("String Quartet in G Major, Op. 76 No. {q}: {m}"))
            })
            .collect();
        let haydn: Vec<&str> = haydn.iter().map(String::as_str).collect();
        albums.push((
            "Haydn: String Quartets, Op. 76",
            "Joseph Haydn",
            Origin::SavedAlbum,
            numbered(&haydn),
        ));
        // A set of pieces: "No. N" orders them.
        albums.push((
            "Chopin: Nocturnes",
            "Frédéric Chopin",
            Origin::SavedAlbum,
            numbered(&[
                "Nocturnes, Op. 9: No. 1 in B-Flat Minor",
                "Nocturnes, Op. 9: No. 2 in E-Flat Major",
                "Nocturnes, Op. 9: No. 3 in B Major",
                "Nocturnes, Op. 15: No. 1 in F Major",
                "Nocturnes, Op. 15: No. 2 in F-Sharp Major",
                "Nocturnes, Op. 15: No. 3 in G Minor",
            ]),
        ));
        // Liked singles: a late movement (Partial), a first movement
        // (Unknown), and a whole piece (Complete).
        albums.push((
            "Beethoven: Symphony No. 9",
            "Ludwig van Beethoven",
            Origin::LikedTrack,
            vec![(
                "Symphony No. 9 in D Minor, Op. 125 \"Choral\": IV. Presto".into(),
                Some(1),
                Some(4),
            )],
        ));
        albums.push((
            "Beethoven: Symphony No. 5",
            "Ludwig van Beethoven",
            Origin::LikedTrack,
            vec![(
                "Symphony No. 5 in C Minor, Op. 67: I. Allegro con brio".into(),
                Some(1),
                Some(1),
            )],
        ));
        albums.push((
            "Satie: Piano Works",
            "Erik Satie",
            Origin::LikedTrack,
            vec![("Gymnopédie No. 1".into(), Some(1), Some(7))],
        ));
        // A cache row from before positions were recorded: numerals decide.
        albums.push((
            "Brahms: Symphony No. 4",
            "Johannes Brahms",
            Origin::SavedAlbum,
            [
                "IV. Allegro energico e passionato",
                "II. Andante moderato",
                "I. Allegro non troppo",
                "III. Allegro giocoso",
            ]
            .map(|m| {
                (
                    format!("Symphony No. 4 in E Minor, Op. 98: {m}"),
                    None,
                    None,
                )
            })
            .to_vec(),
        ));
        // A highlights album with a gap.
        albums.push((
            "Beethoven Favourites",
            "Ludwig van Beethoven",
            Origin::SavedAlbum,
            numbered(&[
                "Symphony No. 7 in A Major, Op. 92: II. Allegretto",
                "Symphony No. 7 in A Major, Op. 92: IV. Allegro con brio",
            ]),
        ));
        albums
    }

    fn corpus_items() -> Vec<LibraryItem> {
        let mut items = Vec::new();
        for (a, (album, composer, origin, tracks)) in corpus().into_iter().enumerate() {
            for (t, (title, disc, track)) in tracks.into_iter().enumerate() {
                items.push(LibraryItem {
                    source_uri: format!("spotify:track:works-{a}-{t}"),
                    title,
                    artists: vec![composer.into(), "Test Ensemble".into()],
                    album: Some(album.into()),
                    album_uri: Some(format!("spotify:album:works-{a}")),
                    album_artists: vec![composer.into()],
                    disc_number: disc,
                    track_number: track,
                    added_at: None,
                    genres: Vec::new(),
                    release_year: None,
                    label: None,
                    duration_secs: Some(300),
                    explicit: false,
                    origin,
                });
            }
        }
        items
    }

    fn find<'w>(works: &'w [Work], album: usize, title_part: &str) -> Vec<&'w Work> {
        let uri = format!("spotify:album:works-{album}");
        works
            .iter()
            .filter(|w| {
                w.album_uri.as_deref() == Some(uri.as_str()) && w.title.contains(title_part)
            })
            .collect()
    }

    fn movement_titles(work: &Work) -> Vec<&str> {
        work.movements
            .iter()
            .map(|m| m.movement.as_deref().unwrap_or(""))
            .collect()
    }

    #[test]
    fn corpus_groups_every_track_into_exactly_one_ordered_work() {
        let items = corpus_items();
        assert!(items.len() >= 100, "corpus has {} titles", items.len());
        let pool = CandidatePool::build(&items);
        assert_eq!(pool.len(), items.len(), "the whole corpus is classical");
        let works = group_works(pool.tracks());

        let mut seen = HashSet::new();
        for work in &works {
            for m in &work.movements {
                assert!(
                    seen.insert(m.track.source_uri.clone()),
                    "{} twice",
                    m.track.source_uri
                );
                assert_eq!(m.work_key, work.work_key);
            }
            assert_eq!(
                work.total_secs,
                300 * u32::try_from(work.movements.len()).unwrap()
            );
        }
        assert_eq!(seen.len(), items.len(), "every track lands in a work");
        // 3 sonatas + Mahler + Goldberg + opera + Requiem + 2 Springs
        // + 6 quartets + 2 nocturne sets + 3 liked + Brahms + highlights.
        assert_eq!(
            works.len(),
            22,
            "{:#?}",
            works.iter().map(|w| &w.title).collect::<Vec<_>>()
        );

        // Multi-work disc splits at the boundaries, each in order.
        for (title, n) in [("No. 8", 3), ("No. 14", 3), ("No. 23", 3)] {
            let w = find(&works, 0, title);
            assert_eq!(w.len(), 1, "{title}");
            assert_eq!(w[0].movements.len(), n);
            assert_eq!(w[0].completeness, Completeness::Complete);
        }
        let moonlight = find(&works, 0, "No. 14")[0];
        assert_eq!(
            movement_titles(moonlight),
            [
                "I. Adagio sostenuto",
                "II. Allegretto",
                "III. Presto agitato"
            ]
        );

        // Two discs, scrambled input → disc/track order.
        let mahler = find(&works, 1, "Symphony No. 2")[0];
        let numerals: Vec<u32> = mahler.movements.iter().filter_map(numeral).collect();
        assert_eq!(numerals, [1, 2, 3, 4, 5]);

        let goldberg = find(&works, 2, "Goldberg")[0];
        assert_eq!(goldberg.movements.len(), 32);
        assert_eq!(movement_titles(goldberg)[0], "Aria");
        assert_eq!(movement_titles(goldberg)[31], "Aria da capo");
        assert_eq!(goldberg.completeness, Completeness::Complete);

        let opera = find(&works, 3, "La traviata")[0];
        assert_eq!(
            (opera.title.as_str(), opera.movements.len()),
            ("La traviata", 5)
        );

        let requiem = find(&works, 4, "Requiem")[0];
        assert_eq!(
            requiem.movements.len(),
            12,
            "repeated numerals stay one work"
        );
        assert_eq!(requiem.completeness, Completeness::Complete);

        let springs = find(&works, 5, "Spring");
        assert_eq!(springs.len(), 2, "two recordings split");
        assert!(
            springs
                .iter()
                .all(|w| movement_titles(w) == ["I. Allegro", "II. Largo", "III. Allegro"])
        );
        assert_eq!(springs[0].work_key, springs[1].work_key);

        assert_eq!(find(&works, 6, "Op. 76").len(), 6);
        let op9 = find(&works, 7, "Op. 9")[0];
        assert_eq!(op9.movements.len(), 3);

        assert_eq!(
            find(&works, 8, "Symphony No. 9")[0].completeness,
            Completeness::Partial
        );
        assert_eq!(
            find(&works, 9, "Symphony No. 5")[0].completeness,
            Completeness::Unknown
        );
        let gymno = find(&works, 10, "Gymnopédie")[0];
        assert_eq!(
            (gymno.movements.len(), gymno.completeness),
            (1, Completeness::Complete)
        );
        assert!(!gymno.needs_expansion());

        // No positions: the numerals order the movements.
        let brahms = find(&works, 11, "Symphony No. 4")[0];
        assert_eq!(
            brahms
                .movements
                .iter()
                .filter_map(numeral)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(brahms.completeness, Completeness::Complete);

        let highlights = find(&works, 12, "Symphony No. 7")[0];
        assert_eq!(highlights.completeness, Completeness::Partial);
        assert!(highlights.needs_expansion());
    }

    #[test]
    fn same_work_on_two_albums_is_two_works() {
        let mut items = corpus_items();
        let mut again = items[9].clone(); // a Mahler movement, on another album
        again.source_uri = "spotify:track:other-mahler".into();
        again.album_uri = Some("spotify:album:other".into());
        items.push(again);
        let works = group_works(CandidatePool::build(&items).tracks());
        let mahlers: Vec<&Work> = works
            .iter()
            .filter(|w| w.title.contains("Resurrection"))
            .collect();
        assert_eq!(mahlers.len(), 2);
        assert_eq!(mahlers[0].work_key, mahlers[1].work_key);
    }

    #[test]
    fn saved_albums_fixture_with_positions() {
        let page = Paging::<SavedAlbum>::parse(include_bytes!(
            "../tests/fixtures/saved_albums_works.json"
        ))
        .unwrap();
        let items: Vec<LibraryItem> = page
            .items
            .iter()
            .flat_map(SavedAlbum::library_items)
            .collect();
        assert_eq!(items.len(), 13);
        assert_eq!(
            (items[0].disc_number, items[0].track_number),
            (Some(1), Some(9))
        );
        let works = group_works(CandidatePool::build(&items).tracks());
        let titles: Vec<(&str, usize)> = works
            .iter()
            .map(|w| (w.title.as_str(), w.movements.len()))
            .collect();
        assert_eq!(
            titles,
            [
                ("Partita No. 2 in D Minor, BWV 1004", 5),
                ("Sonata No. 1 in G Minor, BWV 1001", 4),
                ("Cello Concerto in E Minor, Op. 85", 4),
            ]
        );
        assert!(
            works
                .iter()
                .all(|w| w.completeness == Completeness::Complete)
        );
        for work in &works {
            let numerals: Vec<u32> = work.movements.iter().filter_map(numeral).collect();
            let expected: Vec<u32> = (1..=u32::try_from(work.movements.len()).unwrap()).collect();
            assert_eq!(numerals, expected, "{} out of order", work.title);
        }
        // The concerto spans two discs, listed out of order in the JSON.
        let discs: Vec<Option<u32>> = works[2].movements.iter().map(|m| m.disc_number).collect();
        assert_eq!(discs, [Some(1), Some(1), Some(2), Some(2)]);
    }

    #[test]
    fn songs_are_works_of_their_own_and_classical_works_stay_whole() {
        let items = crate::test_shelf::mixed_items();
        let works = group_works(CandidatePool::build(&items).tracks());
        let (songs, classical): (Vec<&Work>, Vec<&Work>) = works.iter().partition(|w| {
            w.movements[0]
                .track
                .source_uri
                .starts_with("spotify:track:song-")
        });
        // Every song alone: the soundtrack cues and the interludes (titled
        // `Work: Part`) and the deluxe edition's two takes of one song.
        assert_eq!(
            songs.len(),
            26,
            "{songs:#?} (the explicit one too, flagged)"
        );
        for song in &songs {
            assert_eq!(song.movements.len(), 1, "{}", song.title);
            assert_eq!(song.completeness, Completeness::Complete);
            assert!(!song.needs_expansion());
        }
        let harbor: Vec<&&Work> = songs
            .iter()
            .filter(|w| w.title == "Harbor Lights")
            .collect();
        assert_eq!(harbor.len(), 2, "the album take and the acoustic one");
        assert_eq!(
            harbor[0].work_key, harbor[1].work_key,
            "one song to feedback"
        );
        // Beethoven's symphonies keep all four movements, in order.
        let symphonies: Vec<&&Work> = classical
            .iter()
            .filter(|w| w.composer == "Ludwig van Beethoven")
            .collect();
        assert_eq!(symphonies.len(), 9);
        for work in symphonies {
            let numerals: Vec<Option<u32>> = work
                .movements
                .iter()
                .map(|m| m.movement.as_deref().and_then(movement_number))
                .collect();
            assert_eq!(
                numerals,
                [Some(1), Some(2), Some(3), Some(4)],
                "{}",
                work.title
            );
        }
    }

    #[test]
    fn movement_numbers() {
        assert_eq!(movement_number("I. Allegro"), Some(1));
        assert_eq!(movement_number("IV. Presto"), Some(4));
        assert_eq!(movement_number("IX: Finale"), Some(9));
        assert_eq!(movement_number("XIV. Fuga"), Some(14));
        assert_eq!(movement_number("XXIX. Variatio"), Some(29));
        assert_eq!(movement_number("2. Andante"), Some(2));
        assert_eq!(movement_number("No. 4 in E Minor"), Some(4));
        assert_eq!(movement_number("Nr. 12"), Some(12));
        assert_eq!(movement_number("Aria"), None);
        assert_eq!(movement_number("Variatio 1. a 1 Clav."), None);
        assert_eq!(movement_number("Libiamo ne' lieti calici"), None);
        assert_eq!(movement_number("0. Prelude"), None);
        assert_eq!(movement_number(""), None);
    }
}
