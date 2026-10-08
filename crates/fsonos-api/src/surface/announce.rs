//! Announcements on every surface: `POST /announce`, the MCP `announce`
//! tool, and `fsonos say|chime`.
//!
//! The clip is made in the media directory (speech with macOS `say`, or a
//! synthesized chime) and served read-only to the players from the media
//! listener (the daemon's GENA sink, see `fsonos_proto::net::MediaFiles`).
//! [`Announcer`] plays it in the target rooms and puts every zone back
//! afterwards. Each room's level is capped by the house policy for the
//! caller. Announcements are logged, but not undoable: they restore
//! themselves.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::HouseholdState;
use fsonos_core::announce::clip::{Chime, ClipError, ClipSource, MediaStore};
use fsonos_core::announce::{
    AnnounceReport, Announcer, Clip, DEFAULT_VOLUME, Ending, HouseholdAnnouncement,
};
use fsonos_core::policy::Client;
use fsonos_core::rooms::{ControlTarget, ResolveContext, resolve_targets};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::time::SystemTime;

use super::{Surface, room_view};
use crate::failure::{ErrorCode, Failure};
use crate::plan::Rooms;

/// The tool name announcements are authorized and logged under.
pub const TOOL: &str = "announce";

/// The longest title a clip's DIDL carries, in characters.
const TITLE_CHARS: usize = 60;

/// Where the players fetch clips: the media listener's base URL
/// (`http://<address>:<port>`), once it is up.
pub type MediaBase = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// What a surface needs to announce: the clips and where the players fetch
/// them, and the announcer that takes one announcement per household at a
/// time.
pub struct Announcements {
    announcer: Announcer,
    media: MediaStore,
    base: MediaBase,
}

impl Announcements {
    #[must_use]
    pub fn new(media: MediaStore, base: MediaBase) -> Self {
        Self {
            announcer: Announcer::new(),
            media,
            base,
        }
    }

    /// Use `announcer` (its own timing, say) instead of the default.
    #[must_use]
    pub fn with_announcer(mut self, announcer: Announcer) -> Self {
        self.announcer = announcer;
        self
    }
}

/// `POST /announce` / the `announce` tool: speak `text` or play a `chime`
/// in `rooms`, then put the music back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnounceRequest {
    /// Text to speak (macOS `say`), at most 1000 characters. Give this or
    /// `chime`.
    #[serde(default)]
    pub text: Option<String>,
    /// A chime: `bell`, `beep` or `rise`. Give this or `text`.
    #[serde(default)]
    pub chime: Option<String>,
    /// The `say` voice for `text`; the system voice when omitted.
    #[serde(default)]
    pub voice: Option<String>,
    /// Rooms, aliases, or `all`; every room when omitted.
    #[serde(default)]
    pub rooms: Vec<String>,
    /// The level to announce at, 0 to 100 (35 when omitted). The house
    /// policy caps it per room.
    #[serde(default)]
    pub volume: Option<i64>,
}

impl AnnounceRequest {
    /// The clip asked for, or why the request is unusable.
    pub fn source(&self) -> Result<ClipSource, Failure> {
        let text = self.text.as_deref().map(str::trim);
        let chime = self.chime.as_deref().map(str::trim);
        match (text, chime) {
            (Some(_), Some(_)) => Err(Failure::invalid(
                "give either `text` to speak or a `chime`, not both",
            )),
            (None, None) => Err(Failure::invalid("give `text` to speak or a `chime`")
                .with_suggestions(Chime::NAMES.to_vec())),
            (Some(text), None) => Ok(ClipSource::Tts {
                text: text.to_string(),
                voice: self.voice.as_deref().map(str::trim).map(str::to_string),
            }),
            (None, Some(_)) if self.voice.is_some() => {
                Err(Failure::invalid("`voice` is for `text`; a chime has none"))
            }
            (None, Some(name)) => Ok(ClipSource::Chime {
                name: name.to_string(),
            }),
        }
    }

    /// The level asked for, before the policy's caps.
    pub fn volume(&self) -> Result<u8, Failure> {
        match self.volume {
            None => Ok(DEFAULT_VOLUME),
            Some(v) => u8::try_from(v)
                .ok()
                .filter(|v| *v <= 100)
                .ok_or_else(|| Failure::invalid(format!("`volume` {v} is not 0 to 100"))),
        }
    }
}

/// What an announcement did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnnounceDto {
    /// One line for people.
    pub done: String,
    /// Played to the end everywhere, and every zone put back as it was.
    pub clean: bool,
    pub households: Vec<AnnouncedDto>,
}

/// What one household heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnnouncedDto {
    /// The room that played the clip for the others.
    pub lead: String,
    pub rooms: Vec<String>,
    /// Each room's level, after the policy's cap.
    pub levels: Vec<LevelDto>,
    /// `finished`, `deadline` (stopped at its length plus a grace),
    /// `never_started` (the players could not fetch it), or `failed`.
    pub outcome: String,
    /// Why it failed, for `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What could not be put back as it was, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_restored: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LevelDto {
    pub room: String,
    pub volume: u8,
}

