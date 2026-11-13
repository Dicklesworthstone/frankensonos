//! Fair artist sampling and the finite backlog of an accepted full refresh.
//! Exact receipts fence additive publication against newer account reads.

use super::{Artist, MAX_ARTIST_READS, MAX_TASTE_PM};
use crate::SpotifyError;
use crate::client::random_state;
use fsonos_core::store::{SpotifyArtistDiscoveryFence, SpotifyLibraryOwner};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Artist samples attempted by one full or background pass.
pub const ARTIST_SAMPLE_BUDGET: usize = MAX_ARTIST_READS;
/// The followed-directory bound plus twenty top-artist pages of fifty.
pub const ARTIST_DISCOVERY_MAX_ARTISTS: usize = 101_000;
/// Private metadata only; artist names, tokens and track payloads are omitted.
pub const ARTIST_DISCOVERY_MAX_BYTES: usize =
    fsonos_core::store::MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES;
/// A stale cohort waits for a fresh account/library read instead of extending it.
pub const ARTIST_DISCOVERY_MAX_AGE_SECS: i64 = 7 * 86_400;

/// Compact scheduling status. The finite backlog itself stays in the store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtistContinuation {
    pub owner: Option<String>,
    pub generation: Option<u64>,
    /// Identifies one accepted full read's cohort, independently of publications.
    pub cycle: Option<String>,
    pub pending: usize,
    /// Successful artist samples in this cycle, including its first full pass.
    pub completed: usize,
    /// Actual artist attempts in this pass, including failures.
    pub attempted: usize,
    /// The captured cohort exceeded its count or byte bound; automatic reads stop.
    pub limited: bool,
    pub retry_after_secs: u64,
    pub requires_refresh: bool,
}

impl ArtistContinuation {
    #[must_use]
    pub fn refresh_required() -> Self {
        Self {
            requires_refresh: true,
            ..Self::default()
        }
    }

