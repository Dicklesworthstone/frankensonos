//! Classical-music metadata heuristics and the DJ's candidate pool.
//!
//! Spotify has no "is this classical?" flag and (for new apps) no audio
//! features, so everything here is inferred from the metadata the library
//! reads return: titles, artist credits, album names and — when present —
//! genres and label. Classical releases follow strong conventions that carry
//! most of the signal: the composer is credited as an artist, titles read
//! `Work, Catalogue: Movement`, movements carry tempo markings, and albums are
//! titled `Composer: Works`. All of it is pure and deterministic.

use std::collections::HashMap;
use std::sync::OnceLock;

use fsonos_types::Track;
use serde::{Deserialize, Serialize};

use crate::library::{LibraryItem, Origin};

/// Style period, used to spread the DJ's picks across eras.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Period {
    Medieval,
    Renaissance,
    Baroque,
    Classical,
    Romantic,
    LateRomantic,
    Impressionist,
    Modern,
    Contemporary,
    Unknown,
}

impl Period {
    pub const COUNT: usize = 10;

    /// Dense index in `0..Period::COUNT`.
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }
}

/// A composer the heuristics recognise. `full` spellings match artist credits
/// exactly; `short` forms (surnames) only match a `Name:` title/album prefix,
/// where a bare surname is unambiguous.
#[derive(Debug)]
pub struct Composer {
    pub name: &'static str,
    pub period: Period,
    full: &'static [&'static str],
    short: &'static [&'static str],
}

const fn c(
    name: &'static str,
    period: Period,
    full: &'static [&'static str],
    short: &'static [&'static str],
) -> Composer {
    Composer {
        name,
        period,
        full,
        short,
    }
}

