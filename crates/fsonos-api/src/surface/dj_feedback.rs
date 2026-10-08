//! The owner's feedback to the DJ on every surface: `fsonos dj like|dislike`,
//! `POST /dj/feedback`, and the `dj_feedback` tool.
//!
//! A like or dislike is about the work playing in a room's group, and nudges
//! its composer and performer too; the DJ's engine finds the work (the DJ's
//! own pick, or the library's work of the track playing) and records the
//! signal in the store, where the DJ's feedback model weighs it into later
//! picks. Without a room, the group the DJ is playing in is meant, when it
//! plays in exactly one. Early skips and full listens are recorded by the
//! engine itself as the playback events arrive.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};

use super::{Surface, no_dj_store, room_view};
use crate::dj::DjSpeakers;
use crate::failure::Failure;
use crate::plan::resolve;
use crate::request::zone_name;

/// An explicit verdict on the work playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DjFeedback {
    Like,
    Dislike,
}

impl DjFeedback {
    /// `like` or `dislike`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Dislike => "dislike",
        }
    }
}

/// `POST /dj/feedback` body (the `dj_feedback` tool).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DjFeedbackRequest {
    /// A room of the group the work plays in; left out, the group the DJ
    /// plays in (when it plays in exactly one).
    #[serde(default)]
    pub zone: Option<String>,
    /// `like` or `dislike`.
    pub signal: String,
}

impl DjFeedbackRequest {
    /// The trimmed room name, if one is given.
    pub fn zone(&self) -> Result<Option<&str>, Failure> {
        self.zone
            .as_deref()
            .map(|z| zone_name("zone", z))
            .transpose()
    }

    /// The verdict, or why it is unusable.
    pub fn signal(&self) -> Result<DjFeedback, Failure> {
        match self.signal.trim().to_lowercase().as_str() {
            "like" => Ok(DjFeedback::Like),
            "dislike" => Ok(DjFeedback::Dislike),
            other => Err(Failure::invalid(format!(
                "signal {other:?} is neither like nor dislike"
            ))),
        }
    }
}

/// A recorded like or dislike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjFeedbackDto {
    /// The group, named by its coordinator's room.
    pub zone: String,
    /// `Composer: Title` of the work it is about.
    pub work: String,
    pub composer: String,
    /// `like` or `dislike`.
    pub signal: String,
    /// What was recorded, and what it does, in a sentence.
    pub done: String,
}

impl Surface {
    /// Record the owner's like or dislike of the work playing in a group
    /// (`dj_feedback`); see the module docs.
    pub fn dj_feedback(
        &self,
        client: &Client,
        req: &DjFeedbackRequest,
    ) -> Result<DjFeedbackDto, Failure> {
        self.authorize_write(client, "dj_feedback")?;
        let signal = req.signal()?;
        let dj = self.dj_engine()?;
        let households = self.households()?;
        let coordinator = match req.zone()? {
            Some(zone) => {
                let aliases = self.aliases();
                resolve(room_view(&households, aliases.as_ref(), client), zone)
                    .map_err(|f| self.explain(f))?
                    .coordinator
                    .id
                    .clone()
            }
            None => self.only_dj_group(&households)?,
        };
        let at = DjSpeakers {
            transport: &*self.transport,
            households: &households,
            coordinator: &coordinator,
        };
        let result = self
            .with_store(|store| Ok(dj.feedback(at, store, signal, &*self.clock)))?
            .unwrap_or_else(|| Err(no_dj_store()));
        if let Err(f) = &result {
            self.notice(f);
        }
        let intent = format!("dj_feedback: {} in {}", signal.as_str(), coordinator.0);
        self.log_write(client, intent, &result, |d| d.done.clone());
        result
    }

    /// The coordinator of the one group the DJ plays in.
    fn only_dj_group(
        &self,
        households: &[fsonos_core::HouseholdState],
    ) -> Result<PlayerId, Failure> {
        let dj = self.dj_engine()?;
        let fed: Vec<&PlayerId> = households
            .iter()
            .flat_map(|h| &h.groups)
            .map(|g| &g.coordinator)
            .filter(|c| dj.feeds(c))
            .collect();
        match fed.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(Failure::invalid(
                "the DJ plays in no group here, so name the room the work plays in",
            )),
            many => Err(Failure::invalid(format!(
                "the DJ plays in {} groups, so name the room the work plays in",
                many.len()
            ))
            .with_suggestions(many.iter().filter_map(|c| {
                fsonos_core::control::locate(households, c)
                    .ok()
                    .map(|p| p.room_name.clone())
            }))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::ErrorCode;
    use crate::surface::testing::surface;
    use crate::zones::fixtures::households;

    fn req(zone: Option<&str>, signal: &str) -> DjFeedbackRequest {
        DjFeedbackRequest {
            zone: zone.map(Into::into),
            signal: signal.into(),
        }
    }

    #[test]
    fn a_feedback_request_is_a_like_or_a_dislike() {
        assert_eq!(req(None, "like").signal(), Ok(DjFeedback::Like));
        assert_eq!(req(None, " Dislike ").signal(), Ok(DjFeedback::Dislike));
        assert_eq!(
            req(None, "love").signal().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(req(Some(" Patio "), "like").zone(), Ok(Some("Patio")));
        assert_eq!(
            req(Some("  "), "like").zone().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(DjFeedback::Dislike.as_str(), "dislike");
    }

    #[test]
    fn without_a_dj_there_is_no_feedback_and_a_reader_may_not_give_it() {
        let (s, sent) = surface("", households());
        let none = s
            .dj_feedback(&Client::Cli, &req(Some("Patio"), "like"))
            .unwrap_err();
        assert_eq!(none.code, ErrorCode::NotImplemented);
        let denied = s
            .dj_feedback(&Client::Unknown, &req(Some("Patio"), "like"))
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);
        assert!(sent.lock().unwrap().is_empty());
    }
}
