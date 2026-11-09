//! Announcements on every surface: `POST /announce`, the MCP `announce`
//! tool, and `fsonos say|chime|announce`.
//!
//! The clip is made in the media directory (local speech, a synthesized
//! chime, or an uploaded WAV) and served read-only to the players from the media
//! listener (the daemon's GENA sink, see `fsonos_proto::net::MediaFiles`).
//! [`Announcer`] plays it in the target rooms and puts every zone back
//! afterwards. Each room's level is capped by the house policy for the
//! caller. Announcements are logged, but not undoable: they restore
//! themselves.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::announce::clip::{
    Chime, ClipError, ClipSource, MAX_WAV_BYTES, MediaStore, wav_info,
};
use fsonos_core::announce::{
    AnnounceReport, AnnouncementCallbacks, AnnouncementControl, Announcer, Clip, DEFAULT_VOLUME,
    Ending, HouseholdAnnouncement,
};
use fsonos_core::policy::Client;
use fsonos_core::rooms::{ControlTarget, ResolveContext, resolve_targets};
use fsonos_core::{HouseholdState, Room};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use super::{Surface, room_view};
use crate::failure::{ErrorCode, Failure};
use crate::plan::Rooms;

mod jobs;
pub use jobs::{AnnouncementJobDto, AnnouncementJobPhase, AnnouncementProgressDto};

/// The tool name announcements are authorized and logged under.
pub const TOOL: &str = "announce";

/// The longest title a clip's DIDL carries, in characters.
const TITLE_CHARS: usize = 60;

/// The largest base64-encoded WAV accepted, checked before decoding.
pub const MAX_WAV_BASE64_BYTES: usize = MAX_WAV_BYTES.div_ceil(3) * 4;

/// A WAV upload plus room names and the JSON / MCP envelope.
pub const MAX_ANNOUNCE_REQUEST_BYTES: usize = MAX_WAV_BASE64_BYTES + 16 * 1024;

/// Time for local speech synthesis, playback, and restoration over HTTP or MCP.
pub const ANNOUNCE_TIMEOUT_SECS: u64 = 1200;

/// Announcements one client may have playing or waiting their turn at once.
/// Each holds a call open (and one of the daemon's long-call threads) until
/// it has played, so an agent looping `announce` must not queue without end.
pub const MAX_PER_CLIENT: usize = 2;

/// Announcements all clients together may have playing or waiting.
pub const MAX_PENDING: usize = 8;

/// The announcements each client has playing or waiting.
#[derive(Debug, Default)]
struct Turns {
    held: Mutex<HashMap<String, usize>>,
}

/// One announcement's place in [`Turns`], given back when it is dropped:
/// played, failed or refused later on.
struct Turn {
    turns: Arc<Turns>,
    client: String,
}

/// A scheduler reserves capacity before claiming an occurrence; delivery
/// consumes this same lease so another caller cannot take its place.
pub(super) struct AnnouncementPermit(Turn);

impl Turns {
    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A place for one more announcement from `client`, or `NOT_READY`
    /// (retryable) while it already has [`MAX_PER_CLIENT`], or everyone
    /// together [`MAX_PENDING`].
    fn take(self: &Arc<Self>, client: &Client) -> Result<Turn, Failure> {
        let mut held = self.held();
        let mine = held.get(client.key()).copied().unwrap_or(0);
        let all: usize = held.values().sum();
        if mine >= MAX_PER_CLIENT || all >= MAX_PENDING {
            let detail = if mine >= MAX_PER_CLIENT {
                format!("{client} has {mine} announcements playing or waiting already")
            } else {
                format!("{all} announcements are playing or waiting already")
            };
            return Err(Failure::new(ErrorCode::NotReady, detail)
                .with_hint("Retry once an earlier announcement has played."));
        }
        *held.entry(client.key().to_owned()).or_default() += 1;
        Ok(Turn {
            turns: Arc::clone(self),
            client: client.key().to_owned(),
        })
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut held = self.turns.held();
        if let Some(count) = held.get_mut(&self.client) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&self.client);
            }
        }
    }
}

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
    turns: Arc<Turns>,
    jobs: jobs::Jobs,
}