// Aliases are written pre-normalized (see [`normalize`]); each composer's
// canonical name is indexed automatically. On a collision the earlier entry
// wins, so the dominant bearer of a surname comes first.
#[rustfmt::skip]
static COMPOSERS: &[Composer] = {
    use Period::{Baroque, Classical, Contemporary, Impressionist, LateRomantic, Medieval, Modern, Renaissance, Romantic};
    &[
        c("Hildegard von Bingen", Medieval, &["hildegard of bingen"], &["hildegard"]),
        c("Guillaume de Machaut", Medieval, &[], &["machaut"]),
        c("Pérotin", Medieval, &["perotinus"], &[]),
        c("Josquin des Prez", Renaissance, &["josquin des pres", "josquin"], &[]),
        c("Giovanni Pierluigi da Palestrina", Renaissance, &[], &["palestrina"]),
        c("Thomas Tallis", Renaissance, &[], &["tallis"]),
        c("William Byrd", Renaissance, &[], &["byrd"]),
        c("Tomás Luis de Victoria", Renaissance, &[], &[]),
        c("John Dowland", Renaissance, &[], &["dowland"]),
        c("Orlande de Lassus", Renaissance, &["orlando di lasso", "orlando de lassus"], &["lassus"]),
        c("Gregorio Allegri", Renaissance, &[], &["allegri"]),
        c("Claudio Monteverdi", Baroque, &[], &["monteverdi"]),
        c("Johann Sebastian Bach", Baroque, &["j s bach", "js bach"], &["bach"]),
        c("George Frideric Handel", Baroque, &["georg friedrich handel", "georg friedrich haendel", "george frederick handel"], &["handel", "haendel"]),
        c("Antonio Vivaldi", Baroque, &[], &["vivaldi"]),
        c("Georg Philipp Telemann", Baroque, &[], &["telemann"]),
        c("Henry Purcell", Baroque, &[], &["purcell"]),
        c("Arcangelo Corelli", Baroque, &[], &["corelli"]),
        c("Domenico Scarlatti", Baroque, &["d scarlatti"], &["scarlatti"]),
        c("Alessandro Scarlatti", Baroque, &["a scarlatti"], &[]),
        c("Jean-Philippe Rameau", Baroque, &[], &["rameau"]),
        c("François Couperin", Baroque, &[], &["couperin"]),
        c("Johann Pachelbel", Baroque, &[], &["pachelbel"]),
        c("Tomaso Albinoni", Baroque, &[], &["albinoni"]),
        c("Dieterich Buxtehude", Baroque, &[], &["buxtehude"]),
        c("Heinrich Ignaz Franz von Biber", Baroque, &["heinrich biber"], &["biber"]),
        c("Jean-Baptiste Lully", Baroque, &[], &["lully"]),
        c("Giovanni Battista Pergolesi", Baroque, &[], &["pergolesi"]),
        c("Jan Dismas Zelenka", Baroque, &[], &["zelenka"]),
        c("Heinrich Schütz", Baroque, &[], &["schutz"]),
        c("Giuseppe Tartini", Baroque, &[], &["tartini"]),
        c("Marin Marais", Baroque, &[], &["marais"]),
        c("Alessandro Marcello", Baroque, &[], &[]),
        c("Joseph Haydn", Classical, &["franz joseph haydn"], &["haydn"]),
        c("Wolfgang Amadeus Mozart", Classical, &["w a mozart"], &["mozart"]),
        c("Carl Philipp Emanuel Bach", Classical, &["c p e bach", "cpe bach"], &[]),
        c("Johann Christian Bach", Classical, &["j c bach"], &[]),
        c("Christoph Willibald Gluck", Classical, &[], &["gluck"]),
        c("Luigi Boccherini", Classical, &[], &["boccherini"]),
        c("Antonio Salieri", Classical, &[], &["salieri"]),
        c("Muzio Clementi", Classical, &[], &["clementi"]),
        c("Johann Nepomuk Hummel", Classical, &[], &["hummel"]),
        c("Ludwig van Beethoven", Classical, &[], &["beethoven"]),
        c("Franz Schubert", Romantic, &[], &["schubert"]),
        c("Carl Maria von Weber", Romantic, &[], &["weber"]),
        c("Gioachino Rossini", Romantic, &["gioacchino rossini"], &["rossini"]),
        c("Niccolò Paganini", Romantic, &[], &["paganini"]),
        c("Felix Mendelssohn", Romantic, &["felix mendelssohn bartholdy"], &["mendelssohn"]),
        c("Fanny Mendelssohn", Romantic, &["fanny hensel"], &[]),
        c("Frédéric Chopin", Romantic, &["fryderyk chopin"], &["chopin"]),
        c("Robert Schumann", Romantic, &[], &["schumann"]),
        c("Clara Schumann", Romantic, &[], &[]),
        c("Franz Liszt", Romantic, &["ferenc liszt"], &["liszt"]),
        c("Hector Berlioz", Romantic, &[], &["berlioz"]),
        c("Vincenzo Bellini", Romantic, &[], &["bellini"]),
        c("Gaetano Donizetti", Romantic, &[], &["donizetti"]),
        c("John Field", Romantic, &[], &[]),
        c("Richard Wagner", LateRomantic, &[], &["wagner"]),
        c("Giuseppe Verdi", LateRomantic, &[], &["verdi"]),
        c("Johannes Brahms", LateRomantic, &[], &["brahms"]),
        c("Anton Bruckner", LateRomantic, &[], &["bruckner"]),
        c("Pyotr Ilyich Tchaikovsky", LateRomantic, &["peter ilyich tchaikovsky", "piotr ilyich tchaikovsky", "pyotr ilych tchaikovsky", "peter tchaikovsky"], &["tchaikovsky", "tschaikowsky"]),
        c("Antonín Dvořák", LateRomantic, &[], &["dvorak"]),
        c("Edvard Grieg", LateRomantic, &[], &["grieg"]),
        c("Gustav Mahler", LateRomantic, &[], &["mahler"]),
        c("Camille Saint-Saëns", LateRomantic, &[], &["saint saens"]),
        c("Gabriel Fauré", LateRomantic, &[], &["faure"]),
        c("César Franck", LateRomantic, &[], &["franck"]),
        c("Georges Bizet", LateRomantic, &[], &["bizet"]),
        c("Giacomo Puccini", LateRomantic, &[], &["puccini"]),
        c("Edward Elgar", LateRomantic, &[], &["elgar"]),
        c("Nikolai Rimsky-Korsakov", LateRomantic, &["nikolay rimsky korsakov"], &["rimsky korsakov"]),
        c("Modest Mussorgsky", LateRomantic, &["modest moussorgsky"], &["mussorgsky", "moussorgsky"]),
        c("Alexander Borodin", LateRomantic, &[], &["borodin"]),
        c("Bedřich Smetana", LateRomantic, &[], &["smetana"]),
        c("Johann Strauss II", LateRomantic, &["johann strauss jr", "johann strauss"], &["strauss"]),
        c("Richard Strauss", LateRomantic, &["r strauss"], &[]),
        c("Jean Sibelius", LateRomantic, &[], &["sibelius"]),
        c("Sergei Rachmaninoff", LateRomantic, &["sergei rachmaninov", "sergey rachmaninov", "sergey rachmaninoff", "serge rachmaninoff"], &["rachmaninoff", "rachmaninov"]),
        c("Jules Massenet", LateRomantic, &[], &["massenet"]),
        c("Jacques Offenbach", LateRomantic, &[], &["offenbach"]),
        c("Max Bruch", LateRomantic, &[], &["bruch"]),
        c("Alexander Scriabin", LateRomantic, &["alexander skryabin"], &["scriabin", "skryabin"]),
        c("Isaac Albéniz", LateRomantic, &[], &["albeniz"]),
        c("Enrique Granados", LateRomantic, &[], &["granados"]),
        c("Claude Debussy", Impressionist, &[], &["debussy"]),
        c("Maurice Ravel", Impressionist, &[], &["ravel"]),
        c("Erik Satie", Impressionist, &[], &["satie"]),
        c("Ottorino Respighi", Impressionist, &[], &["respighi"]),
        c("Igor Stravinsky", Modern, &[], &["stravinsky"]),
        c("Arnold Schoenberg", Modern, &["arnold schonberg"], &["schoenberg", "schonberg"]),
        c("Alban Berg", Modern, &[], &[]),
        c("Anton Webern", Modern, &[], &["webern"]),
        c("Béla Bartók", Modern, &[], &["bartok"]),
        c("Sergei Prokofiev", Modern, &["sergey prokofiev"], &["prokofiev"]),
        c("Dmitri Shostakovich", Modern, &["dmitry shostakovich"], &["shostakovich"]),
        c("Gustav Holst", Modern, &[], &["holst"]),
        c("Ralph Vaughan Williams", Modern, &[], &["vaughan williams"]),
        c("Benjamin Britten", Modern, &[], &["britten"]),
        c("Aaron Copland", Modern, &[], &["copland"]),
        c("Samuel Barber", Modern, &[], &["barber"]),
        c("George Gershwin", Modern, &[], &["gershwin"]),
        c("Francis Poulenc", Modern, &[], &["poulenc"]),
        c("Olivier Messiaen", Modern, &[], &["messiaen"]),
        c("Leoš Janáček", Modern, &[], &["janacek"]),
        c("Carl Nielsen", Modern, &[], &[]),
        c("Joaquín Rodrigo", Modern, &[], &["rodrigo"]),
        c("Heitor Villa-Lobos", Modern, &[], &["villa lobos"]),
        c("Aram Khachaturian", Modern, &[], &["khachaturian"]),
        c("Carl Orff", Modern, &[], &["orff"]),
        c("György Ligeti", Modern, &[], &["ligeti"]),
        c("Arvo Pärt", Contemporary, &[], &["part"]),
        c("Philip Glass", Contemporary, &[], &["glass"]),
        c("Steve Reich", Contemporary, &[], &["reich"]),
        c("John Adams", Contemporary, &[], &[]),
        c("Henryk Górecki", Contemporary, &["henryk mikolaj gorecki"], &["gorecki"]),
        c("John Tavener", Contemporary, &[], &["tavener"]),
        c("Morten Lauridsen", Contemporary, &[], &["lauridsen"]),
        c("Eric Whitacre", Contemporary, &[], &["whitacre"]),
        c("John Rutter", Contemporary, &[], &["rutter"]),
        c("Karl Jenkins", Contemporary, &[], &[]),
        c("Pēteris Vasks", Contemporary, &[], &["vasks"]),
        c("Kaija Saariaho", Contemporary, &[], &["saariaho"]),
        c("Thomas Adès", Contemporary, &[], &[]),
        c("Caroline Shaw", Contemporary, &[], &[]),
        c("Max Richter", Contemporary, &[], &[]),
        c("Ludovico Einaudi", Contemporary, &[], &["einaudi"]),
        c("Ólafur Arnalds", Contemporary, &[], &[]),
        c("Jóhann Jóhannsson", Contemporary, &[], &[]),
        c("Hildur Guðnadóttir", Contemporary, &[], &[]),
    ]
};

struct ComposerIndex {
    by_artist: HashMap<String, usize>,
    by_prefix: HashMap<String, usize>,
}

fn index() -> &'static ComposerIndex {
    static INDEX: OnceLock<ComposerIndex> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut by_artist = HashMap::new();
        let mut by_prefix = HashMap::new();
        for (i, composer) in COMPOSERS.iter().enumerate() {
            let canonical = normalize(composer.name);
            for alias in std::iter::once(canonical.as_str()).chain(composer.full.iter().copied()) {
                by_artist.entry(alias.to_owned()).or_insert(i);
                by_prefix.entry(alias.to_owned()).or_insert(i);
            }
            for alias in composer.short {
                by_prefix.entry((*alias).to_owned()).or_insert(i);
            }
        }
        ComposerIndex {
            by_artist,
            by_prefix,
        }
    })
}

