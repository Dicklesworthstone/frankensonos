//! Request bodies shared by the HTTP routes and the MCP tools, and their pure
//! validation.
//!
//! Both surfaces deserialize these same JSON shapes and run the same checks,
//! so an agent gets the same answer from `POST /volume` as from the
//! `set_volume` tool. Unknown fields are rejected rather than ignored: an agent
//! that sends `"room"` instead of `"zone"` should hear about it, not have its
//! request silently mean something else.

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};

use crate::dj::{DjSteer, SteerConstraints};
use crate::failure::Failure;
use crate::source::normalize_source_uri;

/// Longest room/zone name accepted, in characters.
pub const MAX_ZONE_LEN: usize = 128;

/// Longest now-playing title accepted, in characters. A title is cosmetic
/// metadata, so an over-long one is truncated rather than rejected.
pub const MAX_TITLE_LEN: usize = 256;

/// Body naming one zone: `POST /pause|resume|next|previous|ungroup` and
/// `POST /dj/{start|skip|stop}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// `POST /mute` body. `mute` defaults to `true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MuteRequest {
    pub zone: String,
    #[serde(default = "yes")]
    pub mute: bool,
}

fn yes() -> bool {
    true
}

impl MuteRequest {
    /// The trimmed zone name, or why it is unusable.
    pub fn zone(&self) -> Result<&str, Failure> {
        zone_name("zone", &self.zone)
    }
}

/// `POST /play/favorite` body: play one of the household's Sonos favorites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayFavoriteRequest {
    pub zone: String,
    /// A title (case, accents and punctuation ignored; a unique prefix or all
    /// its words will do), a 1-based position, or an `FV:2/<n>` id.
    pub favorite: String,
}

impl PlayFavoriteRequest {
    /// The trimmed zone name, or why it is unusable.
    pub fn zone(&self) -> Result<&str, Failure> {
        zone_name("zone", &self.zone)
    }

    /// The trimmed favorite query, or why it is unusable.
    pub fn favorite(&self) -> Result<&str, Failure> {
        let favorite = self.favorite.trim();
        if favorite.is_empty() {
            return Err(Failure::invalid("`favorite` is empty; name a favorite"));
        }
        Ok(favorite)
    }
}

/// Most results a library search returns.
pub const MAX_SEARCH_RESULTS: usize = 50;

/// `search_library` / `GET /library/search?q=&zone=&limit=`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    /// Words of a title, composer, performer or album, or a catalog number
    /// (`bwv 988`, `op 67`).
    pub query: String,
    /// A room: its household's Sonos favorites are searched too.
    #[serde(default)]
    pub zone: Option<String>,
    /// At most this many results, best first (1 to 50, default 10).
    #[serde(default)]
    pub limit: Option<usize>,
}

impl SearchRequest {
    /// The trimmed query, or why it is unusable.
    pub fn query(&self) -> Result<&str, Failure> {
        let query = self.query.trim();
        if query.is_empty() {
            return Err(Failure::invalid("`query` is empty; say what to look for"));
        }
        Ok(query)
    }

    /// The result limit, or why it is out of range.
    pub fn limit(&self) -> Result<usize, Failure> {
        match self.limit {
            None => Ok(10),
            Some(n) if (1..=MAX_SEARCH_RESULTS).contains(&n) => Ok(n),
            Some(n) => Err(Failure::invalid(format!(
                "limit must be 1 to {MAX_SEARCH_RESULTS}, got {n}"
            ))),
        }
    }

    /// The trimmed zone, if one was given, or why it is unusable.
    pub fn zone(&self) -> Result<Option<&str>, Failure> {
        self.zone
            .as_deref()
            .map(|z| zone_name("zone", z))
            .transpose()
    }
}

/// `POST /move` body: move the music `zone` plays to `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveRequest {
    /// The room whose music moves.
    pub zone: String,
    /// The room it moves to.
    pub to: String,
    /// Replay the music there instead (the same track and position) and
    /// stop the source: the only way across households.
    #[serde(default)]
    pub copy: bool,
}

impl MoveRequest {
    /// The trimmed `(zone, to)` names, or why either is unusable.
    pub fn zones(&self) -> Result<(&str, &str), Failure> {
        Ok((zone_name("zone", &self.zone)?, zone_name("to", &self.to)?))
    }
}