impl Announcements {
    #[must_use]
    pub fn new(media: MediaStore, base: MediaBase) -> Self {
        Self {
            announcer: Announcer::new(),
            media,
            base,
            turns: Arc::new(Turns::default()),
            jobs: jobs::Jobs::default(),
        }
    }
}

/// `POST /announce` / the `announce` tool: speak `text`, play a `chime`, or
/// upload `wav_base64` in `rooms`, then put the music back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnounceRequest {
    /// Text for the host's local speech backend, at most 1000 characters.
    /// Give exactly one of `text`, `chime`, or `wav_base64`.
    #[serde(default)]
    pub text: Option<String>,
    /// A chime: `bell`, `beep` or `rise`.
    #[serde(default)]
    pub chime: Option<String>,
    /// A 16-bit PCM WAV as standard padded base64, at most 16 MiB decoded
    /// and five minutes long.
    /// Upload bytes; paths on the daemon host are never accepted.
    #[serde(default)]
    pub wav_base64: Option<String>,
    /// The local backend's voice for `text`; the configured default when omitted.
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
    /// Make a request from a caller's WAV bytes, validating before encoding.
    /// The caller may then set `rooms` and `volume`.
    pub fn from_wav(wav: &[u8]) -> Result<Self, Failure> {
        validate_upload(wav)?;
        Ok(Self {
            wav_base64: Some(BASE64.encode(wav)),
            ..Self::default()
        })
    }

    /// The clip asked for, or why the request is unusable.
    pub fn source(&self) -> Result<ClipSource, Failure> {
        let text = self.text.as_deref().map(str::trim);
        let chime = self.chime.as_deref().map(str::trim);
        let wav = self.wav_base64.as_deref();
        if [text.is_some(), chime.is_some(), wav.is_some()]
            .into_iter()
            .filter(|present| *present)
            .count()
            != 1
        {
            return Err(Failure::invalid(
                "give exactly one of `text`, `chime`, or `wav_base64`",
            ));
        }
        if text.is_none() && self.voice.is_some() {
            return Err(Failure::invalid("`voice` is only valid with `text`"));
        }
        match (text, chime, wav) {
            (Some(text), None, None) => Ok(ClipSource::Tts {
                text: text.to_string(),
                voice: self.voice.as_deref().map(str::trim).map(str::to_string),
            }),
            (None, Some(name), None) => Ok(ClipSource::Chime {
                name: name.to_string(),
            }),
            (None, None, Some(encoded)) => {
                if encoded.len() > MAX_WAV_BASE64_BYTES {
                    return Err(Failure::invalid(ClipError::TooLarge.to_string()));
                }
                let bytes = BASE64
                    .decode(encoded)
                    .map_err(|_| Failure::invalid("`wav_base64` must be standard padded base64"))?;
                validate_upload(&bytes)?;
                Ok(ClipSource::Wav { bytes })
            }
            _ => unreachable!("one source was checked above"),
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

fn validate_upload(wav: &[u8]) -> Result<(), Failure> {
    if wav.len() > MAX_WAV_BYTES {
        return Err(Failure::invalid(ClipError::TooLarge.to_string()));
    }
    wav_info(wav).map(|_| ()).map_err(|e| {
        Failure::invalid(e.to_string())
            .with_hint("Upload a valid 16-bit PCM WAV, at most 16 MiB and five minutes.")
    })
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

    /// How many announcements are playing or waiting their turn: a daemon
    /// that is stopping lets them put the music back first.
    #[must_use]
    pub fn announcements_pending(&self) -> u64 {
        self.announcements
            .as_ref()
            .map_or(0, |with| with.turns.held().values().sum::<usize>() as u64)
    }

    /// Check listener readiness without generating a clip or reserving a turn.
    /// A due schedule may wait for startup without consuming its occurrence.
    pub(super) fn announcement_ready(&self) -> Result<(), Failure> {
        let with = self.announcements.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotImplemented,
                "announcements need the daemon's media listener",
            )
        })?;
        (with.base)()
            .ok_or_else(|| Failure::new(ErrorCode::NotReady, "the media listener is not up yet"))?;
        Ok(())
    }

    /// Reserve one ordinary announcement slot without synthesis or writes.
    pub(super) fn reserve_announcement(
        &self,
        client: &Client,
    ) -> Result<AnnouncementPermit, Failure> {
        self.announcement_ready()?;
        let with = self.announcements.as_ref().expect("readiness checked");
        with.turns.take(client).map(AnnouncementPermit)
    }

    /// Play the clip in the rooms `req` names (every room by default) for
    /// `client`, then put everything back. Blocks until done, including any
    /// wait for an earlier announcement in the same household.
    pub fn announce(&self, client: &Client, req: &AnnounceRequest) -> Result<AnnounceDto, Failure> {
        self.announce_admitted(client, req, None)
    }

    /// Shared delivery after optional scheduler admission. Authorization and
    /// volume policy are still checked for this run, under its creator.
    pub(super) fn announce_admitted(
        &self,
        client: &Client,
        req: &AnnounceRequest,
        permit: Option<AnnouncementPermit>,
    ) -> Result<AnnounceDto, Failure> {
        let (prepared, turn, job) = self.prepare_announcement(client, req, permit, TOOL)?;
        job.preparing();
        let result = self.deliver_announcement(client, prepared, &job.control);
        job.finish(&result, self.clock.now().timestamp());
        drop(turn);
        result
    }

    /// Admit both synchronous and background delivery through the same bounded
    /// capacity and resolve aliases once before the job can wait its turn.
    fn prepare_announcement(
        &self,
        client: &Client,
        req: &AnnounceRequest,
        permit: Option<AnnouncementPermit>,
        operation: &'static str,
    ) -> Result<(PreparedAnnouncement, Turn, Arc<jobs::Job>), Failure> {
        let guard = self.guard(client);
        if let Err(denied) = guard.authorize(operation, false) {
            self.record(
                client,
                operation.into(),
                format!("deny: {}", denied.detail),
                denied.detail.clone(),
                None,
            );
            return Err(denied);
        }
        let source = req.source()?;
        match &source {
            ClipSource::Tts { text, voice } => {
                fsonos_core::announce::clip::check_speech(text, voice.as_deref())
                    .map_err(|e| clip_failure(&e))?;
            }
            ClipSource::Chime { name } => {
                name.parse::<Chime>().map_err(|e| clip_failure(&e))?;
            }
            ClipSource::Wav { .. } => {}
        }
        let volume = req.volume()?;
        self.announcement_ready()?;
        let with = self.announcements.as_ref().expect("readiness checked");
        let turn = match permit {
            Some(AnnouncementPermit(turn)) => {
                if !Arc::ptr_eq(&turn.turns, &with.turns) || turn.client != client.key() {
                    return Err(Failure::new(
                        ErrorCode::Internal,
                        "announcement reservation belongs to another caller or surface",
                    ));
                }
                turn
            }
            None => with.turns.take(client)?,
        };
        let households = self.households()?;
        let aliases = self.aliases();
        let rooms = room_view(&households, aliases.as_ref(), client);
        let selected = targets(&rooms, &req.rooms).map_err(|f| self.explain(f))?;
        let players = selected
            .iter()
            .map(|target| {
                (
                    households
                        .iter()
                        .position(|household| std::ptr::eq(household, target.household))
                        .expect("resolved household"),
                    target.room.primary.clone(),
                )
            })
            .collect();
        let labels = fsonos_core::rooms::household_labels(&households);
        let named: Vec<String> = selected
            .iter()
            .map(|target| {
                let index = households
                    .iter()
                    .position(|household| std::ptr::eq(household, target.household))
                    .expect("resolved household");
                format!("{}@{}", target.room.name, labels[index])
            })
            .collect();
        let kind = match &source {
            ClipSource::Tts { .. } => "speech",
            ClipSource::Chime { .. } => "chime",
            ClipSource::Wav { .. } => "wav",
        };
        let job = with
            .jobs
            .insert(client, kind, named, self.clock.now().timestamp())?;
        Ok((
            PreparedAnnouncement {
                operation,
                source,
                volume,
                households,
                players,
            },
            turn,
            job,
        ))
    }

    #[allow(clippy::too_many_lines)] // Keep admission, delivery and completion in execution order.
    fn deliver_announcement(
        &self,
        client: &Client,
        prepared: PreparedAnnouncement,
        control: &AnnouncementControl,
    ) -> Result<AnnounceDto, Failure> {
        let PreparedAnnouncement {
            operation,
            source,
            volume,
            households,
            players,
        } = prepared;
        let with = self
            .announcements
            .as_ref()
            .expect("announcement was admitted");
        let targets = admitted_targets(&households, &players)?;
        if control.is_cancelled() {
            return Ok(cancelled_before_playback(&households, &targets));
        }
        // Recheck the creator's grant when the background worker begins.
        self.guard(client).authorize(operation, false)?;
        let base = (with.base)().ok_or_else(|| {
            Failure::new(ErrorCode::NotReady, "the media listener is unavailable")
        })?;
        if let Err(e) = with.media.prune(SystemTime::now()) {
            tracing::debug!(error = %e, "old clips not pruned");
        }
        let stored = match with.media.make_controlled(&source, control) {
            Ok(stored) => stored,
            Err(ClipError::Cancelled) => {
                return Ok(cancelled_before_playback(&households, &targets));
            }
            Err(error) => return Err(clip_failure(&error)),
        };
        if control.is_cancelled() {
            return Ok(cancelled_before_playback(&households, &targets));
        }
        // Local synthesis can be slow. A revoked grant cannot survive it and
        // reach a speaker merely because admission happened earlier.
        self.guard(client).authorize(operation, false)?;
        let clip = Clip {
            url: stored.url(&base),
            title: title(&source),
            duration: stored.duration,
        };
        // The announcer asks each room's cap once its household's turn has
        // come, just before it sets the levels: the moment to end the fades
        // there. A sleep timer that was fading sleeps again once the music is
        // back (see `announcing`). Read the policy and clock at that boundary:
        // a queued clip may have waited across quiet hours or a policy edit.
        let sleeping = std::sync::Mutex::new(Vec::new());
        let authorize = || {
            self.guard(client)
                .authorize(operation, false)
                .map_err(|error| error.to_string())
        };
        let cap = |room: &Room| {
            if let Some(receipt) = self.announcing(room) {
                sleeping
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(receipt);
            }
            self.policy()
                .max_volume(&room.name, client, self.clock.now())
        };
        let completed = |household: &HouseholdState, report: &HouseholdAnnouncement| {
            // Each household completes while its announcement turn is still
            // held. No later clip can start before its sleep recovery finishes.
            let finished = {
                let mut pending = sleeping.lock().unwrap_or_else(PoisonError::into_inner);
                let (finished, remaining): (Vec<_>, Vec<_>) = pending
                    .drain(..)
                    .partition(|receipt| household.player(receipt.coordinator()).is_some());
                *pending = remaining;
                finished
            };
            let restored: Vec<_> = finished
                .into_iter()
                .filter(|receipt| restored_for_sleep(report, receipt.coordinator()))
                .collect();
            self.sleep_again(&households, &restored);
        };
        let live = self.live();
        let report = with
            .announcer
            .announce_controlled(
                &*self.transport,
                &targets,
                &clip,
                volume,
                live.as_deref(),
                &AnnouncementCallbacks {
                    authorize: &authorize,
                    cap: &cap,
                    completed: &completed,
                },
                control,
            )
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

struct PreparedAnnouncement {
    operation: &'static str,
    source: ClipSource,
    volume: u8,
    households: Vec<HouseholdState>,
    players: Vec<(usize, PlayerId)>,
}

fn admitted_targets<'a>(
    households: &'a [HouseholdState],
    players: &[(usize, PlayerId)],
) -> Result<Vec<ControlTarget<'a>>, Failure> {
    players
        .iter()
        .map(|(index, id)| {
            let invalid = || {
                Failure::new(
                    ErrorCode::Internal,
                    "admitted announcement room is missing from its retained household",
                )
            };
            let household = households.get(*index).ok_or_else(invalid)?;
            let room = household
                .rooms
                .iter()
                .find(|room| room.primary == *id)
                .ok_or_else(invalid)?;
            Ok(ControlTarget {
                household,
                room,
                player: household.player(id).ok_or_else(invalid)?,
                coordinator: household.player(&room.coordinator).ok_or_else(invalid)?,
            })
        })
        .collect()
}