/// The composer credited by this artist name, if it is a known composer.
#[must_use]
pub fn composer_by_name(artist: &str) -> Option<&'static Composer> {
    index()
        .by_artist
        .get(&normalize(artist))
        .map(|&i| &COMPOSERS[i])
}

/// The composer named by a `Composer: …` prefix (title or album name). With
/// `allow_dash`, a `Composer - …` prefix also counts (common on album names).
#[must_use]
pub fn composer_by_prefix(text: &str, allow_dash: bool) -> Option<&'static Composer> {
    let head = match split_colon(text) {
        Some((head, _)) => head,
        None if allow_dash => text.split_once(" - ")?.0,
        None => return None,
    };
    composer_from_head(head)
}

fn composer_from_head(head: &str) -> Option<&'static Composer> {
    if head.len() > 60 {
        return None;
    }
    // "Bach, J.S.", "Beethoven / Liszt", "Mozart & Salieri": try the first name.
    let first = head.split([',', '/', '&']).next().unwrap_or(head);
    let idx = &index().by_prefix;
    idx.get(&normalize(head))
        .or_else(|| idx.get(&normalize(first)))
        .map(|&i| &COMPOSERS[i])
}

/// Lowercase, fold common Latin diacritics to ASCII, and collapse every run of
/// non-alphanumerics to one space: `"Dvořák: Symphony No. 9, Op.95"` →
/// `"dvorak symphony no 9 op 95"`. Matching everywhere runs on this form.
#[must_use]
pub fn normalize(s: &str) -> String {
    fn push(out: &mut String, text: &str, gap: &mut bool) {
        if *gap && !out.is_empty() {
            out.push(' ');
        }
        *gap = false;
        out.push_str(text);
    }
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for ch in s.chars().flat_map(char::to_lowercase) {
        if let Some(ascii) = fold(ch) {
            push(&mut out, ascii, &mut gap);
        } else if ch == '♭' || ch == '♯' {
            gap = true;
            push(&mut out, if ch == '♭' { "flat" } else { "sharp" }, &mut gap);
            gap = true;
        } else if ch.is_alphanumeric() {
            let mut buf = [0u8; 4];
            push(&mut out, ch.encode_utf8(&mut buf), &mut gap);
        } else {
            gap = true;
        }
    }
    out
}

fn fold(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => "i",
        'ł' | 'ľ' | 'ĺ' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'œ' => "oe",
        'ř' | 'ŕ' => "r",
        'ś' | 'š' | 'ş' | 'ș' => "s",
        'ß' => "ss",
        'ť' | 'ţ' | 'ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'þ' => "th",
        _ => return None,
    })
}

/// Whole-word (or whole-phrase) containment on [`normalize`]d text.
fn has_phrase(norm: &str, phrase: &str) -> bool {
    norm.match_indices(phrase).any(|(start, _)| {
        let end = start + phrase.len();
        (start == 0 || norm.as_bytes()[start - 1] == b' ')
            && (end == norm.len() || norm.as_bytes()[end] == b' ')
    })
}

fn has_any(norm: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|p| has_phrase(norm, p))
}

// ── Title anatomy ──────────────────────────────────────────────────────────

/// A track title split into its work and (optional) movement, e.g.
/// `"Symphony No. 5 in C Minor, Op. 67: I. Allegro con brio"` → work
/// `"Symphony No. 5 in C Minor, Op. 67"`, movement `"I. Allegro con brio"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleParts<'a> {
    pub work: &'a str,
    pub movement: Option<&'a str>,
}

/// Split a classical track title into work + movement. Drops a leading
/// `Composer:` prefix, trailing version notes (`- Remastered 2015`,
/// `(Live)`), and opera act/scene suffixes, so every movement of a work — and
/// every recording of it — shares one work string.
#[must_use]
pub fn split_title(title: &str) -> TitleParts<'_> {
    let mut rest = strip_version_suffix(title);
    if let Some((head, tail)) = split_colon(rest)
        && !tail.is_empty()
        && composer_from_head(head).is_some()
    {
        rest = tail;
    }
    let (work, movement) = match split_colon(rest) {
        Some((work, movement)) if !work.is_empty() && !movement.is_empty() => {
            (work, Some(movement))
        }
        _ => match rest.split_once(" - ") {
            Some((work, movement)) if starts_with_movement_marker(movement) => {
                (work.trim(), Some(movement.trim()))
            }
            _ => (rest, None),
        },
    };
    TitleParts {
        work: trim_act(work),
        movement,
    }
}

/// Split at the first colon that is followed by whitespace (or ends the
/// string), so `"4:33"` stays whole.
fn split_colon(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    s.match_indices(':')
        .find(|&(i, _)| bytes.get(i + 1).is_none_or(u8::is_ascii_whitespace))
        .map(|(i, _)| (s[..i].trim(), s[i + 1..].trim()))
}

const VERSION_WORDS: &[&str] = &[
    "remaster", "live", "version", "mono", "stereo", "recorded", "bonus", "edit", "arr", "transcr",
];

fn strip_version_suffix(title: &str) -> &str {
    let mut s = title.trim();
    loop {
        let cut = if let Some(open) = trailing_bracket(s) {
            is_version_note(&s[open..]).then(|| s[..open].trim_end())
        } else if let Some(pos) = s.rfind(" - ") {
            is_version_note(&s[pos + 3..]).then(|| s[..pos].trim_end())
        } else {
            None
        };
        match cut {
            Some(shorter) if !shorter.is_empty() => s = shorter,
            _ => return s,
        }
    }
}

fn trailing_bracket(s: &str) -> Option<usize> {
    let open = match s.chars().last()? {
        ')' => '(',
        ']' => '[',
        _ => return None,
    };
    s.rfind(open)
}

fn is_version_note(text: &str) -> bool {
    normalize(text)
        .split(' ')
        .any(|w| VERSION_WORDS.iter().any(|v| w.starts_with(v)))
}

fn starts_with_movement_marker(s: &str) -> bool {
    let first = s.split_whitespace().next().unwrap_or("");
    let numeral = first.trim_end_matches(['.', ':']);
    let roman = !numeral.is_empty()
        && numeral.len() <= 5
        && numeral.chars().all(|c| matches!(c, 'I' | 'V' | 'X'));
    roman || TEMPO_WORDS.contains(&normalize(first).as_str())
}

fn trim_act(work: &str) -> &str {
    let lower = work.to_ascii_lowercase();
    [
        ", act ", ", akt ", ", acte ", ", atto ", ", scene ", " / act ",
    ]
    .iter()
    .filter_map(|marker| lower.find(marker))
    .min()
    .map_or(work, |pos| work[..pos].trim_end())
}

// ── Classification signals ─────────────────────────────────────────────────

