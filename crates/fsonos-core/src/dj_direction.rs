//! Persisted DJ direction shared by scenes and scheduled starts. The core
//! keeps the request's vocabulary; the DJ resolves moods, periods and filters.
//! Relative duration starts afresh whenever the saved direction is applied.

use serde::{Deserialize, Serialize};

/// Selection constraints without an absolute session expiry. These fields
/// match the public steering vocabulary, without depending on an API or DJ
/// implementation. Missing fields preserve older saved definitions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DjConstraints {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_composers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_composers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_artists: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_artists: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_genres: Vec<String>,
    /// Canonical period names, for example `baroque` or `late_romantic`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub periods: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_periods: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decades: Vec<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_decades: Vec<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_keywords: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_keywords: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_work_minutes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_work_minutes: Option<u32>,
    pub energy_bias: i8,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_long_works: bool,
}

impl DjConstraints {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A reusable steering request. The mood stays on the containing scene or
/// schedule for compatibility. An explicit empty direction without a mood
/// means return to automatic steering; an absent direction keeps legacy
/// behavior, including preserving current steering for a plain DJ start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DjDirection {
    pub constraints: DjConstraints,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub for_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_direction_round_trips_every_constraint_and_relative_duration() {
        let json = serde_json::json!({
            "constraints": {
                "include_composers": ["Bach"], "exclude_composers": ["Wagner"],
                "include_artists": ["Miles Davis"], "exclude_artists": ["Other artist"],
                "include_genres": ["jazz"], "exclude_genres": ["metal"],
                "periods": ["baroque"], "exclude_periods": ["late_romantic"],
                "decades": [1960], "exclude_decades": [1980],
                "include_keywords": ["piano"], "exclude_keywords": ["opera"],
                "min_work_minutes": 2, "max_work_minutes": 45,
                "energy_bias": -1, "allow_long_works": true
            },
            "for_secs": 7200
        });
        let direction: DjDirection = serde_json::from_value(json.clone()).unwrap();
        assert!(!direction.constraints.is_empty());
        assert_eq!(serde_json::to_value(&direction).unwrap(), json);
    }

    #[test]
    fn an_explicit_empty_direction_is_valid_but_unknown_or_absolute_fields_are_not() {
        let empty: DjDirection = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, DjDirection::default());
        assert!(empty.constraints.is_empty());
        for json in [
            r#"{"expires_at":123}"#,
            r#"{"constraints":{"expires_at":123}}"#,
            r#"{"constraints":{"include_genre":["jazz"]}}"#,
            r#"{"for_secs":-1}"#,
        ] {
            assert!(serde_json::from_str::<DjDirection>(json).is_err(), "{json}");
        }
    }
}