/// `POST /party` body: group every room of a household. Name the room that
/// leads (the others join its group), or the household (`S1`, `S2`, or its
/// id: the group playing now leads, else its first room); with one
/// household, neither.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PartyRequest {
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub household: Option<String>,
}

/// `POST /group` body: move `zone` into the group that `to` belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// Most entries one steering list takes (composers, periods, keywords).
pub const MAX_STEER_LIST: usize = 32;
/// Shortest and longest a timed steer lasts: a minute, a week.
pub const MIN_STEER_SECS: u64 = 60;
pub const MAX_STEER_SECS: u64 = 7 * 24 * 60 * 60;
/// Longest work length a steer can name, in minutes.
pub const MAX_WORK_MINUTES: u32 = 600;

/// `POST /dj/steer` body: replace the DJ steering of the zone a room plays
/// in (a mood, constraints, how long), or clear it. It applies from the
/// DJ's next pick, running or not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DjSteerRequest {
    pub zone: String,
    /// A named mood (`GET /dj/moods` lists them).
    #[serde(default)]
    pub mood: Option<String>,
    /// Laid over the mood's own constraints.
    #[serde(default)]
    pub constraints: SteerConstraints,
    /// How long the steering lasts, in seconds (60 to a week); then the
    /// time-of-day program takes over. Absent: until cleared.
    #[serde(default)]
    pub for_secs: Option<u64>,
    /// Clear the zone's steering instead, back to the time-of-day program
    /// (with no other field).
    #[serde(default)]
    pub clear: bool,
}

impl DjSteerRequest {
    pub fn zone(&self) -> Result<&str, Failure> {
        zone_name("zone", &self.zone)
    }

    /// The steer asked for, checked.
    pub fn steer(&self) -> Result<DjSteer, Failure> {
        if self.clear {
            if self.mood.is_some() || !self.constraints.is_empty() || self.for_secs.is_some() {
                return Err(Failure::invalid(
                    "`clear` takes no `mood`, `constraints` or `for_secs`",
                ));
            }
            return Ok(DjSteer::Clear);
        }
        if self.mood.is_none() && self.constraints.is_empty() {
            return Err(
                Failure::invalid("name a `mood` or a constraint to steer by").with_hint(
                    "To go back to the time-of-day program, clear the steering: clear true \
                     (fsonos dj steer <room> --clear).",
                ),
            );
        }
        steer_set(self.mood.as_deref(), &self.constraints, self.for_secs)
    }
}

/// `POST /dj/start` body: start the DJ in the zone a room plays in, steered
/// by `mood` (for `for_secs`) when one is named.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DjStartRequest {
    pub zone: String,
    /// A named mood to start in (`GET /dj/moods` lists them).
    #[serde(default)]
    pub mood: Option<String>,
    /// How long the mood lasts, in seconds (60 to a week). Absent: until
    /// cleared.
    #[serde(default)]
    pub for_secs: Option<u64>,
}

impl DjStartRequest {
    pub fn zone(&self) -> Result<&str, Failure> {
        zone_name("zone", &self.zone)
    }

    /// The steering to start with; `None` when no mood is named.
    pub fn steer(&self) -> Result<Option<DjSteer>, Failure> {
        match (self.mood.as_deref(), self.for_secs) {
            (None, None) => Ok(None),
            (None, Some(_)) => Err(Failure::invalid("`for_secs` needs a `mood` to steer by")),
            (Some(mood), for_secs) => {
                steer_set(Some(mood), &SteerConstraints::default(), for_secs).map(Some)
            }
        }
    }
}

fn steer_set(
    mood: Option<&str>,
    constraints: &SteerConstraints,
    for_secs: Option<u64>,
) -> Result<DjSteer, Failure> {
    let mood = mood
        .map(|m| steer_word("`mood`", m).map(str::to_lowercase))
        .transpose()?;
    if let Some(secs) = for_secs
        && !(MIN_STEER_SECS..=MAX_STEER_SECS).contains(&secs)
    {
        return Err(Failure::invalid(format!(
            "for_secs {secs} is outside {MIN_STEER_SECS}..={MAX_STEER_SECS} (a minute to a week)"
        )));
    }
    Ok(DjSteer::Set {
        mood,
        constraints: checked(constraints)?,
        for_secs,
    })
}

