//! Refresh what the owner heard on Spotify without rereading their library.
//!
//! Each pass reads one latest-50 window, bracketed by account identity checks.
//! It does not traverse older history or follow the response's continuation URL.
//! Listening outside that window can be missed between passes; this is a recent
//! observation for anti-repeat, not a complete listening-history import.
//!
//! Accepted observations merge monotonically with the current adopted account's
//! heard tracks. A repeat keeps its latest timestamp, an empty window erases
//! nothing, and a concurrent full refresh cannot replace newer heard evidence.
//! The store retains at most the latest 10,000 distinct tracks. Account changes
//! still clear that personal evidence through the existing adoption mechanism.
//! No library rows, taste weights, full-sync timestamp or speaker state change.

use asupersync::Cx;
use fsonos_core::store::Store;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::SpotifyError;
use crate::client::{decode, unix_seconds};
use crate::session::Session;

/// Maximum history rows read in one pass, matching the existing recent endpoint.
pub const RECENT_WINDOW: usize = 50;
/// The optional read-only grant used by this pass.
pub const RECENT_SCOPE: &str = "user-read-recently-played";
/// Small clock differences are tolerated; far-future plays cannot suppress a
/// track indefinitely through a monotonic history merge.
const CLOCK_SKEW_SECS: i64 = 5 * 60;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// Compact status for one lightweight observation. A successful `checked_at`
/// describes the latest window only, never complete historical coverage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentListening {
    pub owner: Option<String>,
    pub generation: Option<u64>,
    /// The caller's Unix time, set only after the ownership-fenced merge accepts.
    pub checked_at: Option<i64>,
    /// Rows returned by Spotify, including repeated and unavailable tracks.
    pub received: usize,
    /// Distinct Spotify track identities accepted as heard, including an identity
    /// whose timestamp was already newer in the store. Not a new-row count.
    pub accepted: usize,
    /// Unavailable, local-file or non-track rows with no usable Spotify identity.
    pub skipped: usize,
    /// An older window was advertised, or this window reached its 50-row limit.
    /// False does not establish complete listening history.
    pub more_available: bool,
    /// This grant lacks the optional recent-listening scope; no HTTP read ran.
    pub missing_scope: bool,
    /// The personal cache needs the current account's full refresh before this
    /// reader may publish. This pass never adopts or switches the account.
    pub requires_refresh: bool,
    /// Shared server cooldown still applying to the session's later reads.
    pub retry_after_secs: u64,
}