impl Surface {
    /// Announce with `announcements`; without them, [`Self::announce`]
    /// answers `NOT_IMPLEMENTED`.
    #[must_use]
    pub fn with_announcements(mut self, announcements: Announcements) -> Self {
        self.announcements = Some(announcements);
        self
    }

    /// Speak or chime in the rooms `req` names (every room by default) for
    /// `client`, then put everything back. Blocks until done, including any
    /// wait for an earlier announcement in the same household.
    pub fn announce(&self, client: &Client, req: &AnnounceRequest) -> Result<AnnounceDto, Failure> {
        let guard = self.guard(client);
        if let Err(denied) = guard.authorize(TOOL, false) {
            self.record(
                client,
                TOOL.to_string(),
                format!("deny: {}", denied.detail),
                denied.detail.clone(),
                None,
            );
            return Err(denied);
        }
        let source = req.source()?;
        let volume = req.volume()?;
        let Some(with) = &self.announcements else {
            return Err(Failure::new(
                ErrorCode::NotImplemented,
                "announcements need a media listener the speakers can fetch the clip from, \
                 which this surface does not run",
            ));
        };
        let base = (with.base)().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotReady,
                "the media listener is not up yet; retry in a few seconds",
            )
        })?;
        let households = self.households()?;
        let aliases = self.aliases();
        let rooms = room_view(&households, aliases.as_ref(), client);
        let targets = targets(&rooms, &req.rooms).map_err(|f| self.explain(f))?;

        if let Err(e) = with.media.prune(SystemTime::now()) {
            tracing::debug!(error = %e, "old clips not pruned");
        }
        let stored = with.media.make(&source).map_err(|e| clip_failure(&e))?;
        let clip = Clip {
            url: stored.url(&base),
            title: title(&source),
            duration: stored.duration,
        };
        let policy = &self.policy;
        let now = self.clock.now();
        let cap = |room: &str| policy.max_volume(room, client, now);
        let report = with
            .announcer
            .announce(&*self.transport, &targets, &clip, volume, &cap)
            .map_err(|e| Failure::invalid(e.to_string()))?;

        let dto = AnnounceDto::from_report(&households, &report);
        let clamped: Vec<String> = dto
            .households
            .iter()
            .flat_map(|h| &h.levels)
            .filter(|l| l.volume < volume)
            .map(|l| format!("{} capped at {}", l.room, l.volume))
            .collect();
        let decision = if clamped.is_empty() {
            "allow".to_string()
        } else {
            format!("clamp: {}", clamped.join("; "))
        };
        let names: Vec<&str> = targets.iter().map(|t| t.room.name.as_str()).collect();
        let intent = format!("{TOOL}: {} in {}", clip.title, names.join(", "));
        // Nothing to undo: the announcement put everything back itself.
        self.record(client, intent, decision, dto.done.clone(), None);
        Ok(dto)
    }
}

/// The rooms `names` resolve to (every room when there are none), each
/// once.
fn targets<'a>(rooms: &Rooms<'a>, names: &[String]) -> Result<Vec<ControlTarget<'a>>, Failure> {
    if rooms.households.iter().all(|h| h.rooms.is_empty()) {
        return Err(Failure::new(
            ErrorCode::NotReady,
            "no rooms discovered yet; discovery may still be running, retry in a few seconds",
        ));
    }
    let everywhere = ["all".to_string()];
    let names = if names.is_empty() {
        &everywhere[..]
    } else {
        names
    };
    let mut out: Vec<ControlTarget<'a>> = Vec::new();
    for name in names {
        let name = crate::request::zone_name("rooms", name)?;
        let ctx = ResolveContext {
            aliases: rooms.aliases,
            client: rooms.client,
        };
        for target in resolve_targets(rooms.households, name, ctx)?.targets {
            if !out.iter().any(|t| t.room.primary == target.room.primary) {
                out.push(target);
            }
        }
    }
    Ok(out)
}

/// The clip's title: the words spoken (shortened), or the chime's name.
fn title(source: &ClipSource) -> String {
    match source {
        ClipSource::Tts { text, .. } if text.chars().count() > TITLE_CHARS => {
            let short: String = text.chars().take(TITLE_CHARS - 1).collect();
            format!("{}…", short.trim_end())
        }
        ClipSource::Tts { text, .. } => text.clone(),
        ClipSource::Chime { name } => format!("{} chime", name.to_lowercase()),
    }
}

/// A clip that could not be made, as a failure.
fn clip_failure(e: &ClipError) -> Failure {
    match e {
        ClipError::UnknownChime { .. } => {
            Failure::invalid(e.to_string()).with_suggestions(Chime::NAMES.to_vec())
        }
        ClipError::EmptySpeech | ClipError::SpeechTooLong(_) | ClipError::BadVoice(_) => {
            Failure::invalid(e.to_string())
        }
        ClipError::Unsupported(_) => Failure::new(ErrorCode::NotImplemented, e.to_string())
            .with_hint("Speech needs macOS `say` on the daemon's host; chimes work anywhere."),
        ClipError::Speech(_) | ClipError::NotWav(_) | ClipError::Io(_) => {
            Failure::new(ErrorCode::Internal, e.to_string())
        }
    }
}

