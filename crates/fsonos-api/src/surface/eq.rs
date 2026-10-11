//! A room's tone on every surface (`get_eq` and `set_eq`,
//! `GET /zones/{room}/eq` and `POST /eq`, `fsonos eq`): its bass and treble
//! from -10 to 10 (0 is flat) and whether loudness compensation is on.
//!
//! A change is a control call like any other: authorized as `set_eq`, and
//! logged with the room's tone before it, so `undo` puts the tone back.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions::{self, ToneBefore};
use fsonos_core::control::{self, Tone, ToneChange};
use fsonos_core::policy::Client;
use fsonos_proto::control::TONE_RANGE;
use serde::{Deserialize, Serialize};

use super::{Surface, resolve, room_view};
use crate::failure::Failure;

/// The tool name a tone read is authorized as.
pub const READ: &str = "get_eq";

/// The tool name a tone change is authorized as.
pub const WRITE: &str = "set_eq";

/// A room's tone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EqDto {
    pub room: String,
    /// -10 to 10; 0 is flat.
    pub bass: i8,
    /// -10 to 10; 0 is flat.
    pub treble: i8,
    /// Loudness compensation (more bass and treble at low volume).
    pub loudness: bool,
}

impl EqDto {
    fn new(room: String, tone: Tone) -> Self {
        Self {
            room,
            bass: tone.bass,
            treble: tone.treble,
            loudness: tone.loudness,
        }
    }

    /// One line: `Kitchen: bass +2, treble 0, loudness on.`
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}: {}.\n", self.room, self.tone())
    }

    fn tone(&self) -> String {
        format!(
            "bass {}, treble {}, loudness {}",
            level(self.bass),
            level(self.treble),
            on_off(self.loudness)
        )
    }
}

/// `POST /eq` body (`set_eq`): the parts of `zone`'s tone to change; at
/// least one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EqRequest {
    pub zone: String,
    /// -10 to 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bass: Option<i64>,
    /// -10 to 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treble: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loudness: Option<bool>,
}

impl EqRequest {
    /// The change asked for, or `INVALID_ARGUMENT` for a level out of range
    /// or nothing to change.
    fn change(&self) -> Result<ToneChange, Failure> {
        let checked = |name: &str, given: Option<i64>| {
            given
                .map(|n| {
                    i8::try_from(n)
                        .ok()
                        .filter(|l| TONE_RANGE.contains(l))
                        .ok_or_else(|| {
                            Failure::invalid(format!("{name} must be from -10 to 10, not {n}"))
                        })
                })
                .transpose()
        };
        let change = ToneChange {
            bass: checked("bass", self.bass)?,
            treble: checked("treble", self.treble)?,
            loudness: self.loudness,
        };
        if change.is_empty() {
            return Err(Failure::invalid(
                "nothing to change: give bass, treble or loudness",
            ));
        }
        Ok(change)
    }
}

/// What a tone change did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EqSetDto {
    /// One line for people.
    pub done: String,
    /// The room's tone now.
    pub eq: EqDto,
}

impl Surface {
    /// The tone of the room `zone` (`get_eq`, read-only).
    pub fn get_eq(&self, client: &Client, zone: &str) -> Result<EqDto, Failure> {
        self.guard(client).authorize(READ, true)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let tone = control::tone(&*self.transport, &households, &target.player.id)
            .map_err(Failure::from)
            .inspect_err(|f| self.notice(f))?;
        Ok(EqDto::new(target.room.name.clone(), tone))
    }

    /// Change the parts of a room's tone `req` names (`set_eq`), logged with
    /// the tone before it so `undo` puts it back.
    pub fn set_eq(&self, client: &Client, req: &EqRequest) -> Result<EqSetDto, Failure> {
        self.authorize_logged(client, WRITE)?;
        let change = req.change()?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), &req.zone)
            .map_err(|f| self.explain(f))?;
        let player = target.player.id.clone();
        let room = target.room.name.clone();
        let before = control::tone(&*self.transport, &households, &player)
            .map_err(Failure::from)
            .inspect_err(|f| self.notice(f))?;
        let intent = format!("{WRITE}: {room} {}", asked(change));
        let undo = actions::before_state_tones(&[ToneBefore {
            player: player.clone(),
            tone: before,
        }]);
        // A change that fails part-way is logged too, so undo can put back
        // what it did change.
        if let Err(e) = control::set_tone(&*self.transport, &households, &player, change) {
            let failure = Failure::from(e);
            self.notice(&failure);
            self.record(
                client,
                intent,
                "allow".into(),
                format!("failed: {}", failure.detail),
                undo,
            );
            return Err(failure);
        }
        let eq = EqDto::new(
            room,
            Tone {
                bass: change.bass.unwrap_or(before.bass),
                treble: change.treble.unwrap_or(before.treble),
                loudness: change.loudness.unwrap_or(before.loudness),
            },
        );
        let done = format!("{}'s tone is {}", eq.room, eq.tone());
        self.record(client, intent, "allow".into(), done.clone(), undo);
        Ok(EqSetDto { done, eq })
    }
}

/// `+3`, `0`, `-2`.
fn level(n: i8) -> String {
    if n > 0 {
        format!("+{n}")
    } else {
        n.to_string()
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The parts `change` names: `bass +3 loudness off`.
fn asked(change: ToneChange) -> String {
    let mut parts = Vec::new();
    if let Some(bass) = change.bass {
        parts.push(format!("bass {}", level(bass)));
    }
    if let Some(treble) = change.treble {
        parts.push(format!("treble {}", level(treble)));
    }
    if let Some(loudness) = change.loudness {
        parts.push(format!("loudness {}", on_off(loudness)));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::ErrorCode;

    fn ask(bass: Option<i64>, treble: Option<i64>, loudness: Option<bool>) -> EqRequest {
        EqRequest {
            zone: "Kitchen".into(),
            bass,
            treble,
            loudness,
        }
    }

    #[test]
    fn a_change_names_a_part_and_keeps_levels_in_range() {
        let change = ask(Some(-10), None, Some(false)).change().unwrap();
        assert_eq!(
            change,
            ToneChange {
                bass: Some(-10),
                treble: None,
                loudness: Some(false),
            }
        );
        assert_eq!(asked(change), "bass -10 loudness off");
        for wrong in [ask(Some(11), None, None), ask(None, Some(-300), None)] {
            let err = wrong.change().unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
            assert!(err.detail.contains("from -10 to 10"), "{err:?}");
        }
        let nothing = ask(None, None, None).change().unwrap_err();
        assert!(nothing.detail.contains("nothing to change"), "{nothing:?}");
    }

    #[test]
    fn the_text_signs_the_levels() {
        let eq = EqDto::new(
            "Kitchen".into(),
            Tone {
                bass: 2,
                treble: 0,
                loudness: true,
            },
        );
        assert_eq!(eq.text(), "Kitchen: bass +2, treble 0, loudness on.\n");
    }
}
