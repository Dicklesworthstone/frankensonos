//! The owner's feedback to the DJ (see `fsonos_api::surface::dj_feedback`):
//! likes and dislikes of the work playing, and the signals playback gives on
//! its own. A movement left within 30 s of starting is an early skip; a work
//! heard in order through every movement earns a full listen. fsonos-spotify's
//! feedback model weighs them all into the DJ's picks (the work, its composer
//! and its performer), decaying over weeks.

use fsonos_api::dj::DjSpeakers;
use fsonos_api::surface::dj_feedback::{DjFeedback, DjFeedbackDto, DjFeedbackRequest};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::Clock;
use fsonos_core::control;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::Client;
use fsonos_core::store::{Store, StoreError};
use fsonos_spotify::dj::WorkPool;
use fsonos_spotify::feed::QueueFeed;
use fsonos_spotify::feedback::{
    EARLY_SKIP_SECS, FeedbackModel, FeedbackSignal, Signal, StoreFeedback,
};
use fsonos_spotify::works::Work;
use fsonos_types::TransportState;
use std::path::PathBuf;
use std::time::Instant;

use super::{State, room};
use crate::config::GlobalArgs;
use crate::dj_view::store_failure;
use crate::remote::Daemon;

mod attribution;

/// The `spotify:track:<id>` a speaker's track URI plays, if it plays one.
#[must_use]
pub fn source_of(track_uri: &str) -> Option<String> {
    let path = track_uri
        .strip_prefix("x-sonos-spotify:")
        .unwrap_or(track_uri)
        .split('?')
        .next()?;
    let decoded = path.replace("%3a", ":").replace("%3A", ":");
    let id = decoded.strip_prefix("spotify:track:")?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .then(|| format!("spotify:track:{id}"))
}

/// A movement of one of the DJ's works, as it starts playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Movement {
    /// Its queue position.
    pub position: u32,
    pub work_key: String,
    /// It is the work's last movement.
    pub last: bool,
    pub duration_secs: Option<u32>,
    /// Exact ordered track identities distinguish recordings of one work.
    pub recording: Vec<String>,
    pub index: usize,
    /// An early skip requires observing this track near its start.
    pub started: bool,
    /// Attribution captured before a library reload can remove/change it.
    pub evidence: FeedbackSignal,
}

impl Movement {
    fn same_recording(&self, other: &Self) -> bool {
        self.work_key == other.work_key && self.recording == other.recording
    }

    fn same_track(&self, other: &Self) -> bool {
        self.same_recording(other) && self.index == other.index
    }

    fn mostly_heard(&self, heard: u64) -> bool {
        self.duration_secs
            .filter(|&seconds| seconds > 0)
            .is_some_and(|d| heard.saturating_mul(10) >= u64::from(d) * 9)
    }
}

/// One group's listening, for the signals playback gives on its own.
#[derive(Debug, Default)]
pub struct Listening {
    /// The movement and accumulated seconds observed in Playing state.
    current: Option<(Movement, u64)>,
    /// All earlier movements of this recording were observed in order.
    prefix_complete: bool,
    finished: Option<FeedbackSignal>,
    observed_at: Option<i64>,
    advancing: bool,
    /// Production observations use elapsed monotonic time, not the wall clock
    /// used to timestamp durable feedback.
    epoch: Option<Instant>,
}

impl Listening {
    /// Lost ownership or unreadable identity is not a listener's preference.
    fn interrupt(&mut self) {
        self.advancing = false;
        self.prefix_complete = false;
        self.finished = None;
        if let Some((movement, _)) = &mut self.current {
            movement.started = false;
        }
    }

    fn observe(
        &mut self,
        transport: Option<TransportState>,
        playing: Option<Movement>,
    ) -> Option<(String, Signal)> {
        let elapsed = self
            .epoch
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs();
        self.on_change(
            i64::try_from(elapsed).unwrap_or(i64::MAX),
            transport,
            playing,
        )
    }