/// `constraints` with every entry trimmed (periods also lowercased, `-` and
/// spaces as `_`), or the first value the DJ can't honor.
fn checked(c: &SteerConstraints) -> Result<SteerConstraints, Failure> {
    if !(-2..=2).contains(&c.energy_bias) {
        return Err(Failure::invalid(format!(
            "energy_bias {} is outside -2..=2",
            c.energy_bias
        )));
    }
    for (field, minutes) in [
        ("min_work_minutes", c.min_work_minutes),
        ("max_work_minutes", c.max_work_minutes),
    ] {
        if let Some(m) = minutes
            && !(1..=MAX_WORK_MINUTES).contains(&m)
        {
            return Err(Failure::invalid(format!(
                "{field} {m} is outside 1..={MAX_WORK_MINUTES}"
            )));
        }
    }
    if let (Some(min), Some(max)) = (c.min_work_minutes, c.max_work_minutes)
        && min > max
    {
        return Err(Failure::invalid(format!(
            "min_work_minutes {min} exceeds max_work_minutes {max}"
        )));
    }
    if c.decades.len() > MAX_STEER_LIST {
        return Err(Failure::invalid(format!(
            "decades has {} entries; the limit is {MAX_STEER_LIST}",
            c.decades.len()
        )));
    }
    if let Some(decade) = c.decades.iter().find(|&&d| d < 1000 || d % 10 != 0) {
        return Err(Failure::invalid(format!(
            "decade {decade} is not a decade's first year: say 1960 for the sixties"
        )));
    }
    let period = |p: &str| p.to_lowercase().replace(['-', ' '], "_");
    Ok(SteerConstraints {
        include_composers: steer_list("include_composers", &c.include_composers, str::to_owned)?,
        exclude_composers: steer_list("exclude_composers", &c.exclude_composers, str::to_owned)?,
        include_artists: steer_list("include_artists", &c.include_artists, str::to_owned)?,
        exclude_artists: steer_list("exclude_artists", &c.exclude_artists, str::to_owned)?,
        include_genres: steer_list("include_genres", &c.include_genres, str::to_owned)?,
        exclude_genres: steer_list("exclude_genres", &c.exclude_genres, str::to_owned)?,
        periods: steer_list("periods", &c.periods, period)?,
        include_keywords: steer_list("include_keywords", &c.include_keywords, str::to_owned)?,
        exclude_keywords: steer_list("exclude_keywords", &c.exclude_keywords, str::to_owned)?,
        ..c.clone()
    })
}

fn steer_list(
    field: &str,
    items: &[String],
    fold: impl Fn(&str) -> String,
) -> Result<Vec<String>, Failure> {
    if items.len() > MAX_STEER_LIST {
        return Err(Failure::invalid(format!(
            "{field} has {} entries; the limit is {MAX_STEER_LIST}",
            items.len()
        )));
    }
    let entry = format!("an entry of `{field}`");
    items
        .iter()
        .map(|raw| steer_word(&entry, raw).map(&fold))
        .collect()
}

/// A mood, composer, period or keyword, trimmed; `what` names it in the
/// failure.
fn steer_word<'a>(what: &str, raw: &'a str) -> Result<&'a str, Failure> {
    let word = raw.trim();
    if word.is_empty() {
        return Err(Failure::invalid(format!("{what} is empty")));
    }
    if word.chars().count() > MAX_ZONE_LEN {
        return Err(Failure::invalid(format!(
            "{what} is longer than {MAX_ZONE_LEN} characters"
        )));
    }
    Ok(word)
}