#[rustfmt::skip]
const FORM_WORDS: &[&str] = &[
    "symphony", "symphonies", "symphonie", "sinfonie", "sinfonia", "concerto", "concerti",
    "konzert", "sonata", "sonatas", "sonate", "sonatina", "quartet", "quartett", "quatuor",
    "quintet", "sextet", "octet", "trio", "suite", "suites", "partita", "prelude", "preludes",
    "praeludium", "fugue", "fuga", "nocturne", "nocturnes", "etude", "etudes", "mazurka",
    "mazurkas", "polonaise", "ballade", "impromptu", "scherzo", "rhapsody", "rhapsodie", "serenade",
    "divertimento", "cantata", "kantate", "oratorio", "mass", "missa", "requiem", "motet",
    "magnificat", "overture", "ouverture", "toccata", "chaconne", "ciaccona", "passacaglia",
    "variations", "variation", "variatio", "bagatelle", "bagatelles", "lieder", "lied", "madrigal",
    "opera", "concertante", "sinfonietta",
];

#[rustfmt::skip]
const TEMPO_WORDS: &[&str] = &[
    "allegro", "allegretto", "adagio", "adagietto", "andante", "andantino", "largo", "larghetto",
    "lento", "presto", "prestissimo", "vivace", "vivacissimo", "moderato", "menuetto", "minuet",
    "menuet", "minuetto", "rondo", "sostenuto", "maestoso", "cantabile", "scherzando", "grazioso",
    "affettuoso", "spiritoso",
];

/// Catalogue prefixes that identify a composer (`BWV 1007` → Bach).
const CATALOGUES: &[(&str, &str)] = &[
    ("bwv", "Johann Sebastian Bach"),
    ("buxwv", "Dieterich Buxtehude"),
    ("hwv", "George Frideric Handel"),
    ("rv", "Antonio Vivaldi"),
    ("twv", "Georg Philipp Telemann"),
    ("swv", "Heinrich Schütz"),
    ("hob", "Joseph Haydn"),
    ("kv", "Wolfgang Amadeus Mozart"),
    ("k", "Wolfgang Amadeus Mozart"),
    ("d", "Franz Schubert"),
    ("woo", "Ludwig van Beethoven"),
    ("sz", "Béla Bartók"),
    ("trv", "Richard Strauss"),
];

/// Catalogue-number pairs (`op 67`, `bwv 1007`) in normalized text. Haydn's
/// `Hob. XVI:52` takes a roman numeral, so `hob` accepts any following word.
fn catalogue_hits(norm: &str) -> impl Iterator<Item = &str> {
    let words: Vec<&str> = norm.split(' ').collect();
    let mut hits = Vec::new();
    for pair in words.windows(2) {
        let (word, next) = (pair[0], pair[1]);
        let numbered = next.starts_with(|c: char| c.is_ascii_digit());
        if (numbered || word == "hob")
            && (matches!(word, "op" | "opus") || CATALOGUES.iter().any(|(k, _)| *k == word))
        {
            hits.push(word);
        }
    }
    hits.into_iter()
}

fn has_catalogue(norm: &str) -> bool {
    catalogue_hits(norm).next().is_some()
}

fn catalogue_composer(norm: &str) -> Option<&'static Composer> {
    catalogue_hits(norm).find_map(|word| {
        let (_, name) = CATALOGUES.iter().find(|(k, _)| *k == word)?;
        composer_by_name(name)
    })
}

/// `in C minor`, `in E flat major`, or German `c moll` / `fis dur`.
fn has_key_signature(norm: &str) -> bool {
    const NOTES: &[&str] = &["a", "b", "c", "d", "e", "f", "g"];
    const GERMAN: &[&str] = &[
        "h", "cis", "dis", "fis", "gis", "ais", "es", "as", "des", "ges", "ces", "eis", "his",
    ];
    let words: Vec<&str> = norm.split(' ').collect();
    words.iter().enumerate().any(|(i, &w)| {
        let english = NOTES.contains(&w);
        if !english && !GERMAN.contains(&w) {
            return false;
        }
        let mut j = i + 1;
        if english && matches!(words.get(j), Some(&("flat" | "sharp"))) {
            j += 1;
        }
        match words.get(j) {
            Some(&("dur" | "moll")) => true,
            Some(&("major" | "minor")) => english && i > 0 && words[i - 1] == "in",
            _ => false,
        }
    })
}

#[rustfmt::skip]
const ENSEMBLE_STEMS: &[&str] = &[
    "orchest", "philharm", "sinfoni", "symphon", "ensemble", "quartet", "quatuor", "choir",
    "chorus", "kammer", "consort", "kapelle", "camerata", "concertgebouw", "gewandhaus",
    "collegium", "academy of", "baroque", "barock", "musica antiqua", "scholars",
];

fn is_ensemble(artist: &str) -> bool {
    let norm = normalize(artist);
    ENSEMBLE_STEMS.iter().any(|stem| norm.contains(stem))
}

#[rustfmt::skip]
const CLASSICAL_GENRE_STEMS: &[&str] = &[
    "classical", "baroque", "romantic era", "post romantic", "renaissance", "medieval",
    "impressionism", "neoclassic", "minimalism", "serialism", "opera", "choral", "chamber",
    "orchestra", "early music", "string quartet", "lieder", "requiem", "compositional ambient",
    "early modern",
];

fn is_classical_genre(genre: &str) -> bool {
    let norm = normalize(genre);
    CLASSICAL_GENRE_STEMS.iter().any(|stem| norm.contains(stem))
}

fn is_classical_label(label: &str) -> bool {
    const EXACT: &[&str] = &["bis", "cpo", "alpha", "archiv", "dg"];
    #[rustfmt::skip]
    const STEMS: &[&str] = &[
        "deutsche grammophon", "decca classics", "harmonia mundi", "hyperion", "naxos", "chandos",
        "erato", "warner classics", "sony classical", "ecm new series", "alpha classics",
        "channel classics", "pentatone", "ondine", "brilliant classics", "philips classics",
        "emi classics", "virgin classics", "rca red seal", "archiv produktion", "glossa",
        "ricercar", "challenge classics", "avie", "orfeo", "capriccio", "signum classics",
        "bis records",
    ];
    let norm = normalize(label);
    EXACT.contains(&norm.as_str()) || STEMS.iter().any(|stem| norm.contains(stem))
}

const GENRE_PERIODS: &[(&str, Period)] = &[
    ("medieval", Period::Medieval),
    ("renaissance", Period::Renaissance),
    ("baroque", Period::Baroque),
    ("classical era", Period::Classical),
    ("early romantic", Period::Romantic),
    ("late romantic", Period::LateRomantic),
    ("post romantic", Period::LateRomantic),
    ("romantic", Period::Romantic),
    ("impressionis", Period::Impressionist),
    ("early modern", Period::Modern),
    ("serialism", Period::Modern),
    ("neoclassicism", Period::Modern),
    ("contemporary classical", Period::Contemporary),
    ("minimalism", Period::Contemporary),
    ("compositional ambient", Period::Contemporary),
];