fn cancelled_before_playback(
    households: &[HouseholdState],
    targets: &[ControlTarget<'_>],
) -> AnnounceDto {
    let mut report = AnnounceReport {
        households: Vec::new(),
    };
    for household in households {
        let rooms: Vec<_> = targets
            .iter()
            .filter(|target| std::ptr::eq(target.household, household))
            .collect();
        if let Some(first) = rooms.first() {
            report.households.push(HouseholdAnnouncement {
                lead: first.room.primary.clone(),
                rooms: rooms
                    .iter()
                    .map(|target| target.room.name.clone())
                    .collect(),
                levels: Vec::new(),
                outcome: Ok(Ending::Cancelled),
                restored: Vec::new(),
                restore_failed: Vec::new(),
            });
        }
    }
    let mut dto = AnnounceDto::from_report(households, &report);
    dto.done = "announcement cancelled before playback; no speaker state was changed".into();
    dto
}

/// Only a fully restored original zone retains permission to resume its
/// interrupted sleep. No report, failed recovery or a preserved manual change
/// is evidence that the old timer should remain ended.
fn restored_for_sleep(report: &HouseholdAnnouncement, coordinator: &PlayerId) -> bool {
    report.restore_failed.is_empty()
        && report
            .restored
            .iter()
            .any(|(restored, result)| restored == coordinator && result.skipped.is_empty())
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
        ClipSource::Wav { .. } => "WAV announcement".to_string(),
    }
}

