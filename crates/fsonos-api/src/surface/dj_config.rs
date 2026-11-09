//! Reusable, owner-wide DJ moods and automatic daily programs.
//!
//! The editable document contains only mood overrides. `programs: null`
//! retains the built-in program; `[]` disables it. Writes replace this
//! document only at the revision the caller read. The engine persists it
//! atomically and makes it available for the next pick, without touching
//! playback, queued music, or a zone's temporary steering.

use std::collections::BTreeMap;

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};

use super::Surface;
use crate::dj::SteerConstraints;
use crate::failure::Failure;

pub(super) mod restore;

/// Remote-edit limits; the Spotify persistence layer independently enforces
/// the same document limits for cooperative local writers.
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;
pub const MAX_CUSTOM_MOODS: usize = 128;
pub const MAX_PROGRAMS: usize = 256;

/// A complete stored mood. Unlike temporary steering, this preserves a
/// mood's song alternatives and optional absolute expiry when it is edited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MoodDefinitionDto {
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub periods: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_periods: Vec<String>,
    /// Alternatives for songs when classical periods are specified.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub song_keywords: Vec<String>,
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub strict: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl MoodDefinitionDto {
    fn checked(&self) -> Result<Self, Failure> {
        // One validation path for the rules shared with temporary steering.
        let checked = crate::request::checked(&SteerConstraints {
            include_composers: self.include_composers.clone(),
            exclude_composers: self.exclude_composers.clone(),
            include_artists: self.include_artists.clone(),
            exclude_artists: self.exclude_artists.clone(),
            include_genres: self.include_genres.clone(),
            exclude_genres: self.exclude_genres.clone(),
            periods: self.periods.clone(),
            exclude_periods: self.exclude_periods.clone(),
            decades: self.decades.clone(),
            exclude_decades: self.exclude_decades.clone(),
            include_keywords: self.include_keywords.clone(),
            exclude_keywords: self.exclude_keywords.clone(),
            min_work_minutes: self.min_work_minutes,
            max_work_minutes: self.max_work_minutes,
            energy_bias: self.energy_bias,
            allow_long_works: self.allow_long_works,
            strict: self.strict,
        })?;
        crate::request::steer_list("song_keywords", &self.song_keywords, str::to_owned)?;
        // Validate using the common rules without rewriting untouched authored
        // strings during a whole-document edit. Period enums are the one
        // canonical representation required by the model boundary.
        Ok(Self {
            periods: checked.periods,
            exclude_periods: checked.exclude_periods,
            ..self.clone()
        })
    }
}

/// One ordered automatic program. Overnight windows belong to their
/// starting day; equal times mean the whole day. The first match wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProgramDefinitionDto {
    /// Days on which the window starts: mon, tue, wed, thu, fri, sat, sun.
    pub days: Vec<String>,
    /// Local wall time, HH:MM (24:00 is accepted).
    pub from: String,
    pub to: String,
    pub mood: String,
}

/// The editable owner overrides, separate from the resolved built-ins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DjConfigDocument {
    /// Set a built-in name to override it, or another name to add a mood.
    /// Removing an override restores the built-in or removes the custom mood.
    pub moods: BTreeMap<String, MoodDefinitionDto>,
    /// Null uses default programs; [] disables them; an array replaces them.
    pub programs: Option<Vec<ProgramDefinitionDto>>,
}

impl DjConfigDocument {
    /// The same input schema for the HTTP API and the typed MCP tool. The
    /// pinned MCP derive accepts this inherent fallback for external types.
    #[must_use]
    pub fn json_schema() -> serde_json::Value {
        serde_json::to_value(<Self as fastapi_openapi::JsonSchema>::schema())
            .expect("DJ configuration schema is JSON")
    }
}

/// Current editable configuration, exact revision, and its effective values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjConfigDto {
    pub revision: String,
    pub config: DjConfigDocument,
    /// Factory definitions, so an editor can preview removing an override.
    pub builtin_moods: BTreeMap<String, MoodDefinitionDto>,
    /// Default ordered daily program, for previewing a reset before saving.
    pub builtin_programs: Vec<ProgramDefinitionDto>,
    /// Built-ins with the owner's overrides applied.
    pub moods: BTreeMap<String, MoodDefinitionDto>,
    /// Effective ordered program, including defaults when not overridden.
    pub programs: Vec<ProgramDefinitionDto>,
}

/// Replace the owner-wide configuration at exactly the revision read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DjConfigureRequest {
    /// Exact revision returned by GET /dj/configuration. Never guess it.
    pub expected_revision: String,
    pub config: DjConfigDocument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjConfiguredDto {
    pub done: String,
    pub changed: bool,
    pub configuration: DjConfigDto,
}

fn name(raw: &str) -> Result<String, Failure> {
    let value = raw;
    if value.trim().is_empty() || value.chars().count() > 128 || value.chars().any(char::is_control)
    {
        return Err(Failure::invalid(
            "a mood name needs 1–128 characters without control characters",
        ));
    }
    Ok(value.to_lowercase())
}

