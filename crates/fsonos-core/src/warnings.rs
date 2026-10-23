//! The daemon's own warnings, counted by kind (co55.7), so that a storm (one
//! warning dozens of times an hour: renewals failing while the Wi-Fi drops,
//! the event listener re-opening as the route flaps) shows in `GET /health`
//! and `fsonos doctor` instead of waiting for someone to tally the log.
//!
//! A kind is where the event is logged (its target and source line), never
//! its field values, which carry addresses and ids. One sample message is
//! kept per kind, with every word that holds a digit masked.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};

/// Counting granularity, in seconds.
const BUCKET_SECS: u64 = 300;
/// Buckets in an hour, the window a storm is judged over.
const HOUR: u64 = 3600 / BUCKET_SECS;
/// Hours kept: a week.
const HOURS_KEPT: u64 = 7 * 24;
/// The fewest warnings of one kind in an hour that make a storm.
pub const STORM_FLOOR: u32 = 20;
/// A storm is also this many times the kind's median hour so far.
pub const STORM_FACTOR: u32 = 10;
/// The longest sample message kept.
const SAMPLE_CHARS: usize = 120;

/// A kind of warning that is storming now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Storm {
    /// Where it is logged: `target@file:line`.
    pub kind: String,
    /// Whether it is logged at ERROR (else WARN).
    pub error: bool,
    /// Its message, every word holding a digit masked as `N`.
    pub sample: String,
    /// How many in the last hour.
    pub last_hour: u32,
    /// How many make a storm for this kind: the larger of [`STORM_FLOOR`]
    /// and [`STORM_FACTOR`] times its median hour.
    pub threshold: u32,
}

/// Warnings counted by kind, in buckets of five minutes, for a week. It is
/// kept across a restart as JSON ([`to_json`], [`restore_json`]).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Counts {
    /// The first bucket counted: hours before it were not observed.
    first: Option<u64>,
    buckets: VecDeque<(u64, HashMap<String, u32>)>,
    samples: HashMap<String, (bool, String)>,
}

impl Counts {
    /// One warning of `kind` at `now` (unix seconds). A clock set back
    /// counts it in the newest bucket.
    pub fn record(&mut self, kind: &str, error: bool, message: &str, now: u64) {
        let newest = self.buckets.back().map_or(0, |(b, _)| *b);
        let at = (now / BUCKET_SECS).max(newest);
        self.first.get_or_insert(at);
        if self.buckets.back().is_none_or(|(b, _)| *b != at) {
            self.buckets.push_back((at, HashMap::new()));
        }
        if let Some((_, counts)) = self.buckets.back_mut() {
            let n = counts.entry(kind.to_owned()).or_default();
            *n = n.saturating_add(1);
        }
        while self
            .buckets
            .front()
            .is_some_and(|(b, _)| *b + HOURS_KEPT * HOUR <= at)
        {
            self.buckets.pop_front();
        }
        self.samples
            .entry(kind.to_owned())
            .or_insert_with(|| (error, mask(message)));
    }

    /// The kinds storming at `now` (unix seconds), by kind.
    #[must_use]
    pub fn storms(&self, now: u64) -> Vec<Storm> {
        let Some(first) = self.first else {
            return Vec::new();
        };
        let at = now / BUCKET_SECS;
        let span = usize::try_from(HOURS_KEPT).unwrap_or(168);
        // Per kind, its count in each hour back from now: [0] is the last
        // hour, [h] the hour that ended h hours ago.
        let mut hourly: HashMap<&str, Vec<u32>> = HashMap::new();
        for (bucket, counts) in &self.buckets {
            let Ok(h) = usize::try_from(at.saturating_sub(*bucket) / HOUR) else {
                continue;
            };
            if h >= span {
                continue;
            }
            for (kind, n) in counts {
                let hours = hourly.entry(kind).or_insert_with(|| vec![0; span]);
                hours[h] = hours[h].saturating_add(*n);
            }
        }
        // Whole hours observed before the last one: they set the baseline.
        let observed = usize::try_from((at + 1).saturating_sub(first) / HOUR)
            .unwrap_or(0)
            .saturating_sub(1)
            .min(span - 1);
        let mut storms: Vec<Storm> = hourly
            .into_iter()
            .filter_map(|(kind, hours)| {
                let mut before: Vec<u32> = hours[1..=observed].to_vec();
                before.sort_unstable();
                let median = before.get(before.len() / 2).copied().unwrap_or(0);
                let threshold = STORM_FLOOR.max(median.saturating_mul(STORM_FACTOR));
                let last_hour = hours[0];
                let (error, sample) = self.samples.get(kind).cloned().unwrap_or_default();
                (last_hour >= threshold).then(|| Storm {
                    kind: kind.to_owned(),
                    error,
                    sample,
                    last_hour,
                    threshold,
                })
            })
            .collect();
        storms.sort_by(|a, b| a.kind.cmp(&b.kind));
        storms
    }

