//! Sonos favorites (My Sonos): list them, find one by name, play it.
//!
//! A favorite carries a working URI and DIDL-Lite metadata for its own
//! household, so playing one needs no learned render parameters. That makes
//! favorites the first real playback on both S1 and S2, and the fallback when
//! a Spotify track won't render. Tracks and streams go straight to the
//! renderer; albums and playlists (containers) replace the group's queue and
//! play it from the top. Shortcuts (artist pages; their `<res/>` is empty)
//! open a browse view in the Sonos app and cannot be played.

use crate::{CoreError, HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_proto::content;
use fsonos_proto::control as soap;
use fsonos_proto::didl::DidlObject;
use fsonos_types::PlayerId;
use fsonos_types::text::normalize;
use std::fmt;

/// How a favorite plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FavoriteKind {
    /// One track: rendered directly.
    Track,
    /// A radio or other live stream: rendered directly.
    Stream,
    /// An album or playlist: replaces the queue.
    Container,
    /// A shortcut with nothing to render.
    Unplayable,
}

/// One entry of a household's `FV:2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Favorite {
    /// The favorite's object id (`FV:2/<n>`).
    pub id: String,
    pub title: String,
    pub kind: FavoriteKind,
    /// The renderer URI, absent for shortcuts.
    pub uri: Option<String>,
    /// The item's own DIDL-Lite (the favorite's `r:resMD`), sent with the URI.
    pub metadata: String,
    /// What the Sonos app shows under the title ("By <artist>", a station).
    pub description: Option<String>,
    pub art_uri: Option<String>,
}

/// Classify one `FV:2` object.
#[must_use]
pub fn classify(o: &DidlObject) -> Favorite {
    let uri = o
        .res
        .as_ref()
        .map(|r| r.uri.trim().to_string())
        .filter(|u| !u.is_empty());
    let metadata = o.res_md.clone().unwrap_or_default();
    let item_class = o
        .res_md_object()
        .ok()
        .flatten()
        .map(|item| item.class)
        .unwrap_or_default();
    let kind = match uri.as_deref() {
        None => FavoriteKind::Unplayable,
        Some(u) if u.starts_with("x-rincon-cpcontainer:") => FavoriteKind::Container,
        Some(u)
            if [
                "x-rincon-mp3radio:",
                "x-sonosapi-stream:",
                "x-sonosapi-radio:",
                "x-sonosapi-hls:",
                "aac:",
            ]
            .iter()
            .any(|scheme| u.starts_with(scheme))
                || item_class.starts_with("object.item.audioItem.audioBroadcast") =>
        {
            FavoriteKind::Stream
        }
        Some(_) => FavoriteKind::Track,
    };
    Favorite {
        id: o.id.clone(),
        title: o.title.clone(),
        kind,
        uri,
        metadata,
        description: o.favorite_description.clone(),
        art_uri: o.album_art_uri.clone(),
    }
}

/// The favorites of the household `coordinator` belongs to.
pub fn list<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<Vec<Favorite>, CoreError> {
    let host = control::locate(households, coordinator)?.ip;
    Ok(content::browse_all(t, host, "FV:2")?
        .iter()
        .map(classify)
        .collect())
}

/// Why a favorite could not be found or played.
#[derive(Debug)]
pub enum FavoriteError {
    /// Nothing matches; `suggestions` are the closest titles.
    Unknown {
        query: String,
        suggestions: Vec<String>,
    },
    /// Several favorites match equally well.
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
    /// The favorite is a shortcut with nothing to play.
    Unplayable {
        title: String,
    },
    Core(CoreError),
}

impl fmt::Display for FavoriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { query, suggestions } if suggestions.is_empty() => {
                write!(f, "no favorite matches {query:?}")
            }
            Self::Unknown { query, suggestions } => write!(
                f,
                "no favorite matches {query:?}; did you mean: {}",
                suggestions.join(", ")
            ),
            Self::Ambiguous { query, candidates } => write!(
                f,
                "{query:?} matches several favorites: {}",
                candidates.join(", ")
            ),
            Self::Unplayable { title } => write!(
                f,
                "favorite {title:?} is a shortcut (an artist or browse page) and cannot be played"
            ),
            Self::Core(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FavoriteError {}

impl From<CoreError> for FavoriteError {
    fn from(e: CoreError) -> Self {
        Self::Core(e)
    }
}

/// Find a favorite the way a person or agent names it: by its id
/// (`FV:2/<n>`, as listings hand it out), its 1-based position, or by title:
/// exact first, then a unique title prefix, then a unique title containing
/// every word of the query (case, accents and punctuation ignored).
pub fn find<'a>(favorites: &'a [Favorite], query: &str) -> Result<&'a Favorite, FavoriteError> {
    if let Some(f) = favorites.iter().find(|f| f.id == query.trim()) {
        return Ok(f);
    }
    if let Ok(n) = query.trim().parse::<usize>()
        && let Some(f) = n.checked_sub(1).and_then(|i| favorites.get(i))
    {
        return Ok(f);
    }
    let q = normalize(query);
    let words: Vec<&str> = q.split(' ').filter(|w| !w.is_empty()).collect();
    let norm: Vec<String> = favorites.iter().map(|f| normalize(&f.title)).collect();
    let pick = |matches: Vec<usize>| -> Option<Result<&'a Favorite, FavoriteError>> {
        match matches.as_slice() {
            [] => None,
            [i] => Some(Ok(&favorites[*i])),
            many => Some(Err(FavoriteError::Ambiguous {
                query: query.to_string(),
                candidates: many.iter().map(|&i| favorites[i].title.clone()).collect(),
            })),
        }
    };
    let by =
        |test: &dyn Fn(&str) -> bool| (0..favorites.len()).filter(|&i| test(&norm[i])).collect();
    if let Some(found) = pick(by(&|t| !q.is_empty() && t == q))
        .or_else(|| pick(by(&|t| !q.is_empty() && t.starts_with(&q))))
        .or_else(|| {
            pick(by(&|t| {
                !words.is_empty() && words.iter().all(|w| t.split(' ').any(|tw| tw == *w))
            }))
        })
    {
        return found;
    }
    // Suggest titles sharing any word with the query.
    let suggestions = (0..favorites.len())
        .filter(|&i| words.iter().any(|w| norm[i].split(' ').any(|tw| tw == *w)))
        .map(|i| favorites[i].title.clone())
        .take(3)
        .collect();
    Err(FavoriteError::Unknown {
        query: query.to_string(),
        suggestions,
    })
}

/// Play `favorite` in the group `coordinator` leads.
pub fn play<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    favorite: &Favorite,
) -> Result<(), FavoriteError> {
    let Some(uri) = favorite.uri.as_deref() else {
        return Err(FavoriteError::Unplayable {
            title: favorite.title.clone(),
        });
    };
    match favorite.kind {
        FavoriteKind::Unplayable => Err(FavoriteError::Unplayable {
            title: favorite.title.clone(),
        }),
        FavoriteKind::Track | FavoriteKind::Stream => Ok(control::play_uri(
            t,
            households,
            coordinator,
            uri,
            &favorite.metadata,
        )?),
        FavoriteKind::Container => {
            let host = control::locate(households, coordinator)?.ip;
            soap::remove_all_tracks_from_queue(t, host).map_err(CoreError::from)?;
            let first = soap::add_uri_to_queue(t, host, uri, &favorite.metadata, false)
                .map_err(CoreError::from)?;
            Ok(control::play_queue_from(t, households, coordinator, first)?)
        }
    }
}