fn time(value: &str) -> Result<String, Failure> {
    let value = value.trim();
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || ![bytes[0], bytes[1], bytes[3], bytes[4]]
            .iter()
            .all(u8::is_ascii_digit)
    {
        return Err(Failure::invalid(
            "program times use HH:MM, from 00:00 through 24:00",
        ));
    }
    let hours = (bytes[0] - b'0') * 10 + bytes[1] - b'0';
    let minutes = (bytes[3] - b'0') * 10 + bytes[4] - b'0';
    if hours > 24 || minutes > 59 || (hours == 24 && minutes != 0) {
        return Err(Failure::invalid(
            "program times use HH:MM, from 00:00 through 24:00",
        ));
    }
    Ok(value.to_owned())
}

impl DjConfigureRequest {
    /// Bounded, canonical remote input. Resolution of mood references and
    /// standing preferences belongs to the engine, before persistence.
    pub fn checked(&self) -> Result<Self, Failure> {
        if self.expected_revision.len() != 64
            || !self
                .expected_revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Failure::invalid(
                "expected_revision must be the 64-character revision returned by GET /dj/configuration",
            ));
        }
        if self.config.moods.len() > MAX_CUSTOM_MOODS
            || self
                .config
                .programs
                .as_ref()
                .is_some_and(|p| p.len() > MAX_PROGRAMS)
            || serde_json::to_vec(&self.config)
                .map_err(|e| Failure::invalid(e.to_string()))?
                .len()
                > MAX_CONFIG_BYTES
        {
            return Err(Failure::invalid(format!(
                "DJ configuration exceeds its limits: {MAX_CONFIG_BYTES} bytes, {MAX_CUSTOM_MOODS} mood overrides, {MAX_PROGRAMS} programs"
            )));
        }
        let mut moods = BTreeMap::new();
        for (raw, mood) in &self.config.moods {
            let key = name(raw)?;
            if moods.insert(key.clone(), mood.checked()?).is_some() {
                return Err(Failure::invalid(format!(
                    "mood names collide after case folding: {key}"
                )));
            }
        }
        let programs = self
            .config
            .programs
            .as_ref()
            .map(|programs| {
                programs
                    .iter()
                    .map(|program| {
                        const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
                        if program.days.is_empty() || program.days.len() > 7 {
                            return Err(Failure::invalid("a program needs 1–7 starting days"));
                        }
                        let mut present = [false; 7];
                        for raw in &program.days {
                            let day = raw.trim().to_lowercase();
                            let index = DAYS.iter().position(|d| *d == day).ok_or_else(|| {
                                Failure::invalid(format!(
                                    "unknown program day {raw:?}; use mon through sun"
                                ))
                            })?;
                            if present[index] {
                                return Err(Failure::invalid(format!(
                                    "program day {day} is repeated"
                                )));
                            }
                            present[index] = true;
                        }
                        Ok(ProgramDefinitionDto {
                            days: DAYS
                                .iter()
                                .zip(present)
                                .filter(|(_, set)| *set)
                                .map(|(day, _)| (*day).to_owned())
                                .collect(),
                            from: time(&program.from)?,
                            to: time(&program.to)?,
                            mood: name(&program.mood)?,
                        })
                    })
                    .collect::<Result<Vec<_>, Failure>>()
            })
            .transpose()?;
        Ok(Self {
            expected_revision: self.expected_revision.clone(),
            config: DjConfigDocument { moods, programs },
        })
    }
}

impl Surface {
    /// Read owner-wide configuration without resolving a speaker or library.
    pub fn dj_configuration(&self, client: &Client) -> Result<DjConfigDto, Failure> {
        self.guard(client).authorize("dj_configuration", true)?;
        self.dj_engine()?.configuration()
    }

    /// Atomically replace reusable moods and automatic programs, logged under
    /// the DJ policy grant. No speaker operation or playback undo is needed.
    pub fn dj_configure(
        &self,
        client: &Client,
        request: &DjConfigureRequest,
    ) -> Result<DjConfiguredDto, Failure> {
        self.authorize_write(client, "dj_configure")?;
        let checked = request.checked()?;
        let _configuration = self
            .dj_configuration_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dj = self.dj_engine()?;
        // Use the existing restoring guard and per-call SharedStore locks;
        // never hold the database mutex while taking the DJ-state lock.
        let result = self
            .with_dj_store(|store| Ok(dj.configure(&checked, store, &*self.clock)))?
            .unwrap_or_else(|| Err(super::no_dj_store()));
        self.record(
            client,
            "dj_configure".to_owned(),
            "allow".to_owned(),
            match &result {
                Ok(done) => done.done.clone(),
                Err(failure) => format!("failed: {}", failure.detail),
            },
            None,
        );
        result
    }
}

#[cfg(test)]
mod tests;
