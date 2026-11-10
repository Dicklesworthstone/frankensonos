//! A bounded, read-only episode selection for an explicitly requested show.
//!
//! A show id is a catalog container, not an episode Sonos can start. Read its
//! first embedded episode page before any speaker mutation, preserving Spotify's
//! order without claiming that it means newest, unplayed or complete. Podcast
//! metadata stays separate from the music library and the DJ's preferences.

use std::collections::HashSet;

use asupersync::Cx;
use serde::Deserialize;

use crate::SpotifyError;
use crate::client::{Paging, decode};
use crate::session::Session;

/// At most this many distinct playable episodes are returned.
pub const MAX_EPISODES: usize = 10;
/// Accept only the first, bounded episode page; never follow its `next` link.
pub const EPISODE_PAGE_LIMIT: u32 = 50;

/// One concrete episode, ready for Sonos' existing Spotify episode renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowEpisode {
    pub episode_uri: String,
    pub title: String,
    pub duration_secs: u32,
    pub explicit: bool,
}

/// A validated provider-ordered sample, not a full show or an unplayed queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowPlayback {
    pub show_uri: String,
    pub title: String,
    pub episodes: Vec<ShowEpisode>,
}

// Get Show includes both the parent identity and the first episode page. A
// standalone episodes page cannot prove which show its entries belong to.
#[derive(Debug, Deserialize)]
struct Show {
    id: String,
    uri: String,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    total_episodes: u32,
    episodes: Paging<Episode>,
}

#[derive(Debug, Deserialize)]
struct Episode {
    id: String,
    uri: String,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    duration_ms: u64,
    explicit: bool,
    is_playable: bool,
    #[serde(default)]
    restrictions: Option<Restrictions>,
}

// Any restriction, including an unknown future reason, makes this row unsafe
// to enqueue. Do not require or interpret the optional reason string.
#[derive(Debug, Deserialize)]
struct Restrictions {}

/// Resolve a canonical `spotify:show:<id>` URI or a bare 22-character show id
/// to at most [`MAX_EPISODES`] playable episodes in Spotify's returned order.
/// This reads one show document using [`Session::get_brief`]'s existing retry,
/// refresh and cooldown policy. It neither follows paging URLs nor requests
/// listening history, downloads audio, changes preferences or starts playback.
///
/// Missing authorization, API refusal, an empty selection and malformed data
/// all fail before returning a selection. No extra OAuth scopes are requested.
pub async fn resolve(
    session: &mut Session,
    cx: &Cx,
    show_uri: &str,
) -> Result<ShowPlayback, SpotifyError> {
    let id = show_uri.strip_prefix("spotify:show:").unwrap_or(show_uri);
    if !valid_id(id) {
        return Err(SpotifyError::Config(
            "show playback requires a Spotify show id of 22 ASCII letters or digits".into(),
        ));
    }
    let url = session.endpoints().show(id);
    let body = session.get_brief(cx, &url).await?;
    let show: Show = decode(&body, "show episodes")?;
    check_identity(&show.id, &show.uri, &show.kind, "show")?;
    if show.id != id {
        return Err(SpotifyError::Decode(
            "show playback received a different Spotify show".into(),
        ));
    }
    check_title(&show.name)?;
    check_page(&show)?;
    let mut episodes = Vec::new();
    let mut seen = HashSet::new();
    for episode in show.episodes.items {
        if !episode.is_playable || episode.restrictions.is_some() {
            continue;
        }
        check_identity(&episode.id, &episode.uri, &episode.kind, "episode")?;
        check_title(&episode.name)?;
        if !seen.insert(episode.uri.clone()) {
            continue;
        }
        let duration_secs = u32::try_from(episode.duration_ms.div_ceil(1000)).map_err(|_| {
            SpotifyError::Decode("show playback received an invalid episode duration".into())
        })?;
        episodes.push(ShowEpisode {
            episode_uri: episode.uri,
            title: episode.name,
            duration_secs,
            explicit: episode.explicit,
        });
        if episodes.len() == MAX_EPISODES {
            break;
        }
    }
    if episodes.is_empty() {
        return Err(SpotifyError::Decode(
            "no playable episodes for this show in the bounded Spotify selection".into(),
        ));
    }
    Ok(ShowPlayback {
        show_uri: show.uri,
        title: show.name,
        episodes,
    })
}

fn valid_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn check_identity(id: &str, uri: &str, kind: &str, expected: &str) -> Result<(), SpotifyError> {
    if valid_id(id) && kind == expected && uri == format!("spotify:{expected}:{id}") {
        Ok(())
    } else {
        Err(SpotifyError::Decode(format!(
            "show playback received a malformed or inconsistent Spotify {expected} identity"
        )))
    }
}

fn check_title(title: &str) -> Result<(), SpotifyError> {
    if title.trim().is_empty() || title.len() > 4096 || title.chars().any(char::is_control) {
        Err(SpotifyError::Decode(
            "show playback received an invalid catalog title".into(),
        ))
    } else {
        Ok(())
    }
}

fn check_page(show: &Show) -> Result<(), SpotifyError> {
    let page = &show.episodes;
    if page.offset != 0
        || !(1..=EPISODE_PAGE_LIMIT).contains(&page.limit)
        || page.items.len() > page.limit as usize
        || page.items.len() > page.total as usize
        || page.items.len() > show.total_episodes as usize
    {
        Err(SpotifyError::Decode(
            "show playback received inconsistent episode page counters".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