    /// Fold playback at `now`, seconds on a monotonic observation clock.
    /// Paused, transitioning and unknown intervals do not count as listening.
    /// A terminal change returns the ended movement's work and its signal.
    pub fn on_change(
        &mut self,
        now: i64,
        transport: Option<TransportState>,
        playing: Option<Movement>,
    ) -> Option<(String, Signal)> {
        self.finished = None;
        if self.observed_at.is_some_and(|at| now < at) {
            return None;
        }
        let elapsed = self
            .observed_at
            .replace(now)
            .map_or(0, |at| now.abs_diff(at));
        if self.advancing
            && let Some((_, heard)) = &mut self.current
        {
            *heard = heard.saturating_add(elapsed);
        }
        self.advancing = transport == Some(TransportState::Playing);
        let playing = match transport {
            Some(TransportState::Playing) => playing,
            Some(TransportState::Stopped) => None,
            _ => return None,
        };
        if let (Some((heard, _)), Some(next)) = (&mut self.current, &playing)
            && heard.same_track(next)
        {
            if heard.position != next.position {
                // A trim, queue edit or replay may reuse the same URI at a
                // different slot. Do not manufacture an early skip, and do
                // not infer a complete traversal across this discontinuity.
                self.interrupt();
                let mut next = next.clone();
                next.started = false;
                self.current = Some((next, 0));
            } else {
                heard.duration_secs = next.duration_secs.or(heard.duration_secs);
            }
            return None;
        }
        self.finish(playing)
    }

    fn finish(&mut self, playing: Option<Movement>) -> Option<(String, Signal)> {
        let previous = self.current.take();
        let prefix = self.prefix_complete;
        self.prefix_complete = playing.as_ref().is_some_and(|next| {
            next.index == 0
                || previous.as_ref().is_some_and(|(old, heard)| {
                    prefix
                        && old.mostly_heard(*heard)
                        && old.same_recording(next)
                        && old.index.checked_add(1) == Some(next.index)
                })
        });
        self.current = playing.map(|movement| (movement, 0));
        let (movement, heard) = previous?;
        let most = movement.mostly_heard(heard);
        let signal = if most && movement.last && prefix {
            Signal::FullListen
        } else if !most && movement.started && heard < EARLY_SKIP_SECS.unsigned_abs() {
            Signal::EarlySkip
        } else {
            return None;
        };
        let mut evidence = movement.evidence;
        evidence.signal = signal;
        self.finished = Some(evidence);
        Some((movement.work_key, signal))
    }
}

/// Record `signal` about `work` (its composer and performer too) at `now`.
fn record(store: &mut dyn Store, work: &Work, signal: Signal, now: i64) -> Result<(), StoreError> {
    store.record_feedback(&FeedbackSignal::about(work, signal, now).to_store())
}

/// Fold `playback` into the listening of the group `feed` plays in, and
/// record the signal a movement's end gives; whether one was recorded.
pub(super) fn implicit(
    listening: &mut Listening,
    feed: &QueueFeed,
    pool: &WorkPool,
    store: &mut dyn Store,
    playback: &PlayerPlayback,
    now: i64,
) -> bool {
    if !feed.is_active() {
        listening.interrupt();
        return false;
    }
    let playing = attribution::movement(
        feed.queued(),
        pool,
        playback,
        listening.current.as_ref().map(|(movement, _)| movement),
    );
    if playback.transport == Some(TransportState::Playing) && playing.is_none() {
        listening.interrupt();
        return false;
    }
    if playback.transport == Some(TransportState::Stopped)
        && let Some(uri) = playback.track_uri.as_deref().filter(|uri| !uri.is_empty())
        && let Some((previous, _)) = &listening.current
        && source_of(uri).as_ref() != previous.recording.get(previous.index)
    {
        listening.interrupt();
        return false;
    }
    let _ = listening.observe(playback.transport, playing);
    let Some(mut evidence) = listening.finished.take() else {
        return false;
    };
    evidence.at = now;
    match store.record_feedback(&evidence.to_store()) {
        Ok(()) => {
            tracing::info!(
                work = ?evidence.work_key,
                signal = ?evidence.signal,
                "the DJ noted how a recording was heard"
            );
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "listening feedback not recorded");
            false
        }
    }
}