pub(crate) fn zone_name<'a>(field: &str, raw: &'a str) -> Result<&'a str, Failure> {
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
    fn mute_defaults_to_on() {
        let m: MuteRequest = serde_json::from_value(json!({ "zone": "Den" })).unwrap();
        assert!(m.mute);
        let m: MuteRequest =
            serde_json::from_value(json!({ "zone": "Den", "mute": false })).unwrap();
        assert!(!m.mute);
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

    fn steer(body: serde_json::Value) -> Result<DjSteer, Failure> {
        serde_json::from_value::<DjSteerRequest>(body)
            .map_err(|e| Failure::invalid(e.to_string()))?
            .steer()
    }

    #[test]
    fn a_steer_is_trimmed_and_checked() {
        assert_eq!(
            steer(json!({
                "zone": "Kitchen", "mood": " Focus ",
                "constraints": {
                    "include_composers": [" Bach "],
                    "periods": ["Late Romantic", "baroque"],
                    "energy_bias": -1,
                    "max_work_minutes": 30
                },
                "for_secs": 7200
            }))
            .unwrap(),
            DjSteer::Set {
                mood: Some("focus".into()),
                constraints: SteerConstraints {
                    include_composers: vec!["Bach".into()],
                    periods: vec!["late_romantic".into(), "baroque".into()],
                    energy_bias: -1,
                    max_work_minutes: Some(30),
                    ..SteerConstraints::default()
                },
                for_secs: Some(7200),
            }
        );
        assert_eq!(
            steer(json!({ "zone": "Kitchen", "clear": true })).unwrap(),
            DjSteer::Clear
        );
        for (body, says) in [
            (json!({ "zone": "Kitchen" }), "name a `mood`"),
            (
                json!({ "zone": "Kitchen", "clear": true, "mood": "focus" }),
                "`clear` takes no",
            ),
            (
                json!({ "zone": "Kitchen", "mood": "focus", "for_secs": 30 }),
                "for_secs 30 is outside",
            ),
            (
                json!({ "zone": "Kitchen", "constraints": { "energy_bias": 3 } }),
                "energy_bias 3 is outside -2..=2",
            ),
            (
                json!({ "zone": "Kitchen", "constraints": {
                    "min_work_minutes": 30, "max_work_minutes": 10 } }),
                "min_work_minutes 30 exceeds max_work_minutes 10",
            ),
            (
                json!({ "zone": "Kitchen", "constraints": { "exclude_keywords": ["  "] } }),
                "an entry of `exclude_keywords` is empty",
            ),
            (
                json!({ "zone": "Kitchen", "constraints": { "composer": ["Bach"] } }),
                "unknown field `composer`",
            ),
        ] {
            let err = steer(body.clone()).unwrap_err();
            assert!(err.detail.contains(says), "{body}: {}", err.detail);
        }
    }

    #[test]
    fn genres_are_trimmed_and_decades_are_first_years() {
        assert_eq!(
            steer(json!({
                "zone": "Kitchen",
                "constraints": {
                    "include_genres": [" jazz "],
                    "exclude_genres": ["pop"],
                    "decades": [1960, 1970]
                }
            }))
            .unwrap(),
            DjSteer::Set {
                mood: None,
                constraints: SteerConstraints {
                    include_genres: vec!["jazz".into()],
                    exclude_genres: vec!["pop".into()],
                    decades: vec![1960, 1970],
                    ..SteerConstraints::default()
                },
                for_secs: None,
            }
        );
        for (decades, says) in [
            (json!([1965]), "decade 1965 is not a decade's first year"),
            (json!([60]), "decade 60 is not a decade's first year"),
            (json!(["1960s"]), "invalid type"),
        ] {
            let body = json!({ "zone": "Kitchen", "constraints": { "decades": decades } });
            let err = steer(body.clone()).unwrap_err();
            assert!(err.detail.contains(says), "{body}: {}", err.detail);
        }
        let blank = json!({ "zone": "Kitchen", "constraints": { "include_genres": [" "] } });
        assert!(
            steer(blank)
                .unwrap_err()
                .detail
                .contains("an entry of `include_genres` is empty")
        );
    }

    #[test]
    fn a_dj_start_steers_only_with_a_mood() {
        let start = |mood: Option<&str>, for_secs| DjStartRequest {
            zone: "Kitchen".into(),
            mood: mood.map(str::to_owned),
            for_secs,
        };
        assert_eq!(start(None, None).steer().unwrap(), None);
        assert_eq!(
            start(Some("Dinner"), Some(3600)).steer().unwrap(),
            Some(DjSteer::Set {
                mood: Some("dinner".into()),
                constraints: SteerConstraints::default(),
                for_secs: Some(3600),
            })
        );
        assert!(
            start(None, Some(3600))
                .steer()
                .unwrap_err()
                .detail
                .contains("needs a `mood`")
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