    pub(crate) fn matches(&self, owner: &SpotifyLibraryOwner) -> bool {
        self.owner.as_deref() == Some(owner.owner_id.as_str())
            && self.generation == Some(owner.generation)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cohort {
    id: String,
    created_at: i64,
    scopes: u8,
    /// Sorted stable IDs and captured weights, including already covered artists
    /// so a guest-credit sample can still weigh its actual lead correctly.
    signals: Vec<(String, u32)>,
    remaining: Vec<usize>,
    completed: usize,
    limited: bool,
    omitted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    version: u8,
    last_artist_id: Option<String>,
    publication: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cohort: Option<Cohort>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// An invalid scheduling hint may be replaced by a successful full refresh.
/// The caller reports that recovery and retains all previous taste candidates.
pub(super) fn cursor(raw: Option<&str>) -> Result<Option<String>, SpotifyError> {
    let Some(raw) = raw else { return Ok(None) };
    Ok(Progress::decode(raw)?.last_artist_id)
}

impl Progress {
    pub(crate) fn decode(raw: &str) -> Result<Self, SpotifyError> {
        let invalid = || SpotifyError::Decode("invalid artist discovery cursor".into());
        if raw.len() > ARTIST_DISCOVERY_MAX_BYTES {
            return Err(invalid());
        }
        let progress: Self = serde_json::from_str(raw).map_err(|_| invalid())?;
        if !matches!(
            (progress.version, &progress.cohort),
            (1, None) | (2, Some(_))
        ) || progress.publication.is_empty()
            || progress.publication.len() > 128
            || progress.publication.chars().any(char::is_control)
            || progress
                .last_artist_id
                .as_deref()
                .is_some_and(|id| !valid_id(id))
        {
            return Err(invalid());
        }
        if let Some(cohort) = &progress.cohort {
            cohort.validate()?;
        }
        Ok(progress)
    }

    pub(crate) fn report(&self, owner: &SpotifyLibraryOwner, now: i64) -> ArtistContinuation {
        let Some(cohort) = &self.cohort else {
            return ArtistContinuation::refresh_required();
        };
        ArtistContinuation {
            owner: Some(owner.owner_id.clone()),
            generation: Some(owner.generation),
            cycle: Some(cohort.id.clone()),
            pending: cohort.remaining.len().saturating_add(cohort.omitted),
            completed: cohort.completed,
            limited: cohort.limited,
            requires_refresh: now < cohort.created_at
                || now.saturating_sub(cohort.created_at) >= ARTIST_DISCOVERY_MAX_AGE_SECS,
            ..ArtistContinuation::default()
        }
    }

    pub(crate) fn permits_scopes(&self, missing: &[&str]) -> bool {
        self.cohort.as_ref().is_some_and(|cohort| {
            (cohort.scopes & 1 == 0 || !missing.contains(&"user-follow-read"))
                && (cohort.scopes & 2 == 0 || !missing.contains(&"user-top-read"))
        })
    }

    pub(crate) fn selection(&self) -> Vec<Artist> {
        let Some(cohort) = &self.cohort else {
            return Vec::new();
        };
        if cohort.limited {
            return Vec::new();
        }
        let artists: Vec<_> = cohort
            .remaining
            .iter()
            .map(|index| Artist {
                id: Some(cohort.signals[*index].0.clone()),
                name: cohort.signals[*index].0.clone(),
                genres: Vec::new(),
            })
            .collect();
        select(artists.iter().collect(), self.last_artist_id.as_deref())
            .0
            .into_iter()
            .cloned()
            .collect()
    }

    pub(crate) fn accepted(&mut self, successful: &[String], last_attempted: Option<&str>) {
        let Some(cohort) = &mut self.cohort else {
            return;
        };
        let successful: HashSet<_> = successful.iter().map(String::as_str).collect();
        let before = cohort.remaining.len();
        cohort
            .remaining
            .retain(|index| !successful.contains(cohort.signals[*index].0.as_str()));
        cohort.completed += before - cohort.remaining.len();
        if let Some(last) = last_attempted {
            self.last_artist_id = Some(last.to_owned());
        }
    }

    pub(crate) fn weigh(&self, items: &mut [crate::library::LibraryItem]) {
        let Some(cohort) = &self.cohort else { return };
        for item in items {
            item.taste_pm = Some(1000);
            if let Some(id) = &item.artist_id
                && let Ok(index) = cohort
                    .signals
                    .binary_search_by(|(candidate, _)| candidate.cmp(id))
            {
                item.taste_pm = Some(cohort.signals[index].1);
            }
        }
    }

    pub(crate) fn publication(
        &self,
        expected: &str,
    ) -> Result<SpotifyArtistDiscoveryFence, SpotifyError> {
        prepare_publication(
            Some(expected),
            self.last_artist_id.as_deref(),
            self.cohort.as_ref(),
        )
    }
}

impl Cohort {
    pub(super) fn seed(
        weights: &HashMap<String, u32>,
        pending: &[String],
        completed: usize,
        scopes: u8,
        now: i64,
    ) -> Self {
        let mut signals: Vec<_> = weights
            .iter()
            .filter(|(id, _)| valid_id(id))
            .map(|(id, weight)| (id.clone(), (*weight).min(MAX_TASTE_PM)))
            .collect();
        signals.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let pending: HashSet<_> = pending.iter().map(String::as_str).collect();
        let remaining = signals
            .iter()
            .enumerate()
            .filter_map(|(index, (id, _))| pending.contains(id.as_str()).then_some(index))
            .collect();
        Self {
            id: String::new(),
            created_at: now,
            scopes,
            signals,
            remaining,
            completed,
            limited: false,
            omitted: 0,
        }
    }

    fn validate(&self) -> Result<(), SpotifyError> {
        let invalid = || SpotifyError::Decode("invalid artist discovery cohort".into());
        if self.id.is_empty()
            || self.id.len() > 128
            || self.id.chars().any(char::is_control)
            || self.scopes > 3
            || self.signals.len() > ARTIST_DISCOVERY_MAX_ARTISTS
            || self.remaining.len() > self.signals.len()
            || self.completed > ARTIST_DISCOVERY_MAX_ARTISTS
            || self
                .signals
                .iter()
                .any(|(id, weight)| !valid_id(id) || !(1000..=MAX_TASTE_PM).contains(weight))
            || self.signals.windows(2).any(|pair| pair[0].0 >= pair[1].0)
            || self
                .remaining
                .iter()
                .any(|index| *index >= self.signals.len())
            || self.remaining.windows(2).any(|pair| pair[0] >= pair[1])
            || (!self.limited
                && (self.omitted != 0
                    || self.completed + self.remaining.len() > self.signals.len()))
            || (self.limited && (!self.signals.is_empty() || !self.remaining.is_empty()))
            || (self.scopes == 0 && (!self.signals.is_empty() || self.completed > 0))
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn limit(&mut self) {
        self.omitted = self.remaining.len();
        self.remaining.clear();
        self.signals.clear();
        self.limited = true;
    }
}

/// Sort by stable identity, resume strictly after the accepted cursor and wrap.
/// Missing or newly covered cursor artists need no special case. New names or
/// response ordering cannot reset the rotation or hide identically named artists.
pub(super) fn select<'a>(
    mut artists: Vec<&'a Artist>,
    cursor: Option<&str>,
) -> (Vec<&'a Artist>, Vec<&'a Artist>) {
    artists.retain(|artist| artist.id.as_deref().is_some_and(valid_id));
    artists.sort_by(|left, right| left.id.cmp(&right.id));
    artists.dedup_by(|left, right| left.id == right.id);
    if let Some(cursor) = cursor {
        let start =
            artists.partition_point(|artist| artist.id.as_deref().is_some_and(|id| id <= cursor));
        artists.rotate_left(start);
    }
    let later = artists.split_off(artists.len().min(MAX_ARTIST_READS));
    (artists, later)
}

/// Rotate the publication receipt even when only a failed attempt advanced the
/// cursor. The cycle identity survives background passes but changes on a new
/// full refresh. Metadata bounds stop automatic reads without failing that union.
pub(crate) fn prepare_publication(
    expected: Option<&str>,
    cursor: Option<&str>,
    cohort: Option<&Cohort>,
) -> Result<SpotifyArtistDiscoveryFence, SpotifyError> {
    if cursor.is_some_and(|id| !valid_id(id)) {
        return Err(SpotifyError::Decode(
            "invalid artist discovery publication".into(),
        ));
    }
    let mut cohort = cohort.cloned();
    if let Some(cohort) = &mut cohort {
        if cohort.id.is_empty() {
            cohort.id = random_state()?;
        }
        if cohort.signals.len() > ARTIST_DISCOVERY_MAX_ARTISTS {
            cohort.limit();
        }
    }
    let mut prepared = Progress {
        version: if cohort.is_some() { 2 } else { 1 },
        last_artist_id: cursor.map(str::to_owned),
        publication: random_state()?,
        cohort,
    };
    let encode = |progress: &Progress| {
        serde_json::to_string(progress).map_err(|error| SpotifyError::Decode(error.to_string()))
    };
    let mut progress = encode(&prepared)?;
    if progress.len() > ARTIST_DISCOVERY_MAX_BYTES {
        if let Some(cohort) = &mut prepared.cohort {
            cohort.limit();
        }
        progress = encode(&prepared)?;
    }
    Progress::decode(&progress)?;
    Ok(SpotifyArtistDiscoveryFence {
        expected: expected.map(str::to_owned),
        progress,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artists(count: usize) -> Vec<Artist> {
        (0..count)
            .map(|number| Artist {
                id: Some(format!("Artist{number:04}")),
                name: "A shared artist name".into(),
                genres: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn accepted_receipts_continue_on_the_same_day_and_wrap() {
        let all = artists(120);
        let mut accepted = None;
        let mut seen = std::collections::HashSet::new();
        for expected in ["Artist0000", "Artist0050", "Artist0100"] {
            let cursor = cursor(accepted.as_deref()).unwrap();
            let (selected, later) = select(all.iter().rev().collect(), cursor.as_deref());
            assert_eq!((selected.len(), later.len()), (MAX_ARTIST_READS, 70));
            assert_eq!(selected[0].id.as_deref(), Some(expected));
            seen.extend(selected.iter().filter_map(|artist| artist.id.as_deref()));
            let fence = prepare_publication(
                accepted.as_deref(),
                selected.last().unwrap().id.as_deref(),
                None,
            )
            .unwrap();
            assert_eq!(fence.expected, accepted);
            accepted = Some(fence.progress);
        }
        assert_eq!(seen.len(), all.len());
        assert_eq!(
            cursor(accepted.as_deref()).unwrap().as_deref(),
            Some("Artist0029")
        );
    }

    #[test]
    fn removed_cursor_and_duplicate_or_invalid_ids_do_not_reset_rotation() {
        let all = artists(6);
        let invalid = Artist {
            id: Some("not/an/id".into()),
            name: "invalid".into(),
            genres: Vec::new(),
        };
        let (selected, later) = select(
            vec![&all[5], &all[0], &all[2], &all[2], &invalid],
            Some("Artist0003"),
        );
        assert!(later.is_empty());
        assert_eq!(
            selected
                .iter()
                .filter_map(|artist| artist.id.as_deref())
                .collect::<Vec<_>>(),
            ["Artist0005", "Artist0000", "Artist0002"]
        );
    }

    #[test]
    fn each_publication_changes_its_receipt_without_inventing_attempts() {
        let first = prepare_publication(None, Some("Artist0009"), None).unwrap();
        let second = prepare_publication(Some(&first.progress), Some("Artist0009"), None).unwrap();
        assert_eq!(
            cursor(Some(&second.progress)).unwrap().as_deref(),
            Some("Artist0009")
        );
        assert_ne!(first.progress, second.progress);
        assert_eq!(second.expected.as_deref(), Some(first.progress.as_str()));
    }

    #[test]
    fn malformed_or_unbounded_cursor_is_not_used_for_selection() {
        for raw in [
            "{}".to_owned(),
            "x".repeat(ARTIST_DISCOVERY_MAX_BYTES + 1),
            r#"{"version":1,"last_artist_id":"a/b","publication":"valid"}"#.to_owned(),
        ] {
            assert!(cursor(Some(&raw)).is_err());
        }
        assert_eq!(cursor(None).unwrap(), None);
    }
    fn owner() -> SpotifyLibraryOwner {
        SpotifyLibraryOwner {
            owner_id: "test-owner".into(),
            generation: 1,
        }
    }

    #[test]
    fn finite_cohorts_accumulate_success_and_rotate_the_remaining_failures() {
        let artists = artists(120);
        let weights = artists
            .iter()
            .map(|artist| (artist.id.clone().unwrap(), super::super::ARTIST_PM))
            .collect();
        let pending: Vec<_> = artists[50..]
            .iter()
            .map(|artist| artist.id.clone().unwrap())
            .collect();
        let cohort = Cohort::seed(&weights, &pending, 50, 1, 100);
        let fence = prepare_publication(None, Some("Artist0049"), Some(&cohort)).unwrap();
        let mut progress = Progress::decode(&fence.progress).unwrap();
        let cycle = progress.report(&owner(), 100).cycle;
        let selected = progress.selection();
        assert_eq!(selected.len(), 50);
        let successes: Vec<_> = selected[..49]
            .iter()
            .map(|artist| artist.id.clone().unwrap())
            .collect();
        progress.accepted(&successes, Some("Artist0099"));
        let state = progress.report(&owner(), 100);
        assert_eq!((state.completed, state.pending), (99, 21));
        assert_eq!(state.cycle, cycle);
        let selected = progress.selection();
        assert_eq!(selected[0].id.as_deref(), Some("Artist0100"));
        assert_eq!(selected.last().unwrap().id.as_deref(), Some("Artist0099"));
        let successes: Vec<_> = selected[..20]
            .iter()
            .map(|artist| artist.id.clone().unwrap())
            .collect();
        progress.accepted(&successes, Some("Artist0099"));
        assert_eq!(progress.report(&owner(), 100).pending, 1);
        progress.accepted(&["Artist0099".into()], Some("Artist0099"));
        let finished = progress.report(&owner(), 100);
        assert_eq!((finished.completed, finished.pending), (120, 0));
        assert!(progress.selection().is_empty());
        let next = progress.publication(&fence.progress).unwrap();
        assert_ne!(next.progress, fence.progress);
        assert_eq!(
            Progress::decode(&next.progress)
                .unwrap()
                .report(&owner(), 100)
                .cycle,
            cycle
        );
    }

    #[test]
    fn legacy_expired_changed_scope_and_malformed_cohorts_cannot_sample() {
        let legacy = prepare_publication(None, Some("Artist0001"), None).unwrap();
        assert!(
            Progress::decode(&legacy.progress)
                .unwrap()
                .report(&owner(), 100)
                .requires_refresh
        );
        let cohort = Cohort::seed(
            &HashMap::from([("Artist0001".into(), super::super::ARTIST_PM)]),
            &["Artist0001".into()],
            0,
            3,
            100,
        );
        let fence = prepare_publication(None, None, Some(&cohort)).unwrap();
        let progress = Progress::decode(&fence.progress).unwrap();
        assert!(!progress.permits_scopes(&["user-top-read"]));
        assert!(progress.permits_scopes(&[]));
        assert!(progress.report(&owner(), 99).requires_refresh);
        assert!(
            progress
                .report(&owner(), 100 + ARTIST_DISCOVERY_MAX_AGE_SECS)
                .requires_refresh
        );
        let body: serde_json::Value = serde_json::from_str(&fence.progress).unwrap();
        for replacement in [
            serde_json::json!([1]),
            serde_json::json!([0, 0]),
            serde_json::json!([-1]),
        ] {
            let mut invalid = body.clone();
            invalid["cohort"]["remaining"] = replacement;
            assert!(Progress::decode(&invalid.to_string()).is_err());
        }
        let mut invalid = body;
        invalid["cohort"]["signals"][0][1] = serde_json::json!(MAX_TASTE_PM + 1);
        assert!(Progress::decode(&invalid.to_string()).is_err());
    }

    #[test]
    fn an_oversized_cohort_reports_its_limit_without_blocking_full_publication() {
        let count = ARTIST_DISCOVERY_MAX_ARTISTS + 1;
        let cohort = Cohort {
            id: String::new(),
            created_at: 100,
            scopes: 1,
            signals: (0..count)
                .map(|index| (format!("Artist{index:06}"), 1050))
                .collect(),
            remaining: (0..count).collect(),
            completed: 0,
            limited: false,
            omitted: 0,
        };
        let fence = prepare_publication(None, None, Some(&cohort)).unwrap();
        let progress = Progress::decode(&fence.progress).unwrap();
        let state = progress.report(&owner(), 100);
        assert!(state.limited);
        assert!(!state.requires_refresh);
        assert_eq!(state.pending, count);
        assert!(progress.selection().is_empty());
        assert!(fence.progress.len() < 1024);
    }
}
