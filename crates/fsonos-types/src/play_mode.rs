//! Queue ordering and repeat settings, using Sonos's exact wire vocabulary.

use serde::{Deserialize, Serialize};

/// A coordinator's queue play mode. `Shuffle` also repeats the whole queue;
/// `ShuffleNoRepeat` is shuffle without repetition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayMode {
    #[serde(rename = "NORMAL")]
    Normal,
    #[serde(rename = "REPEAT_ALL")]
    RepeatAll,
    #[serde(rename = "REPEAT_ONE")]
    RepeatOne,
    #[serde(rename = "SHUFFLE")]
    Shuffle,
    #[serde(rename = "SHUFFLE_NOREPEAT")]
    ShuffleNoRepeat,
    #[serde(rename = "SHUFFLE_REPEAT_ONE")]
    ShuffleRepeatOne,
}

impl PlayMode {
    pub const ALL: [Self; 6] = [
        Self::Normal,
        Self::RepeatAll,
        Self::RepeatOne,
        Self::Shuffle,
        Self::ShuffleNoRepeat,
        Self::ShuffleRepeatOne,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::RepeatAll => "REPEAT_ALL",
            Self::RepeatOne => "REPEAT_ONE",
            Self::Shuffle => "SHUFFLE",
            Self::ShuffleNoRepeat => "SHUFFLE_NOREPEAT",
            Self::ShuffleRepeatOne => "SHUFFLE_REPEAT_ONE",
        }
    }

    /// Decode a wire value. Unknown or absent settings are not NORMAL.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value.trim() {
            "NORMAL" => Some(Self::Normal),
            "REPEAT_ALL" => Some(Self::RepeatAll),
            "REPEAT_ONE" => Some(Self::RepeatOne),
            "SHUFFLE" => Some(Self::Shuffle),
            "SHUFFLE_NOREPEAT" => Some(Self::ShuffleNoRepeat),
            "SHUFFLE_REPEAT_ONE" => Some(Self::ShuffleRepeatOne),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_has_a_distinct_reversible_wire_value() {
        for mode in PlayMode::ALL {
            assert_eq!(PlayMode::from_wire(mode.as_str()), Some(mode));
            let padded = format!(" {} ", mode.as_str());
            assert_eq!(PlayMode::from_wire(&padded), Some(mode));
            assert_eq!(PlayMode::ALL.iter().filter(|m| **m == mode).count(), 1);
        }
    }

    #[test]
    fn unavailable_and_unrecognized_modes_remain_unknown() {
        for value in [
            "",
            "NOT_IMPLEMENTED",
            "UNKNOWN",
            "normal",
            "SHUFFLE_NO_REPEAT",
        ] {
            assert_eq!(PlayMode::from_wire(value), None, "{value:?}");
        }
    }
}