fn period_from_genres(genres: &[String]) -> Period {
    genres
        .iter()
        .map(|g| normalize(g))
        .find_map(|g| {
            GENRE_PERIODS
                .iter()
                .find(|(stem, _)| g.contains(stem))
                .map(|&(_, p)| p)
        })
        .unwrap_or(Period::Unknown)
}

/// Minimum [`classical_score`] for a track to count as classical on its own.
pub const CLASSICAL_THRESHOLD: i32 = 5;

/// Additive evidence that an item is classical. Each signal family counts
/// once: composer credit (+5 as an artist, +4 via title/album prefix or album
/// artist), catalogue number (+3), musical form (+2), key signature (+2),
/// tempo marking (+2), ensemble performer (+2), classical genre (+5; −4 when
/// genres are known and none is classical), classical label (+2), explicit
/// (−10). [`CLASSICAL_THRESHOLD`] is deliberately above any two weak signals,
/// so "Concerto for a Rainy Day" by Electric Light Orchestra stays out.
#[must_use]
pub fn classical_score(item: &LibraryItem) -> i32 {
    let title = normalize(&item.title);
    let album = item.album.as_deref().map(normalize).unwrap_or_default();
    let mut score = 0;
    if item.artists.iter().any(|a| composer_by_name(a).is_some()) {
        score += 5;
    } else if composer_by_prefix(&item.title, false).is_some()
        || item
            .album
            .as_deref()
            .is_some_and(|a| composer_by_prefix(a, true).is_some())
        || item
            .album_artists
            .iter()
            .any(|a| composer_by_name(a).is_some())
    {
        score += 4;
    }
    if has_catalogue(&title) || has_catalogue(&album) {
        score += 3;
    }
    if has_any(&title, FORM_WORDS) || has_any(&album, FORM_WORDS) {
        score += 2;
    }
    if has_key_signature(&title) || has_key_signature(&album) {
        score += 2;
    }
    if has_any(&title, TEMPO_WORDS) {
        score += 2;
    }
    if item
        .artists
        .iter()
        .chain(&item.album_artists)
        .any(|a| is_ensemble(a))
    {
        score += 2;
    }
    if !item.genres.is_empty() {
        score += if item.genres.iter().any(|g| is_classical_genre(g)) {
            5
        } else {
            -4
        };
    }
    if item.label.as_deref().is_some_and(is_classical_label) {
        score += 2;
    }
    if item.explicit {
        score -= 10;
    }
    score
}

/// Whether an item counts as classical on its own evidence.
#[must_use]
pub fn is_classical(item: &LibraryItem) -> bool {
    classical_score(item) >= CLASSICAL_THRESHOLD
}

// ── Energy ─────────────────────────────────────────────────────────────────

/// Tempo markings, characters and forms with an energy on a 0–100 scale,
/// most specific phrases first (a matched phrase is blanked so `allegro`
/// doesn't re-match inside `allegro molto`).
const ENERGY_TABLE: &[(&str, u8)] = &[
    ("ride of the valkyries", 92),
    ("dies irae", 90),
    ("allegro con brio", 82),
    ("allegro con fuoco", 85),
    ("allegro molto", 82),
    ("molto allegro", 82),
    ("allegro vivace", 82),
    ("allegro assai", 80),
    ("andante con moto", 48),
    ("con fuoco", 85),
    ("clair de lune", 22),
    ("pie jesu", 15),
    ("agnus dei", 25),
    ("ave maria", 18),
    ("requiem aeternam", 25),
    ("spiegel im spiegel", 10),
    ("hungarian dance", 78),
    ("prestissimo", 92),
    ("vivacissimo", 90),
    ("presto", 88),
    ("furioso", 88),
    ("tarantella", 85),
    ("galop", 85),
    ("csardas", 80),
    ("vivace", 80),
    ("agitato", 78),
    ("toccata", 78),
    ("scherzo", 75),
    ("march", 72),
    ("marcia", 72),
    ("gigue", 72),
    ("giga", 72),
    ("finale", 72),
    ("allegro", 70),
    ("overture", 70),
    ("ouverture", 70),
    ("polonaise", 70),
    ("rondo", 66),
    ("rhapsody", 65),
    ("bourree", 65),
    ("courante", 62),
    ("corrente", 62),
    ("concerto", 62),
    ("symphony", 60),
    ("etude", 60),
    ("allegretto", 58),
    ("gavotte", 58),
    ("fugue", 58),
    ("menuetto", 55),
    ("minuetto", 55),
    ("minuet", 55),
    ("menuet", 55),
    ("waltz", 55),
    ("valse", 55),
    ("moderato", 52),
    ("mazurka", 52),
    ("variation", 52),
    ("variations", 52),
    ("prelude", 50),
    ("sonata", 50),
    ("allemande", 48),
    ("quartet", 48),
    ("intermezzo", 45),
    ("andantino", 45),
    ("andante", 40),
    ("mass", 40),
    ("requiem", 40),
    ("aria", 38),
    ("barcarolle", 35),
    ("pastorale", 35),
    ("canon", 35),
    ("madrigal", 35),
    ("siciliano", 30),
    ("siciliana", 30),
    ("romance", 30),
    ("romanze", 30),
    ("motet", 30),
    ("sarabande", 25),
    ("sarabanda", 25),
    ("adagio", 25),
    ("larghetto", 25),
    ("grave", 25),
    ("nocturne", 25),
    ("notturno", 25),
    ("pavane", 25),
    ("antiphon", 25),
    ("air", 25),
    ("adagietto", 22),
    ("lento", 22),
    ("lament", 22),
    ("elegy", 22),
    ("elegie", 22),
    ("meditation", 22),
    ("reverie", 22),
    ("consolation", 22),
    ("gnossienne", 22),
    ("largo", 20),
    ("miserere", 20),
    ("chant", 20),
    ("berceuse", 15),
    ("lullaby", 15),
    ("wiegenlied", 15),
    ("gymnopedie", 15),
];

/// Estimate a track's energy (0 = still, 100 = blazing) from its movement's
/// tempo/character marking, falling back to the work's form; 50 when the
/// title says nothing. Several markings (`Adagio – Allegro`) average;
/// `ma non troppo` pulls toward the middle, `molto`/`assai` push outward.
#[must_use]
pub fn estimate_energy(movement: Option<&str>, work: &str) -> u8 {
    let from = |text: &str| -> Option<i32> {
        let mut norm = normalize(text);
        let mut matched = Vec::new();
        for &(phrase, energy) in ENERGY_TABLE {
            while let Some(start) = find_phrase(&norm, phrase) {
                matched.push(i32::from(energy));
                norm.replace_range(start..start + phrase.len(), &" ".repeat(phrase.len()));
            }
        }
        let count = i32::try_from(matched.len()).ok().filter(|&n| n > 0)?;
        let mut energy = matched.iter().sum::<i32>() / count;
        if has_phrase(&norm, "non troppo") {
            energy = i32::midpoint(energy, 55);
        }
        if has_phrase(&norm, "molto") || has_phrase(&norm, "assai") {
            energy = 50 + (energy - 50) * 5 / 4;
        }
        Some(energy)
    };
    let energy = movement.and_then(from).or_else(|| from(work)).unwrap_or(50);
    u8::try_from(energy.clamp(5, 95)).unwrap_or(50)
}