    /// Add the counts `kept` (a previous run's) to these, bucket by bucket,
    /// so the kinds' usual hours survive a restart.
    pub fn merge(&mut self, kept: Counts) {
        self.first = match (self.first, kept.first) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mut buckets: BTreeMap<u64, HashMap<String, u32>> = BTreeMap::new();
        for (bucket, counts) in kept.buckets.into_iter().chain(self.buckets.drain(..)) {
            let into = buckets.entry(bucket).or_default();
            for (kind, n) in counts {
                let total = into.entry(kind).or_default();
                *total = total.saturating_add(n);
            }
        }
        self.buckets = buckets.into_iter().collect();
        for (kind, sample) in kept.samples {
            self.samples.entry(kind).or_insert(sample);
        }
    }
}

/// This process's warnings.
static COUNTS: LazyLock<Mutex<Counts>> = LazyLock::new(Mutex::default);

/// Count one warning in this process ([`Counts::record`]). It logs nothing,
/// so a tracing layer may call it.
pub fn record(kind: &str, error: bool, message: &str, now: u64) {
    COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(kind, error, message, now);
}

/// The kinds storming in this process at `now` ([`Counts::storms`]).
#[must_use]
pub fn storms(now: u64) -> Vec<Storm> {
    COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .storms(now)
}

/// This process's counts as JSON, to keep across a restart.
#[must_use]
pub fn to_json() -> String {
    serde_json::to_string(&*COUNTS.lock().unwrap_or_else(PoisonError::into_inner))
        .unwrap_or_default()
}

/// Add the counts a previous run kept ([`to_json`]) to this process's.
///
/// # Errors
/// When `text` is not such JSON; nothing is changed then.
pub fn restore_json(text: &str) -> Result<(), serde_json::Error> {
    let kept: Counts = serde_json::from_str(text)?;
    COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .merge(kept);
    Ok(())
}

