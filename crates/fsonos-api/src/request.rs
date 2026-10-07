//! Request bodies shared by the HTTP routes and the MCP tools, and their pure
//! validation.
//!
//! Both surfaces deserialize these same JSON shapes and run the same checks,
//! so an agent gets the same answer from `POST /volume` as from the
//! `set_volume` tool. Unknown fields are rejected rather than ignored: an agent
//! that sends `"room"` instead of `"zone"` should hear about it, not have its
//! request silently mean something else.

use serde::{Deserialize, Serialize};

use crate::failure::Failure;
use crate::source::normalize_source_uri;

/// Longest room/zone name accepted, in characters.
pub const MAX_ZONE_LEN: usize = 128;

/// Longest now-playing title accepted, in characters. A title is cosmetic
/// metadata, so an over-long one is truncated rather than rejected.
pub const MAX_TITLE_LEN: usize = 256;

/// Body naming one zone: `POST /pause|resume|next|previous|ungroup` and
/// `POST /dj/{start|skip|stop}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneRequest {
    /// Room name, matched case-insensitively with curly apostrophes folded.
    pub zone: String,
}

impl ZoneRequest {
    /// The trimmed zone name, or why it is unusable.
    pub fn zone(&self) -> Result<&str, Failure> {
        zone_name("zone", &self.zone)
    }
}

/// `POST /play` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayRequest {
    pub zone: String,
    /// `spotify:<kind>:<id>`, an `open.spotify.com` link, or a renderer URI.
    pub source_uri: String,
    /// Display title for the speaker's now-playing metadata.
    #[serde(default)]
    pub title: Option<String>,
}

impl PlayRequest {
    /// Trim the zone and title and canonicalize the source URI (see
    /// [`normalize_source_uri`]).
    pub fn normalized(&self) -> Result<Self, Failure> {
        Ok(Self {
            zone: zone_name("zone", &self.zone)?.to_string(),
            source_uri: normalize_source_uri(&self.source_uri)?,
            title: self
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| t.chars().take(MAX_TITLE_LEN).collect()),
        })
    }
}

/// `POST /volume` body: exactly one of `volume` (absolute) or `delta`
/// (relative). With `group`, the change applies to the zone's whole group
/// rather than the one room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRequest {
    pub zone: String,
    /// Absolute volume, 0–100.
    #[serde(default)]
    pub volume: Option<i64>,
    /// Relative change, −100–100, non-zero.
    #[serde(default)]
    pub delta: Option<i64>,
    #[serde(default)]
    pub group: bool,
}

/// A validated volume change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeChange {
    /// Set the volume to this level (0–100).
    Set(u8),
    /// Raise (positive) or lower (negative) the volume by this much.
    Adjust(i8),
}

impl VolumeRequest {
    /// The trimmed zone name, or why it is unusable.
    pub fn zone(&self) -> Result<&str, Failure> {
        zone_name("zone", &self.zone)
    }

    /// The requested change, or why it is unusable.
    pub fn change(&self) -> Result<VolumeChange, Failure> {
        match (self.volume, self.delta) {
            (Some(_), Some(_)) => Err(Failure::invalid(
                "give either `volume` (absolute) or `delta` (relative), not both",
            )),
            (None, None) => Err(Failure::invalid(
                "give `volume` (0 to 100) or `delta` (-100 to 100)",
            )),
            (Some(v), None) => u8::try_from(v)
                .ok()
                .filter(|v| *v <= 100)
                .map(VolumeChange::Set)
                .ok_or_else(|| Failure::invalid(format!("volume must be 0 to 100, got {v}"))),
            (None, Some(0)) => Err(Failure::invalid("delta must be non-zero")),
            (None, Some(d)) => i8::try_from(d)
                .ok()
                .filter(|d| (-100..=100).contains(d))
                .map(VolumeChange::Adjust)
                .ok_or_else(|| Failure::invalid(format!("delta must be -100 to 100, got {d}"))),
        }
    }
}

/// `POST /group` body: move `zone` into the group that `to` belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRequest {
    /// The room that moves.
    pub zone: String,
    /// Any room in the group it joins.
    pub to: String,
}

impl GroupRequest {
    /// The trimmed `(zone, to)` names, or why either is unusable.
    pub fn zones(&self) -> Result<(&str, &str), Failure> {
        Ok((zone_name("zone", &self.zone)?, zone_name("to", &self.to)?))
    }
}