/// An explicit like or dislike of the work playing in `at`'s group: the
/// library's work of the track playing there, whether the DJ picked it or
/// not.
pub(super) fn explicit(
    state: &mut State,
    moods_file: Option<&PathBuf>,
    at: DjSpeakers<'_>,
    store: &mut dyn Store,
    feedback: DjFeedback,
    clock: &dyn Clock,
) -> Result<DjFeedbackDto, Failure> {
    let now = clock.now().timestamp();
    if state.pool.is_empty() {
        state.reload(store, moods_file, now)?;
    }
    let room = room(at);
    let playback = control::playback(at.transport, at.households, at.coordinator)?;
    let work = source_of(&playback.position.uri)
        .and_then(|source| state.pool.work_of(&source))
        .ok_or_else(|| {
            Failure::new(
                ErrorCode::NoMatch,
                format!("nothing from your library plays in {room}'s group"),
            )
            .with_hint("Like or dislike while the DJ, or a work from your library, plays there.")
        })?
        .clone();
    let signal = match feedback {
        DjFeedback::Like => Signal::Like,
        DjFeedback::Dislike => Signal::Dislike,
    };
    record(store, &work, signal, now).map_err(store_failure)?;
    // From the DJ's next pick on.
    state.feedback = FeedbackModel::load(&StoreFeedback(&*store), now).ok();
    let name = format!("{}: {}", work.composer, work.title);
    // What the signal also reaches: a classical work's composer and
    // performer, a song's artist and album (`FeedbackSignal::about`).
    let also = if work.is_classical() {
        "its composer and its performer"
    } else {
        "its artist and its album"
    };
    let done = match feedback {
        DjFeedback::Like => format!("noted that you like {name}: the DJ favors it, {also}"),
        DjFeedback::Dislike => format!(
            "noted that you dislike {name}: the DJ plays it, {also} less (a second dislike \
             keeps the work out for 180 days)"
        ),
    };
    Ok(DjFeedbackDto {
        zone: room,
        work: name,
        composer: work.composer.clone(),
        signal: feedback.as_str().to_string(),
        done,
    })
}

/// `fsonos dj like|dislike [room]`, through the same surface as the HTTP API
/// and the MCP tools (as the policy's `cli` client): through the daemon when
/// one answers, so its DJ weighs the feedback from its next pick (and knows
/// the group it plays in), else right here.
pub fn run(global: &GlobalArgs, zone: Option<&str>, feedback: DjFeedback) -> anyhow::Result<()> {
    let req = DjFeedbackRequest {
        zone: zone.map(str::to_string),
        signal: feedback.as_str().to_string(),
    };
    let text = |g: &DjFeedbackDto| format!("{}\n", g.done);
    if let Some(daemon) = Daemon::find(global, global.daemon)? {
        let given: DjFeedbackDto = daemon.post("/dj/feedback", &req)?;
        return crate::emit(global.json, &given, text);
    }
    let dir = crate::daemon::data_dir(global)?;
    let surface = crate::daemon::surface(global, crate::daemon::policy(&dir)?)?;
    let surface = crate::daemon::with_action_log(surface, &dir, "cli");
    let given = surface.dj_feedback(&Client::Cli, &req)?;
    crate::emit(global.json, &given, text)
}

#[cfg(test)]
mod tests {
    mod attribution;
    mod listening;

    use super::*;

    const PLAYING: Option<TransportState> = Some(TransportState::Playing);

    fn movement(position: u32, work: &str, last: bool) -> Movement {
        Movement {
            position,
            work_key: work.into(),
            last,
            duration_secs: Some(180),
            recording: vec![format!("spotify:track:{work}{position}")],
            index: 0,
            started: true,
            evidence: FeedbackSignal {
                at: 0,
                work_key: Some(work.into()),
                composer_key: None,
                performer: None,
                artist_key: None,
                album_key: None,
                genres: Vec::new(),
                signal: Signal::FullListen,
            },
        }
    }

