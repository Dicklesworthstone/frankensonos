//! Page counters describe raw entries, before unavailable tracks or episodes
//! are filtered out. Missing optional counters are not invented as zero.

use super::parameter;
use crate::SpotifyError;
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
    pub offset: Option<u32>,
    pub total: Option<u32>,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Coverage {
    offset: u32,
    total: Option<u32>,
}

fn invalid(detail: &str) -> SpotifyError {
    SpotifyError::Decode(detail.into())
}

impl Coverage {
    /// Validate before publishing this page. A bad page does not consume
    /// coverage; a successful read must account for every advertised entry.
    pub(super) fn accept<T>(&mut self, page: &Page<T>) -> Result<(), SpotifyError> {
        if page.offset.is_some_and(|offset| offset != self.offset) {
            return Err(invalid("taste response offset differs from the requested page"));
        }
        if let (Some(before), Some(after)) = (self.total, page.total)
            && before != after
        {
            return Err(invalid("taste collection changed total while being read"));
        }
        let count = u32::try_from(page.items.len())
            .map_err(|_| invalid("taste page contains too many entries"))?;
        let Some(end) = self.offset.checked_add(count) else {
            return Err(invalid("taste page offset overflow"));
        };
        let total = page.total.or(self.total);
        if total.is_some_and(|total| end > total) {
            return Err(invalid("taste page exceeds its advertised total"));
        }
        match &page.next {
            Some(next) => {
                let next_offset = parameter(next, "offset")?
                    .parse::<u32>()
                    .map_err(|_| invalid("invalid taste paging offset"))?;
                if count == 0 || next_offset != end {
                    return Err(invalid("taste continuation skips or overlaps returned entries"));
                }
                if total == Some(end) {
                    return Err(invalid("taste continuation exceeds the advertised collection"));
                }
            }
            None if total.is_some_and(|total| end < total) => {
                return Err(invalid("taste collection ended before its advertised total"));
            }
            None => {}
        }
        self.offset = end;
        self.total = total;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn page(
        items: Vec<Value>,
        offset: Option<u32>,
        total: Option<u32>,
        next: Option<u32>,
    ) -> Page<Value> {
        Page {
            items,
            offset,
            total,
            next: next.map(|offset| {
                format!("https://api.spotify.com/v1/me/tracks?offset={offset}")
            }),
        }
    }

    #[test]
    fn raw_entries_not_playable_tracks_determine_page_coverage() {
        let mut coverage = Coverage::default();
        let entries = vec![
            Value::Null,
            json!({ "type": "episode" }),
            json!({ "is_playable": false }),
        ];
        coverage
            .accept(&page(entries, Some(0), Some(4), Some(3)))
            .unwrap();
        coverage
            .accept(&page(
                vec![json!({ "uri": "spotify:track:Test" })],
                Some(3),
                Some(4),
                None,
            ))
            .unwrap();
        assert_eq!(coverage.offset, 4);
    }

    #[test]
    fn absent_counters_still_follow_contiguous_pages_and_an_explicit_empty_page_is_complete() {
        let mut coverage = Coverage::default();
        coverage
            .accept(&page(vec![Value::Null], None, None, Some(1)))
            .unwrap();
        coverage
            .accept(&page(vec![Value::Null], None, None, None))
            .unwrap();
        assert_eq!(coverage.offset, 2);
        Coverage::default()
            .accept(&page(Vec::new(), Some(0), Some(0), None))
            .unwrap();
    }

    #[test]
    fn terminal_truncation_gaps_overlaps_and_counter_mismatches_are_not_completion() {
        for bad in [
            page(vec![Value::Null], Some(0), Some(2), None),
            page(vec![Value::Null], Some(0), Some(3), Some(2)),
            page(vec![Value::Null], Some(0), Some(3), Some(0)),
            page(Vec::new(), Some(0), Some(3), Some(1)),
            page(vec![Value::Null], Some(1), Some(2), None),
            page(vec![Value::Null], Some(0), Some(0), None),
            page(vec![Value::Null], Some(0), Some(1), Some(1)),
        ] {
            let mut coverage = Coverage::default();
            assert!(coverage.accept(&bad).is_err());
            assert_eq!(coverage, Coverage::default());
        }
    }

    #[test]
    fn changing_totals_cannot_replace_earlier_coverage() {
        let mut coverage = Coverage::default();
        coverage
            .accept(&page(vec![Value::Null], Some(0), Some(2), Some(1)))
            .unwrap();
        assert!(
            coverage
                .accept(&page(vec![Value::Null], Some(1), Some(3), Some(2)))
                .is_err()
        );
        assert_eq!(coverage.offset, 1);
        coverage
            .accept(&page(vec![Value::Null], Some(1), Some(2), None))
            .unwrap();
    }

    #[test]
    fn offset_overflow_is_rejected_without_advancing() {
        let mut coverage = Coverage {
            offset: u32::MAX,
            total: None,
        };
        assert!(
            coverage
                .accept(&page(vec![Value::Null], None, None, None))
                .is_err()
        );
        assert_eq!(coverage.offset, u32::MAX);
    }
}