/// `message` with every word that holds a digit (an address, a port, an id)
/// as `N`, cut to [`SAMPLE_CHARS`].
fn mask(message: &str) -> String {
    fn flush(word: &mut String, out: &mut String) {
        if word.chars().any(|c| c.is_ascii_digit()) {
            out.push('N');
        } else {
            out.push_str(word);
        }
        word.clear();
    }
    let mut out = String::new();
    let mut word = String::new();
    for c in message.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out.chars().take(SAMPLE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RENEWAL: &str = "fsonos_core::events@crates/fsonos-core/src/events.rs:120";
    const LISTENER: &str = "fsonos_core::live@crates/fsonos-core/src/live.rs:640";
    /// A Thursday, on a bucket boundary.
    const T0: u64 = 1_792_713_600;

    fn spread(c: &mut Counts, kind: &str, per_hour: u64, hours: std::ops::Range<u64>) {
        for h in hours {
            for i in 0..per_hour {
                c.record(
                    kind,
                    false,
                    "renewal failed",
                    T0 + h * 3600 + i * 3600 / per_hour,
                );
            }
        }
    }

    #[test]
    fn samples_mask_every_word_holding_a_digit() {
        assert_eq!(
            mask("this host now reaches the players from 192.0.2.10, not 198.51.100.7"),
            "this host now reaches the players from N.N.N.N, not N.N.N.N"
        );
        assert_eq!(mask("RINCON_000E58A0000401400 at fd7a::5"), "N at N::N");
        assert_eq!(mask("renewal failed"), "renewal failed");
        assert_eq!(mask(&"x".repeat(500)).len(), SAMPLE_CHARS);
    }

    #[test]
    fn twenty_in_an_hour_storm_from_the_first_hour() {
        let mut c = Counts::default();
        spread(&mut c, RENEWAL, 19, 0..1);
        let now = T0 + 3599;
        assert!(c.storms(now).is_empty(), "19 is under the floor");
        c.record(RENEWAL, false, "renewal failed", now);
        let storms = c.storms(now);
        assert_eq!(storms.len(), 1);
        assert_eq!(storms[0].kind, RENEWAL);
        assert_eq!(
            (storms[0].last_hour, storms[0].threshold),
            (20, STORM_FLOOR)
        );
        assert_eq!(storms[0].sample, "renewal failed");
        // An hour later, with none since, it has passed.
        assert!(c.storms(now + 3600).is_empty());
    }

    #[test]
    fn a_kind_that_is_always_busy_storms_only_at_ten_times_its_usual_hour() {
        let mut c = Counts::default();
        // Five a day-hour for a day, then 30 in an hour: under 10 x 5.
        spread(&mut c, LISTENER, 5, 0..24);
        spread(&mut c, LISTENER, 30, 24..25);
        let now = T0 + 25 * 3600 - 1;
        assert!(c.storms(now).is_empty(), "{:?}", c.storms(now));
        // 50 is ten times the median: a storm.
        spread(&mut c, LISTENER, 50, 25..26);
        let storms = c.storms(T0 + 26 * 3600 - 1);
        assert_eq!(storms.len(), 1, "{storms:?}");
        assert_eq!((storms[0].last_hour, storms[0].threshold), (50, 50));
    }

    /// A kind that is always busy is not a storm just because the daemon
    /// restarted: the usual hours it kept come back with it.
    #[test]
    fn a_busy_kinds_usual_hour_survives_a_restart() {
        let mut before = Counts::default();
        spread(&mut before, LISTENER, 5, 0..24);
        let kept = serde_json::to_string(&before).unwrap();
        // The restarted daemon: 30 in its first hour.
        let mut after = Counts::default();
        spread(&mut after, LISTENER, 30, 24..25);
        let now = T0 + 25 * 3600 - 1;
        assert_eq!(
            after.storms(now).len(),
            1,
            "with no history, 30 is over the floor"
        );
        after.merge(serde_json::from_str(&kept).unwrap());
        assert!(after.storms(now).is_empty(), "{:?}", after.storms(now));
        assert_eq!(after.first, Some(T0 / BUCKET_SECS));
        assert!(
            after
                .buckets
                .iter()
                .zip(after.buckets.iter().skip(1))
                .all(|(a, b)| a.0 < b.0)
        );
        assert_eq!(after.samples[LISTENER].1, "renewal failed");
    }

    #[test]
    fn a_quiet_kind_beside_a_storm_is_not_reported_and_old_buckets_go() {
        let mut c = Counts::default();
        c.record(LISTENER, true, "No space left on device (os error 28)", T0);
        spread(&mut c, RENEWAL, 40, 0..1);
        let storms = c.storms(T0 + 3599);
        assert_eq!(
            storms.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(),
            [RENEWAL]
        );
        let week = HOURS_KEPT * 3600;
        c.record(LISTENER, true, "again", T0 + week + 10);
        assert!(c.buckets.iter().all(|(b, _)| *b * BUCKET_SECS >= T0 + 300));
        assert_eq!(
            c.samples[LISTENER],
            (true, "No space left on device (os error N)".into())
        );
    }
}