impl AnnounceDto {
    fn from_report(households: &[HouseholdState], report: &AnnounceReport) -> Self {
        let name = |id: &PlayerId| {
            households
                .iter()
                .find_map(|h| h.player(id))
                .map_or_else(|| id.0.clone(), |p| p.room_name.clone())
        };
        let announced: Vec<AnnouncedDto> = report
            .households
            .iter()
            .map(|h| {
                let (outcome, error) = match &h.outcome {
                    Ok(Ending::Finished) => ("finished", None),
                    Ok(Ending::Deadline) => ("deadline", None),
                    Ok(Ending::NeverStarted) => ("never_started", None),
                    Err(why) => ("failed", Some(why.clone())),
                };
                let mut not_restored: Vec<String> = h
                    .restored
                    .iter()
                    .flat_map(|(zone, r)| {
                        r.skipped.iter().map(move |(aspect, why)| {
                            let aspect = format!("{aspect:?}").to_lowercase();
                            format!("{} {aspect}: {why}", name(zone))
                        })
                    })
                    .collect();
                not_restored.extend(
                    h.restore_failed
                        .iter()
                        .map(|(zone, why)| format!("{}: {why}", name(zone))),
                );
                AnnouncedDto {
                    lead: name(&h.lead),
                    rooms: h.rooms.clone(),
                    levels: h
                        .levels
                        .iter()
                        .map(|(room, volume)| LevelDto {
                            room: room.clone(),
                            volume: *volume,
                        })
                        .collect(),
                    outcome: outcome.to_string(),
                    error,
                    not_restored,
                }
            })
            .collect();
        let clean = report
            .households
            .iter()
            .all(HouseholdAnnouncement::is_clean);
        let rooms: Vec<&str> = announced
            .iter()
            .flat_map(|h| h.rooms.iter().map(String::as_str))
            .collect();
        let mut done = format!("announced in {}", rooms.join(", "));
        if clean {
            done.push_str("; everything put back");
        }
        for h in &announced {
            let _ = match (&h.error, h.outcome.as_str()) {
                (Some(why), _) => write!(done, "; {} failed: {why}", h.lead),
                (None, "never_started") => write!(done, "; {} could not fetch the clip", h.lead),
                (None, "deadline") => write!(done, "; {} was stopped at the clip's end", h.lead),
                _ => Ok(()),
            };
            for problem in &h.not_restored {
                let _ = write!(done, "; not put back: {problem}");
            }
        }
        Self {
            done,
            clean,
            households: announced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::ErrorCode;

    fn req(text: Option<&str>, chime: Option<&str>) -> AnnounceRequest {
        AnnounceRequest {
            text: text.map(Into::into),
            chime: chime.map(Into::into),
            ..AnnounceRequest::default()
        }
    }

    #[test]
    fn a_request_names_speech_or_a_chime() {
        assert_eq!(
            req(Some(" dinner "), None).source().unwrap(),
            ClipSource::Tts {
                text: "dinner".into(),
                voice: None
            }
        );
        assert_eq!(
            req(None, Some("bell")).source().unwrap(),
            ClipSource::Chime {
                name: "bell".into()
            }
        );
        for bad in [req(None, None), req(Some("hi"), Some("bell"))] {
            assert_eq!(bad.source().unwrap_err().code, ErrorCode::InvalidArgument);
        }
        let voiced_chime = AnnounceRequest {
            voice: Some("Samantha".into()),
            ..req(None, Some("bell"))
        };
        assert!(voiced_chime.source().is_err());
    }

    #[test]
    fn volume_defaults_and_is_bounded() {
        assert_eq!(req(None, Some("bell")).volume().unwrap(), DEFAULT_VOLUME);
        for (v, ok) in [(0, true), (100, true), (101, false), (-1, false)] {
            let r = AnnounceRequest {
                volume: Some(v),
                ..req(None, Some("bell"))
            };
            assert_eq!(r.volume().is_ok(), ok, "{v}");
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_str::<AnnounceRequest>(r#"{"text":"hi","room":"Kitchen"}"#);
        assert!(err.is_err());
    }

    #[test]
    fn clip_failures_carry_stable_codes() {
        let unknown = clip_failure(&ClipError::UnknownChime {
            name: "gong".into(),
        });
        assert_eq!(unknown.code, ErrorCode::InvalidArgument);
        assert!(
            unknown.suggestions.iter().any(|s| s == "bell"),
            "{unknown:?}"
        );
        assert_eq!(
            clip_failure(&ClipError::Unsupported("no say here")).code,
            ErrorCode::NotImplemented
        );
        assert_eq!(
            clip_failure(&ClipError::SpeechTooLong(1001)).code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn long_speech_gets_a_short_title() {
        let long = "a".repeat(200);
        let t = title(&ClipSource::Tts {
            text: long,
            voice: None,
        });
        assert_eq!(t.chars().count(), TITLE_CHARS);
        assert_eq!(
            title(&ClipSource::Chime {
                name: "Bell".into()
            }),
            "bell chime"
        );
    }
}