fn zone_name<'a>(field: &str, raw: &'a str) -> Result<&'a str, Failure> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(Failure::invalid(format!("`{field}` is empty; name a room")));
    }
    if name.chars().count() > MAX_ZONE_LEN {
        return Err(Failure::invalid(format!(
            "`{field}` is longer than {MAX_ZONE_LEN} characters"
        )));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn volume(volume: Option<i64>, delta: Option<i64>) -> Result<VolumeChange, Failure> {
        VolumeRequest {
            zone: "Kitchen".into(),
            volume,
            delta,
            group: false,
        }
        .change()
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_value::<ZoneRequest>(json!({ "room": "Kitchen" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `room`"), "{err}");
        assert!(
            serde_json::from_value::<PlayRequest>(json!({
                "zone": "Kitchen", "source_uri": "spotify:track:x", "volume": 3
            }))
            .is_err()
        );
    }

    #[test]
    fn optional_fields_default() {
        let v: VolumeRequest =
            serde_json::from_value(json!({ "zone": "Den", "volume": 20 })).unwrap();
        assert_eq!((v.delta, v.group), (None, false));
        let p: PlayRequest =
            serde_json::from_value(json!({ "zone": "Den", "source_uri": "x-a:b" })).unwrap();
        assert_eq!(p.title, None);
    }

    #[test]
    fn zone_names_are_trimmed_and_required() {
        let z = ZoneRequest {
            zone: "  Ada\u{2019}s Studio ".into(),
        };
        assert_eq!(z.zone().unwrap(), "Ada\u{2019}s Studio");
        let empty = ZoneRequest { zone: " \t".into() }.zone().unwrap_err();
        assert_eq!(empty.status(), 422);
        assert!(empty.detail.contains("`zone` is empty"));
        let long = ZoneRequest {
            zone: "x".repeat(MAX_ZONE_LEN + 1),
        }
        .zone()
        .unwrap_err();
        assert!(long.detail.contains("longer than"));
    }

    #[test]
    fn group_names_both_rooms() {
        let g = GroupRequest {
            zone: " Kitchen".into(),
            to: "Den ".into(),
        };
        assert_eq!(g.zones().unwrap(), ("Kitchen", "Den"));
        let g = GroupRequest {
            zone: "Kitchen".into(),
            to: String::new(),
        };
        assert!(g.zones().unwrap_err().detail.contains("`to` is empty"));
    }

    #[test]
    fn play_is_normalized() {
        let p = PlayRequest {
            zone: " Den ".into(),
            source_uri: "https://open.spotify.com/track/0123456789ABCDEFabcdef?si=z".into(),
            title: Some("  ".into()),
        }
        .normalized()
        .unwrap();
        assert_eq!(p.zone, "Den");
        assert_eq!(p.source_uri, "spotify:track:0123456789ABCDEFabcdef");
        assert_eq!(p.title, None);
        let bad = PlayRequest {
            zone: "Den".into(),
            source_uri: "Goldberg Variations".into(),
            title: None,
        };
        assert_eq!(bad.normalized().unwrap_err().status(), 422);
    }

    #[test]
    fn volume_takes_exactly_one_form() {
        assert_eq!(volume(Some(0), None).unwrap(), VolumeChange::Set(0));
        assert_eq!(volume(Some(100), None).unwrap(), VolumeChange::Set(100));
        assert_eq!(volume(None, Some(-5)).unwrap(), VolumeChange::Adjust(-5));
        assert_eq!(volume(None, Some(100)).unwrap(), VolumeChange::Adjust(100));
        assert!(
            volume(Some(10), Some(2))
                .unwrap_err()
                .detail
                .contains("not both")
        );
        assert!(
            volume(None, None)
                .unwrap_err()
                .detail
                .contains("give `volume`")
        );
    }

    #[test]
    fn volume_out_of_range_says_so() {
        for (v, d) in [(Some(101), None), (Some(-1), None), (Some(i64::MAX), None)] {
            assert!(
                volume(v, d)
                    .unwrap_err()
                    .detail
                    .contains("volume must be 0 to 100")
            );
        }
        for d in [101, -101, i64::MIN] {
            assert!(
                volume(None, Some(d))
                    .unwrap_err()
                    .detail
                    .contains("delta must be -100 to 100")
            );
        }
        assert!(
            volume(None, Some(0))
                .unwrap_err()
                .detail
                .contains("non-zero")
        );
    }
}
