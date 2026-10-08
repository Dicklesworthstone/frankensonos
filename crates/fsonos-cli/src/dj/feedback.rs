//! The owner's feedback to the DJ (see `fsonos_api::surface::dj_feedback`):
//! likes and dislikes of the work playing, and the signals playback gives on
//! its own. A movement left within 30 s of starting is an early skip; a work
//! heard to the end of its last movement is a full listen. fsonos-spotify's
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
    EARLY_SKIP_SECS, FeedbackModel, FeedbackSignal, Signal, StoreFeedback, listen_signal,
};
use fsonos_spotify::works::Work;
use fsonos_types::TransportState;
use std::path::PathBuf;

use super::{State, room};
use crate::config::GlobalArgs;
use crate::dj_view::store_failure;

/// How far short of its length a movement may end and still count as heard
/// to the end (events arrive a moment late).
const END_SLACK_SECS: i64 = 5;

/// The `spotify:track:<id>` a speaker's track URI plays, if it plays one.
#[must_use]
pub fn source_of(track_uri: &str) -> Option<String> {
    let lower = track_uri.to_ascii_lowercase();
    let start = ["spotify%3atrack%3a", "spotify:track:"]
        .iter()
        .find_map(|marker| lower.find(marker).map(|i| i + marker.len()))?;
    let id: String = track_uri[start..]
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    (!id.is_empty()).then(|| format!("spotify:track:{id}"))
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
}

/// One group's listening, for the signals playback gives on its own.
#[derive(Debug, Default)]
pub struct Listening {
    /// The movement playing, and when it started (unix seconds).
    current: Option<(Movement, i64)>,
}

impl Listening {
    /// Fold the group's playback at `now` (unix seconds): `playing` is the
    /// DJ's movement now playing under `transport` (`None`: nothing of the
    /// DJ's). When the movement heard until now ended with a signal, the
    /// work it belongs to and that signal.
    pub fn on_change(
        &mut self,
        now: i64,
        transport: Option<TransportState>,
        playing: Option<Movement>,
    ) -> Option<(String, Signal)> {
        // A pause holds the movement; a transition settles in a moment.
        let playing = match transport {
            Some(TransportState::Playing) => playing,
            Some(TransportState::Stopped) => None,
            _ => return None,
        };
        if let (Some((heard, _)), Some(next)) = (&self.current, &playing)
            && heard.position == next.position
            && heard.work_key == next.work_key
        {
            return None;
        }
        let (movement, started) = std::mem::replace(&mut self.current, playing.map(|m| (m, now)))?;
        let heard = now - started;
        let skipped = movement.duration_secs.map_or(heard < EARLY_SKIP_SECS, |d| {
            heard + END_SLACK_SECS < i64::from(d)
        });
        match listen_signal(started, now, movement.duration_secs, skipped)? {
            // A whole work heard: its last movement ended.
            Signal::FullListen if !movement.last => None,
            signal => Some((movement.work_key, signal)),
        }
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
    let playing = playback.queue_position.and_then(|position| {
        let work = feed.queued().iter().find(|w| w.contains(position))?;
        Some(Movement {
            position,
            work_key: work.work_key.clone(),
            last: work.movements.last().and_then(|m| m.position) == Some(position),
            duration_secs: playback.duration_secs,
        })
    });
    let Some((key, signal)) = listening.on_change(now, playback.transport, playing) else {
        return false;
    };
    let Some(work) = pool.works().iter().find(|w| w.work_key == key) else {
        return false;
    };
    match record(store, work, signal, now) {
        Ok(()) => {
            tracing::info!(work = %work.title, ?signal, "the DJ noted how a work was heard");
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
    let done = match feedback {
        DjFeedback::Like => {
            format!("noted that you like {name}: the DJ favors it, its composer and its performer")
        }
        DjFeedback::Dislike => format!(
            "noted that you dislike {name}: the DJ plays it, its composer and its performer \
             less (a second dislike keeps the work out for 180 days)"
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
/// and the MCP tools (as the policy's `cli` client).
pub fn run(global: &GlobalArgs, zone: Option<&str>, feedback: DjFeedback) -> anyhow::Result<()> {
    let dir = crate::daemon::data_dir(global)?;
    let surface = crate::daemon::surface(global, crate::daemon::policy(&dir)?)?;
    let surface = crate::daemon::with_action_log(surface, &dir, "cli");
    let req = DjFeedbackRequest {
        zone: zone.map(str::to_string),
        signal: feedback.as_str().to_string(),
    };
    let given = surface.dj_feedback(&Client::Cli, &req)?;
    crate::emit(global.json, &given, |g: &DjFeedbackDto| {
        format!("{}\n", g.done)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYING: Option<TransportState> = Some(TransportState::Playing);

    fn movement(position: u32, work: &str, last: bool) -> Movement {
        Movement {
            position,
            work_key: work.into(),
            last,
            duration_secs: Some(180),
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
            l.on_change(362, PLAYING, Some(movement(3, "b", false))),
            Some(("a".into(), Signal::FullListen))
        );
        // The queue runs out: the last movement ended on its own.
        assert_eq!(l.on_change(542, Some(TransportState::Stopped), None), None);
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
        // Without a known length, a minute in is not a skip.
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
            Some(("d".into(), Signal::FullListen))
        );
    }
}