    #[test]
    fn the_track_a_speaker_plays_names_its_library_source() {
        assert_eq!(
            source_of("x-sonos-spotify:spotify%3atrack%3aAbC123xyz?sid=9&flags=8224&sn=1")
                .as_deref(),
            Some("spotify:track:AbC123xyz")
        );
        assert_eq!(
            source_of("x-sonos-spotify:spotify%3Atrack%3AAbC123xyz?sid=9").as_deref(),
            Some("spotify:track:AbC123xyz")
        );
        assert_eq!(
            source_of("spotify:track:AbC123xyz").as_deref(),
            Some("spotify:track:AbC123xyz")
        );
        assert_eq!(
            source_of("x-rincon-mp3radio://stream.example.invalid/a.mp3"),
            None
        );
        assert_eq!(source_of("x-sonos-spotify:spotify%3atrack%3a?sid=9"), None);
    }

    #[test]
    fn a_movement_left_within_30_s_is_an_early_skip() {
        let mut l = Listening::default();
        assert_eq!(
            l.on_change(1000, PLAYING, Some(movement(1, "a", false))),
            None
        );
        // The same movement again (a volume change, say) changes nothing.
        assert_eq!(
            l.on_change(1010, PLAYING, Some(movement(1, "a", false))),
            None
        );
        assert_eq!(
            l.on_change(1020, PLAYING, Some(movement(3, "b", false))),
            Some(("a".into(), Signal::EarlySkip))
        );
        // Left after a minute: no signal either way.
        assert_eq!(
            l.on_change(1080, PLAYING, Some(movement(4, "b", true))),
            None
        );
    }

    #[test]
    fn a_work_heard_to_the_end_of_its_last_movement_is_a_full_listen() {
        let mut l = Listening::default();
        assert_eq!(l.on_change(0, PLAYING, Some(movement(1, "a", false))), None);
        // The first movement ends on its own: not the whole work yet.
        assert_eq!(
            l.on_change(180, PLAYING, Some(movement(2, "a", true))),
            None
        );
        // A pause holds it, and so does a transition.
        assert_eq!(l.on_change(200, Some(TransportState::Paused), None), None);
        assert_eq!(
            l.on_change(250, Some(TransportState::Transitioning), None),
            None
        );
        assert_eq!(
            l.on_change(250, PLAYING, Some(movement(2, "a", true))),
            None
        );
        assert_eq!(
            l.on_change(412, PLAYING, Some(movement(3, "b", false))),
            Some(("a".into(), Signal::FullListen))
        );
        // The queue runs out: the last movement ended on its own.
        assert_eq!(l.on_change(592, Some(TransportState::Stopped), None), None);
        let mut end = Listening::default();
        end.on_change(0, PLAYING, Some(movement(5, "c", true)));
        assert_eq!(
            end.on_change(181, Some(TransportState::Stopped), None),
            Some(("c".into(), Signal::FullListen))
        );
    }

    #[test]
    fn a_stop_soon_after_starting_is_an_early_skip_and_nothing_of_the_djs_is_quiet() {
        let mut l = Listening::default();
        l.on_change(0, PLAYING, Some(movement(1, "a", true)));
        assert_eq!(
            l.on_change(12, Some(TransportState::Stopped), None),
            Some(("a".into(), Signal::EarlySkip))
        );
        // Something that isn't the DJ's plays: nothing to note.
        assert_eq!(l.on_change(20, PLAYING, None), None);
        assert_eq!(l.on_change(25, PLAYING, None), None);
        // Without a known length, a minute in proves neither a skip nor completion.
        let mut unknown = Listening::default();
        unknown.on_change(
            0,
            PLAYING,
            Some(Movement {
                duration_secs: None,
                ..movement(1, "d", true)
            }),
        );
        assert_eq!(
            unknown.on_change(60, PLAYING, Some(movement(2, "e", false))),
            None
        );
    }
}