fn find_phrase(norm: &str, phrase: &str) -> Option<usize> {
    norm.match_indices(phrase).map(|(i, _)| i).find(|&start| {
        let end = start + phrase.len();
        (start == 0 || norm.as_bytes()[start - 1] == b' ')
            && (end == norm.len() || norm.as_bytes()[end] == b' ')
    })
}

// ── Analysis + the candidate pool ──────────────────────────────────────────

/// A library track as the DJ sees it: who wrote it, when, which work it
/// belongs to, and how energetic it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassicalTrack {
    pub track: Track,
    /// Display name: the canonical composer, or the lead artist if unknown.
    pub composer: String,
    /// Normalized grouping key for composer variety.
    pub composer_key: String,
    pub known_composer: bool,
    pub period: Period,
    /// Display name of the work (title minus movement).
    pub work: String,
    /// Grouping key for work variety: composer key + normalized work.
    pub work_key: String,
    /// Grouping key for album variety (empty when unknown).
    pub album_key: String,
    pub energy: u8,
    pub origin: Origin,
}

/// Analyse one library item. Does not decide whether it is classical — see
/// [`is_classical`] and [`CandidatePool::build`].
#[must_use]
pub fn analyze(item: &LibraryItem) -> ClassicalTrack {
    let composer = detect_composer(item);
    let (name, known) = match composer {
        Some(c) => (c.name.to_owned(), true),
        None => (
            item.artists
                .first()
                .or(item.album_artists.first())
                .cloned()
                .unwrap_or_default(),
            false,
        ),
    };
    let composer_key = normalize(&name);
    let parts = split_title(&item.title);
    ClassicalTrack {
        track: item.to_track(),
        work: parts.work.to_owned(),
        work_key: format!("{composer_key}|{}", normalize(parts.work)),
        composer: name,
        composer_key,
        known_composer: known,
        period: composer.map_or_else(|| period_from_genres(&item.genres), |c| c.period),
        album_key: item.album_key().unwrap_or_default(),
        energy: estimate_energy(parts.movement, parts.work),
        origin: item.origin,
    }
}

fn detect_composer(item: &LibraryItem) -> Option<&'static Composer> {
    item.artists
        .iter()
        .find_map(|a| composer_by_name(a))
        .or_else(|| composer_by_prefix(&item.title, false))
        .or_else(|| {
            item.album
                .as_deref()
                .and_then(|a| composer_by_prefix(a, true))
        })
        .or_else(|| item.album_artists.iter().find_map(|a| composer_by_name(a)))
        .or_else(|| catalogue_composer(&normalize(&item.title)))
        .or_else(|| {
            item.album
                .as_deref()
                .and_then(|a| catalogue_composer(&normalize(a)))
        })
}

/// A track that credits no known composer (`"Aria"` by the pianist alone)
/// takes the composer its album siblings unanimously credit.
fn adopt_album_composers(tracks: &mut [ClassicalTrack]) {
    // album key → the one known composer on it (None once two disagree).
    let mut credited: HashMap<String, Option<(String, String, Period)>> = HashMap::new();
    for t in tracks
        .iter()
        .filter(|t| t.known_composer && !t.album_key.is_empty())
    {
        let who = (t.composer.clone(), t.composer_key.clone(), t.period);
        credited
            .entry(t.album_key.clone())
            .and_modify(|seen| {
                if seen.as_ref().is_some_and(|s| s.1 != who.1) {
                    *seen = None;
                }
            })
            .or_insert(Some(who));
    }
    for t in tracks.iter_mut().filter(|t| !t.known_composer) {
        if let Some(Some((name, key, period))) = credited.get(&t.album_key) {
            t.work_key = format!("{key}|{}", normalize(&t.work));
            t.composer.clone_from(name);
            t.composer_key.clone_from(key);
            t.period = *period;
            t.known_composer = true;
        }
    }
}

/// The DJ's candidate pool: the owner's classical tracks, analysed, deduped
/// by `source_uri`, in a stable order (first sighting wins).
#[derive(Debug, Clone, Default)]
pub struct CandidatePool {
    tracks: Vec<ClassicalTrack>,
    by_uri: HashMap<String, usize>,
    composer_sizes: HashMap<String, usize>,
}

impl CandidatePool {
    /// Build the pool from saved-album tracks and liked tracks. A track is
    /// classical if its own evidence clears [`CLASSICAL_THRESHOLD`], or if at
    /// least half the library's tracks from its album do (so a bare "Aria" on
    /// a Goldberg Variations album comes along with its siblings). Explicit
    /// tracks never qualify. Duplicates (liked *and* on a saved album) merge.
    #[must_use]
    pub fn build(items: &[LibraryItem]) -> Self {
        let mut merged: Vec<LibraryItem> = Vec::new();
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for item in items {
            if let Some(&i) = seen.get(item.source_uri.as_str()) {
                merged[i].absorb(item);
            } else {
                seen.insert(&item.source_uri, merged.len());
                merged.push(item.clone());
            }
        }

        let scores: Vec<i32> = merged.iter().map(classical_score).collect();
        let album_keys: Vec<Option<String>> = merged.iter().map(LibraryItem::album_key).collect();
        let mut albums: HashMap<&str, (usize, usize)> = HashMap::new(); // (classical, total)
        for (key, &score) in album_keys.iter().zip(&scores) {
            if let Some(key) = key {
                let tally = albums.entry(key.as_str()).or_default();
                tally.1 += 1;
                if score >= CLASSICAL_THRESHOLD {
                    tally.0 += 1;
                }
            }
        }
        let album_is_classical = |key: &Option<String>| {
            key.as_deref()
                .and_then(|k| albums.get(k))
                .is_some_and(|&(classical, total)| classical * 2 >= total)
        };

        let mut tracks: Vec<ClassicalTrack> = merged
            .iter()
            .zip(&scores)
            .zip(&album_keys)
            .filter(|&((item, &score), key)| {
                !item.explicit && (score >= CLASSICAL_THRESHOLD || album_is_classical(key))
            })
            .map(|((item, _), _)| analyze(item))
            .collect();
        adopt_album_composers(&mut tracks);
        Self::from_tracks(tracks)
    }

    /// A pool from already-analysed tracks (e.g. a cached library), deduped by
    /// `source_uri` with origins merged.
    #[must_use]
    pub fn from_tracks(tracks: Vec<ClassicalTrack>) -> Self {
        let mut pool = Self::default();
        for track in tracks {
            if let Some(&i) = pool.by_uri.get(&track.track.source_uri) {
                let kept = &mut pool.tracks[i];
                kept.origin = kept.origin.merge(track.origin);
                continue;
            }
            *pool
                .composer_sizes
                .entry(track.composer_key.clone())
                .or_default() += 1;
            pool.by_uri
                .insert(track.track.source_uri.clone(), pool.tracks.len());
            pool.tracks.push(track);
        }
        pool
    }

