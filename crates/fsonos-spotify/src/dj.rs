//! The classical-music DJ: pure selection logic.
//!
//! Given a candidate pool (the user's saved classical tracks) and recent play
//! history, pick the next track for pleasant variety. The real engine will
//! weight by composer/period/work and energy; this is the testable skeleton
//! with a working anti-repeat selector so the daemon has end-to-end behavior
//! from day one.

use fsonos_types::Track;

/// A seedable, dependency-free pseudo-random generator (xorshift64*). Keeping
/// randomness in-crate avoids pulling `rand` and makes DJ picks reproducible in
/// tests by fixing the seed.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform index in `0..n` (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % (n as u64)) as usize
    }
}

/// Pick the next track: prefer a candidate not among the last `avoid_window`
/// plays; fall back to a random candidate if all are recent. `recent` is the
/// most-recent-last list of previously played `source_uri`s.
#[must_use]
pub fn pick_next<'a>(
    pool: &'a [Track],
    recent: &[String],
    avoid_window: usize,
    rng: &mut Rng,
) -> Option<&'a Track> {
    if pool.is_empty() {
        return None;
    }
    let recent_set: std::collections::HashSet<&str> = recent
        .iter()
        .rev()
        .take(avoid_window)
        .map(String::as_str)
        .collect();
    let fresh: Vec<&Track> = pool
        .iter()
        .filter(|t| !recent_set.contains(t.source_uri.as_str()))
        .collect();
    let choices = if fresh.is_empty() {
        pool.iter().collect::<Vec<_>>()
    } else {
        fresh
    };
    let idx = rng.below(choices.len());
    Some(choices[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(uri: &str) -> Track {
        Track {
            title: uri.into(),
            artist: None,
            album: None,
            source_uri: uri.into(),
            uri: None,
            duration_secs: None,
        }
    }

    #[test]
    fn avoids_recent() {
        let pool = vec![t("a"), t("b"), t("c")];
        let recent = vec!["a".to_string(), "b".to_string()];
        let mut rng = Rng::new(42);
        let picked = pick_next(&pool, &recent, 2, &mut rng).unwrap();
        assert_eq!(picked.source_uri, "c");
    }

    #[test]
    fn falls_back_when_all_recent() {
        let pool = vec![t("a")];
        let recent = vec!["a".to_string()];
        let mut rng = Rng::new(1);
        assert!(pick_next(&pool, &recent, 5, &mut rng).is_some());
    }
}