#[derive(Deserialize)]
struct RecentPage {
    // Missing `items` is malformed, not a successfully observed empty history.
    items: Vec<RecentPlay>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct RecentPlay {
    played_at: String,
    #[serde(default)]
    track: Option<RecentTrack>,
}

/// Heard-state needs only identity. Missing titles, albums, explicit flags or
/// current market availability cannot undo an observed historical listen.
#[derive(Deserialize)]
struct RecentTrack {
    #[serde(default)]
    uri: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    is_local: bool,
}

struct Observation {
    plays: Vec<(String, i64)>,
    received: usize,
    skipped: usize,
    more_available: bool,
}

fn invalid(detail: &str) -> SpotifyError {
    SpotifyError::Decode(format!("recent listening: {detail}"))
}

fn observation(body: &[u8], now: i64) -> Result<Observation, SpotifyError> {
    if body.len() > MAX_PAGE_BYTES {
        return Err(invalid(
            "the latest window exceeds the 2 MiB response bound",
        ));
    }
    let page: RecentPage = decode(body, "recent listening")?;
    if page.items.len() > RECENT_WINDOW {
        return Err(invalid("the latest window exceeds 50 history rows"));
    }
    let received = page.items.len();
    let more_available =
        received == RECENT_WINDOW || page.next.as_deref().is_some_and(|next| !next.is_empty());
    let mut heard = BTreeMap::<String, i64>::new();
    let mut skipped = 0;
    for item in page.items {
        let at = unix_seconds(Some(&item.played_at))
            .filter(|&at| at >= 0 && at <= now.saturating_add(CLOCK_SKEW_SECS))
            .ok_or_else(|| invalid("a history row has a malformed or future played_at"))?;
        let Some(track) = item.track else {
            skipped += 1;
            continue;
        };
        let Some(uri) = track.uri.filter(|_| {
            !track.is_local && track.kind.as_deref().is_none_or(|kind| kind == "track")
        }) else {
            skipped += 1;
            continue;
        };
        if !uri.strip_prefix("spotify:track:").is_some_and(|id| {
            !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }) {
            return Err(invalid(
                "a nonlocal history track has an invalid Spotify URI",
            ));
        }
        heard
            .entry(uri)
            .and_modify(|previous| *previous = (*previous).max(at))
            .or_insert(at);
    }
    Ok(Observation {
        plays: heard.into_iter().collect(),
        received,
        skipped,
        more_available,
    })
}

/// Full-library taste reads publish through the same monotonic heard-state
/// merge as the lightweight reader. Validate their whole window too: a future
/// timestamp must never become permanent evidence through another intake path.
pub(crate) fn validated_plays(body: &[u8], now: i64) -> Result<Vec<(String, i64)>, SpotifyError> {
    Ok(observation(body, now)?.plays)
}

/// Refresh anti-repeat evidence from one latest-50 history window, with at most
/// three logical HTTP reads: identity, recent plays, then identity again. The
/// session's existing bounded retries, authentication handling and server
/// cooldown apply to each read. The caller must retain that cooldown on error
/// as well, through [`Session::retry_after_secs`].
///
/// A missing optional scope is a quiet status, not an authentication failure.
/// An unadopted or changed account requests a full refresh without publishing.
/// Exact account generation is checked around I/O and atomically at merge, so
/// delayed results from an earlier A→B→A adoption cannot enter the new cache.
/// Timestamps over five minutes ahead of `now` reject the whole observation.
/// Missing, malformed and failed windows never erase previous heard-state.
///
/// The daemon can call this before its existing 15-minute enrichment check,
/// including during unfinished full intake. It need not rebuild the DJ pool:
/// the next selection already reads current heard-state from the store.
pub async fn refresh_recent_listening<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    now: i64,
) -> Result<RecentListening, SpotifyError> {
    if now < 0 {
        return Err(SpotifyError::Config(
            "recent listening needs a nonnegative observation time".into(),
        ));
    }
    if !session.is_authorized() {
        return Err(SpotifyError::Auth("not signed in to Spotify".into()));
    }
    let mut report = RecentListening {
        retry_after_secs: session.retry_after_secs(),
        ..RecentListening::default()
    };
    if session.missing_taste_scopes().contains(&RECENT_SCOPE) {
        report.missing_scope = true;
        return Ok(report);
    }
    let Some(receipt) = store.spotify_library_owner()? else {
        report.requires_refresh = true;
        return Ok(report);
    };
    report.owner = Some(receipt.owner_id.clone());
    report.generation = Some(receipt.generation);
    let owner = session.read_owner(cx).await?;
    if owner != receipt.owner_id || store.spotify_library_owner()?.as_ref() != Some(&receipt) {
        report.requires_refresh = true;
        return Ok(report);
    }
    let url = session.endpoints().recently_played();
    let observed = observation(&session.get_brief(cx, &url).await?, now)?;
    report.received = observed.received;
    report.skipped = observed.skipped;
    report.more_available = observed.more_available;
    if store.spotify_library_owner()?.as_ref() != Some(&receipt) {
        report.requires_refresh = true;
        return Ok(report);
    }
    let owner = session.read_owner(cx).await?;
    report.retry_after_secs = session.retry_after_secs();
    if owner != receipt.owner_id || !store.merge_spotify_heard(&receipt, &observed.plays)? {
        report.requires_refresh = true;
        return Ok(report);
    }
    report.checked_at = Some(now);
    report.accepted = observed.plays.len();
    Ok(report)
}

#[cfg(test)]
mod tests;