    #[must_use]
    pub fn tracks(&self) -> &[ClassicalTrack] {
        &self.tracks
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    #[must_use]
    pub fn get(&self, source_uri: &str) -> Option<&ClassicalTrack> {
        self.index_of(source_uri).map(|i| &self.tracks[i])
    }

    /// Position of a track in [`Self::tracks`].
    #[must_use]
    pub fn index_of(&self, source_uri: &str) -> Option<usize> {
        self.by_uri.get(source_uri).copied()
    }

    /// How many pool tracks share this composer key.
    #[must_use]
    pub fn composer_size(&self, composer_key: &str) -> usize {
        self.composer_sizes.get(composer_key).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, artists: &[&str], album: Option<&str>) -> LibraryItem {
        LibraryItem {
            source_uri: format!("spotify:track:{}", normalize(title).replace(' ', "")),
            title: title.into(),
            artists: artists.iter().map(|s| (*s).to_owned()).collect(),
            album: album.map(str::to_owned),
            album_uri: None,
            album_artists: Vec::new(),
            genres: Vec::new(),
            label: None,
            duration_secs: Some(300),
            explicit: false,
            origin: Origin::SavedAlbum,
        }
    }

    #[test]
    fn normalize_folds_diacritics_and_punctuation() {
        assert_eq!(normalize("Antonín Dvořák"), "antonin dvorak");
        assert_eq!(normalize("Camille Saint-Saëns"), "camille saint saens");
        assert_eq!(normalize("J.S. Bach"), "j s bach");
        assert_eq!(normalize("  Op.67 —  No. 2 "), "op 67 no 2");
        assert_eq!(normalize("Hildur Guðnadóttir"), "hildur gudnadottir");
        assert_eq!(normalize("Straße"), "strasse");
        assert_eq!(normalize("Sonata in B♭ major"), "sonata in b flat major");
    }

    #[test]
    fn composers_match_artist_credits_and_aliases() {
        let name = |s| composer_by_name(s).map(|c| c.name);
        assert_eq!(name("Johann Sebastian Bach"), Some("Johann Sebastian Bach"));
        assert_eq!(name("J.S. Bach"), Some("Johann Sebastian Bach"));
        assert_eq!(name("Antonin Dvorak"), Some("Antonín Dvořák"));
        assert_eq!(name("Sergei Rachmaninov"), Some("Sergei Rachmaninoff"));
        assert_eq!(
            name("Carl Philipp Emanuel Bach"),
            Some("Carl Philipp Emanuel Bach")
        );
        assert_eq!(name("Arvo Pärt"), Some("Arvo Pärt"));
        // Bare surnames are not artist credits (no "Glass" the band, no "Field").
        assert_eq!(name("Glass"), None);
        assert_eq!(name("Glenn Gould"), None);
        assert_eq!(name("Ed Sheeran"), None);
    }

    #[test]
    fn composer_prefixes() {
        let pre = |s, dash| composer_by_prefix(s, dash).map(|c| c.name);
        assert_eq!(
            pre("Beethoven: Symphony No. 5", false),
            Some("Ludwig van Beethoven")
        );
        assert_eq!(
            pre("Bach, J.S.: Goldberg Variations", false),
            Some("Johann Sebastian Bach")
        );
        assert_eq!(pre("Pärt: Tabula Rasa", false), Some("Arvo Pärt"));
        assert_eq!(
            pre("Liszt / Wagner: Isoldes Liebestod", false),
            Some("Franz Liszt")
        );
        assert_eq!(pre("Carmen: Habanera", false), None);
        assert_eq!(pre("Part 1: The Beginning", false), None);
        assert_eq!(pre("Chopin - Nocturnes", true), Some("Frédéric Chopin"));
        assert_eq!(pre("Chopin - Nocturnes", false), None);
    }

    #[test]
    fn split_title_formats() {
        let s = |t| {
            let p = split_title(t);
            (p.work, p.movement)
        };
        assert_eq!(
            s("Symphony No. 5 in C Minor, Op. 67: I. Allegro con brio"),
            (
                "Symphony No. 5 in C Minor, Op. 67",
                Some("I. Allegro con brio")
            )
        );
        assert_eq!(
            s("Beethoven: Symphony No. 5 in C Minor, Op. 67: II. Andante con moto"),
            (
                "Symphony No. 5 in C Minor, Op. 67",
                Some("II. Andante con moto")
            )
        );
        assert_eq!(
            s("Requiem in D Minor, K. 626: III. Sequentia: No. 1, Dies irae"),
            (
                "Requiem in D Minor, K. 626",
                Some("III. Sequentia: No. 1, Dies irae")
            )
        );
        assert_eq!(
            s("Symphony No. 9 in D Minor, Op. 125 - IV. Presto"),
            ("Symphony No. 9 in D Minor, Op. 125", Some("IV. Presto"))
        );
        assert_eq!(
            s("Nocturne No. 2 in E-Flat Major, Op. 9 No. 2 - Remastered 2015"),
            ("Nocturne No. 2 in E-Flat Major, Op. 9 No. 2", None)
        );
        assert_eq!(
            s("Piano Sonata No. 14 \"Moonlight\": I. Adagio sostenuto (Live)"),
            (
                "Piano Sonata No. 14 \"Moonlight\"",
                Some("I. Adagio sostenuto")
            )
        );
        assert_eq!(
            s("La traviata, Act 1: Libiamo ne' lieti calici"),
            ("La traviata", Some("Libiamo ne' lieti calici"))
        );
        assert_eq!(s("Spiegel im Spiegel"), ("Spiegel im Spiegel", None));
        assert_eq!(s("4:33"), ("4:33", None));
        // A dash that isn't a movement or version note stays in the work.
        assert_eq!(s("Ma mère l'Oye - Suite"), ("Ma mère l'Oye - Suite", None));
    }

    #[test]
    fn movements_of_one_work_share_a_work_key() {
        let a = analyze(&item(
            "Cello Suite No. 1 in G Major, BWV 1007: I. Prélude",
            &["Johann Sebastian Bach", "Yo-Yo Ma"],
            None,
        ));
        let b = analyze(&item(
            "Cello Suite No. 1 in G Major, BWV 1007: VI. Gigue",
            &["Johann Sebastian Bach", "Pablo Casals"],
            None,
        ));
        let other = analyze(&item(
            "Cello Suite No. 2 in D Minor, BWV 1008: I. Prélude",
            &["Johann Sebastian Bach", "Yo-Yo Ma"],
            None,
        ));
        assert_eq!(a.work_key, b.work_key);
        assert_ne!(a.work_key, other.work_key);
        assert_eq!(a.composer, "Johann Sebastian Bach");
        assert_eq!(a.period, Period::Baroque);
        assert!(b.energy > a.energy, "gigue livelier than prelude");
    }

    #[test]
    fn composer_detection_fallbacks() {
        // Composer credited after the performers.
        let t = analyze(&item(
            "Clair de lune",
            &["Alexis Weissenberg", "Claude Debussy"],
            None,
        ));
        assert_eq!(
            (t.composer.as_str(), t.period),
            ("Claude Debussy", Period::Impressionist)
        );
        // Only the album names the composer.
        let t = analyze(&item(
            "Aria",
            &["Glenn Gould"],
            Some("Bach: Goldberg Variations"),
        ));
        assert_eq!(t.composer, "Johann Sebastian Bach");
        // Only a catalogue number identifies it.
        let t = analyze(&item(
            "Sonata in C Major, K. 545: I. Allegro",
            &["Mitsuko Uchida"],
            None,
        ));
        assert_eq!(t.composer, "Wolfgang Amadeus Mozart");
        let t = analyze(&item(
            "Keyboard Sonata, Hob. XVI:52",
            &["Alfred Brendel"],
            None,
        ));
        assert_eq!(t.composer, "Joseph Haydn");
        // Unknown composer: lead artist stands in, period from genres.
        let mut it = item("Lux aeterna", &["Some Composer", "Some Choir"], None);
        it.genres = vec!["early music".into(), "renaissance".into()];
        let t = analyze(&it);
        assert!(!t.known_composer);
        assert_eq!(
            (t.composer.as_str(), t.period),
            ("Some Composer", Period::Renaissance)
        );
    }

    #[test]
    fn energy_orders_tempo_markings() {
        let e = |m: &str| estimate_energy(Some(m), "Sonata");
        assert!(e("II. Largo") < e("II. Andante"));
        assert!(e("II. Andante") < e("I. Allegro"));
        assert!(e("I. Allegro") < e("IV. Presto"));
        assert!(e("I. Allegro ma non troppo") < e("I. Allegro"));
        assert!(e("IV. Allegro molto") > e("I. Allegro"));
        // No movement: the work's character decides.
        assert!(estimate_energy(None, "Nocturne No. 2") < 30);
        assert!(estimate_energy(None, "Hungarian Dance No. 5") > 70);
        assert_eq!(estimate_energy(None, "Fratres"), 50);
        // Slow intro into a fast body averages.
        let both = e("I. Adagio - Allegro vivace");
        assert!(e("Adagio") < both && both < e("Allegro vivace"));
    }

    #[test]
    fn classical_scoring() {
        let yes = |it: &LibraryItem| assert!(is_classical(it), "{it:?} {}", classical_score(it));
        let no = |it: &LibraryItem| assert!(!is_classical(it), "{it:?} {}", classical_score(it));
        yes(&item(
            "Gymnopédie No. 1",
            &["Erik Satie", "Jean-Yves Thibaudet"],
            None,
        ));
        yes(&item(
            "Symphony No. 7 in A Major, Op. 92: II. Allegretto",
            &["Wiener Philharmoniker", "Carlos Kleiber"],
            None,
        ));
        yes(&item(
            "Sonata in C Major: I. Allegro",
            &["Unknown Pianist"],
            None,
        ));
        yes(&item(
            "Variatio 1 a 1 Clav.",
            &["Glenn Gould"],
            Some("Bach: Goldberg Variations, BWV 988"),
        ));
        no(&item("Shape of You", &["Ed Sheeran"], Some("÷")));
        no(&item("Mambo No. 5", &["Lou Bega"], None));
        no(&item(
            "Bohemian Rhapsody",
            &["Queen"],
            Some("A Night at the Opera"),
        ));
        no(&item(
            "Concerto for a Rainy Day",
            &["Electric Light Orchestra"],
            None,
        ));
        let mut explicit = item("Requiem", &["Wolfgang Amadeus Mozart"], None);
        explicit.explicit = true;
        no(&explicit);
        let mut pop = item("Prelude", &["Some Band"], None);
        pop.genres = vec!["indie pop".into()];
        no(&pop);
        let mut neo = item("Experience", &["Ludovico Einaudi"], None);
        neo.genres = vec!["neoclassical".into()];
        yes(&neo);
    }

    #[test]
    fn key_signatures_and_catalogues() {
        assert!(has_key_signature(&normalize("Nocturne in E-flat Major")));
        assert!(has_key_signature(&normalize("Sinfonie Nr. 5 c-Moll")));
        assert!(has_key_signature(&normalize("Präludium fis-Moll")));
        assert!(!has_key_signature(&normalize("A Major Problem")));
        assert!(has_catalogue(&normalize("Partita No. 2, BWV 1004")));
        assert!(has_catalogue(&normalize("Op.10 No.3")));
        assert!(!has_catalogue(&normalize("Mambo No. 5")));
    }

    #[test]
    fn pool_keeps_classical_merges_duplicates_and_inherits_album_verdict() {
        let album_track = |title: &str, artists: &[&str]| {
            let mut it = item(title, artists, Some("Goldberg Variations"));
            it.album_uri = Some("spotify:album:goldberg".into());
            it.album_artists = vec!["Glenn Gould".into()];
            it
        };
        let bach_gould = ["Johann Sebastian Bach", "Glenn Gould"];
        // "Aria" credits only the pianist: on its own it scores just the album
        // form word (+2), but most of its album is classical, so it rides along.
        let aria = album_track("Aria", &["Glenn Gould"]);
        assert!(!is_classical(&aria));
        let var1 = album_track("Variatio 1 a 1 Clav.", &bach_gould);
        let var2 = album_track("Variatio 2 a 1 Clav.", &bach_gould);
        let mut liked_var1 = var1.clone();
        liked_var1.origin = Origin::LikedTrack;
        let pop = item("Shape of You", &["Ed Sheeran"], Some("÷"));
        let pop_hit = item("Bad Habits", &["Ed Sheeran"], Some("="));
        let pool =
            CandidatePool::build(&[aria.clone(), var1.clone(), var2, liked_var1, pop, pop_hit]);

        assert_eq!(pool.len(), 3);
        assert!(pool.get("spotify:track:shapeofyou").is_none());
        let v1 = pool.get(&var1.source_uri).unwrap();
        assert_eq!(v1.origin, Origin::Both);
        assert_eq!(v1.composer, "Johann Sebastian Bach");
        assert_eq!(pool.index_of(&var1.source_uri), Some(1));
        // Aria credits only the pianist; its album siblings vouch for Bach.
        let aria = pool.get(&aria.source_uri).unwrap();
        assert_eq!(
            (aria.composer.as_str(), aria.period),
            ("Johann Sebastian Bach", Period::Baroque)
        );
        assert_eq!(aria.work_key, "johann sebastian bach|aria");
        assert_eq!(pool.composer_size("johann sebastian bach"), 3);
    }
}