/// A clip that could not be made, as a failure.
fn clip_failure(e: &ClipError) -> Failure {
    match e {
        // Controlled delivery consumes this as a cancelled job before reaching
        // this conversion. Keep other callers exhaustive without suggesting a
        // retry of an announcement the caller explicitly stopped.
        ClipError::Cancelled => Failure::invalid(e.to_string()),
        ClipError::UnknownChime { .. } => {
            Failure::invalid(e.to_string()).with_suggestions(Chime::NAMES.to_vec())
        }
        ClipError::EmptySpeech | ClipError::SpeechTooLong(_) | ClipError::BadVoice(_) => {
            Failure::invalid(e.to_string())
        }
        ClipError::Unsupported(_) => Failure::new(ErrorCode::NotImplemented, e.to_string())
            .with_hint("Configure a local speech command on the daemon host, or upload a WAV."),
        ClipError::SpeechUnavailable(_) | ClipError::SpeechConfig(_) => {
            Failure::new(ErrorCode::NotReady, e.to_string()).with_hint(
                "Run fsonos doctor --only speech on the daemon host and check speech.toml.",
            )
        }
        ClipError::SpeechTimeout { .. } => Failure::new(ErrorCode::NotReady, e.to_string())
            .with_hint("Check the local speech engine and its configured synthesis timeout."),
        ClipError::Speech(_) | ClipError::NotWav(_) | ClipError::Io(_) | ClipError::TooLarge => {
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
                    Ok(Ending::Cancelled) => ("cancelled", None),
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
                (None, "cancelled") => write!(done, "; {} cancelled", h.lead),
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
mod policy_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::ErrorCode;

    #[test]
    fn retained_announcement_targets_use_exact_household_and_player_identity() {
        use fsonos_types::{Generation, Player};
        let id = PlayerId("RINCON_SELECTED".into());
        let other = PlayerId("RINCON_OTHER".into());
        let make = |id: &PlayerId, name: &str| {
            (
                Player {
                    id: id.clone(),
                    room_name: name.into(),
                    ip: "192.0.2.10".parse().unwrap(),
                    model: String::new(),
                    generation: Generation::S2,
                },
                Room {
                    name: name.into(),
                    primary: id.clone(),
                    players: vec![id.clone()],
                    missing: Vec::new(),
                    coordinator: id.clone(),
                },
            )
        };
        let (selected, selected_room) = make(&id, "Kitchen");
        let (shadow, shadow_room) = make(&other, &id.0);
        let house = HouseholdState {
            players: vec![selected, shadow],
            rooms: vec![selected_room, shadow_room],
            ..HouseholdState::default()
        };
        let mut first = house.clone();
        first.rooms[0].name = "Other household".into();
        let houses = [first, house];
        let targets = admitted_targets(&houses, &[(1, id.clone())]).unwrap();
        assert!(std::ptr::eq(
            targets[0].household,
            std::ptr::from_ref(&houses[1])
        ));
        assert_eq!(targets[0].room.name, "Kitchen");
        assert_eq!(targets[0].player.id, id);
        assert!(admitted_targets(&houses, &[(2, other)]).is_err());
    }

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
    fn wav_upload_round_trips_and_excludes_other_sources_and_voices() {
        let bytes = Chime::Beep.wav();
        let upload = AnnounceRequest::from_wav(&bytes).unwrap();
        let wire = serde_json::to_vec(&upload).unwrap();
        let request: AnnounceRequest = serde_json::from_slice(&wire).unwrap();
        assert_eq!(request.source().unwrap(), ClipSource::Wav { bytes });
        assert_eq!(title(&request.source().unwrap()), "WAV announcement");
        for bad in [
            AnnounceRequest {
                text: Some("hi".into()),
                ..upload.clone()
            },
            AnnounceRequest {
                chime: Some("bell".into()),
                ..upload.clone()
            },
            AnnounceRequest {
                voice: Some("en".into()),
                ..upload
            },
        ] {
            assert_eq!(bad.source().unwrap_err().code, ErrorCode::InvalidArgument);
        }
    }

    #[test]
    fn wav_upload_rejects_bad_encoding_format_and_size_before_playback() {
        for encoded in ["%%%".to_string(), String::new(), BASE64.encode(b"not WAV")] {
            let upload = AnnounceRequest {
                wav_base64: Some(encoded),
                ..AnnounceRequest::default()
            };
            assert_eq!(
                upload.source().unwrap_err().code,
                ErrorCode::InvalidArgument
            );
        }
        let upload = AnnounceRequest {
            // Deliberately invalid encoding: size must be checked before decoding.
            wav_base64: Some("!".repeat(MAX_WAV_BASE64_BYTES + 1)),
            ..AnnounceRequest::default()
        };
        assert_eq!(
            upload.source().unwrap_err().detail,
            ClipError::TooLarge.to_string()
        );
        assert_eq!(
            AnnounceRequest::from_wav(b"not WAV").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
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
        for body in [
            r#"{"file":"/private/clip.wav"}"#,
            r#"{"wav_path":"clip.wav"}"#,
        ] {
            assert!(serde_json::from_str::<AnnounceRequest>(body).is_err());
        }
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
        for error in [
            ClipError::SpeechUnavailable("no engine".into()),
            ClipError::SpeechConfig("bad speech.toml".into()),
            ClipError::SpeechTimeout {
                backend: "franken-tts".into(),
                seconds: 1,
            },
        ] {
            assert_eq!(clip_failure(&error).code, ErrorCode::NotReady);
        }
        // Backend-generated bad audio is a host failure; uploaded bad audio
        // is rejected as INVALID_ARGUMENT by source(), above.
        assert_eq!(
            clip_failure(&ClipError::NotWav("bad format")).code,
            ErrorCode::Internal
        );
    }

    #[test]
    fn each_client_and_the_daemon_have_a_bounded_number_of_turns() {
        let turns = Arc::new(Turns::default());
        let agent = Client::Tailnet("agent@example.com".into());
        let first = turns.take(&agent).unwrap();
        let second = turns.take(&agent).unwrap();
        let refused = turns.take(&agent).err().unwrap();
        assert_eq!(refused.code, ErrorCode::NotReady);
        assert!(refused.code.retryable());
        assert!(
            refused.detail.contains("agent@example.com"),
            "{}",
            refused.detail
        );
        // Another client still gets its own turns.
        let cli = turns.take(&Client::Cli).unwrap();
        // A turn comes back when its announcement is done.
        drop(first);
        let third = turns.take(&agent).unwrap();
        drop((second, third, cli));
        assert!(turns.held().is_empty());

        // Everyone together: MAX_PENDING, however many clients.
        let clients: Vec<Client> = (0..MAX_PENDING)
            .map(|n| Client::Tailnet(format!("agent{n}@example.com")))
            .collect();
        let all: Vec<Turn> = clients.iter().map(|c| turns.take(c).unwrap()).collect();
        let full = turns.take(&Client::Cli).err().unwrap();
        assert_eq!(full.code, ErrorCode::NotReady);
        assert!(
            full.detail.contains(&MAX_PENDING.to_string()),
            "{}",
            full.detail
        );
        drop(all);
        assert!(turns.take(&Client::Cli).is_ok());
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
