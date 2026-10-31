//! Durable announcement recipes use the immediate delivery path at each run.

use super::AnnounceRequest;
use super::{Failure, ScheduleAction, dispatch, plan, zone_name};
use fsonos_core::announce::clip::{Chime, ClipSource, check_speech};
use fsonos_core::schedule::ScheduledAnnouncementSource;

/// Validate and normalize without synthesis, media files or speaker calls.
pub(super) fn action(req: &AnnounceRequest) -> Result<ScheduleAction, Failure> {
    if req.wav_base64.is_some() {
        return Err(Failure::invalid(
            "scheduled announcements take text or a chime, not wav_base64",
        ));
    }
    let invalid =
        |error: fsonos_core::announce::clip::ClipError| Failure::invalid(error.to_string());
    let source = match req.source()? {
        ClipSource::Tts { text, voice } => {
            check_speech(&text, voice.as_deref()).map_err(invalid)?;
            ScheduledAnnouncementSource::Speech { text, voice }
        }
        ClipSource::Chime { name } => ScheduledAnnouncementSource::Chime {
            name: name.parse::<Chime>().map_err(invalid)?.name().into(),
        },
        ClipSource::Wav { .. } => unreachable!("scheduled uploads were refused above"),
    };
    let level = req.volume()?;
    let rooms = req
        .rooms
        .iter()
        .map(|name| zone_name("rooms", name).map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ScheduleAction::Announce {
        source,
        rooms,
        volume: req.volume.map(|_| level),
    })
}

pub(super) fn request(
    source: &ScheduledAnnouncementSource,
    rooms: &[String],
    volume: Option<u8>,
) -> AnnounceRequest {
    let mut request = AnnounceRequest {
        rooms: rooms.to_vec(),
        volume: volume.map(i64::from),
        ..AnnounceRequest::default()
    };
    match source {
        ScheduledAnnouncementSource::Speech { text, voice } => {
            request.text = Some(text.clone());
            request.voice.clone_from(voice);
        }
        ScheduledAnnouncementSource::Chime { name } => request.chime = Some(name.clone()),
    }
    request
}

/// A partial alias must not consume a run that targets another missing room.
/// Empty rooms, like an immediate announcement, means every discovered room.
pub(super) fn targets_ready(rooms: &plan::Rooms<'_>, names: &[String]) -> Result<(), Failure> {
    if names.is_empty() {
        return dispatch::targets_ready(rooms, "all", false);
    }
    for name in names {
        dispatch::targets_ready(rooms, name, false)?;
    }
    Ok(())
}

pub(super) fn summary(
    source: &ScheduledAnnouncementSource,
    rooms: &[String],
    volume: Option<u8>,
) -> String {
    let clip = match source {
        ScheduledAnnouncementSource::Speech { text, .. } => format!("say {text:?}"),
        ScheduledAnnouncementSource::Chime { name } => format!("play the {name} chime"),
    };
    let rooms = if rooms.is_empty() {
        "every room".into()
    } else {
        rooms.join(", ")
    };
    let mut summary = format!("{clip} in {rooms}");
    if let Some(volume) = volume {
        use std::fmt::Write as _;
        let _ = write!(summary, " at volume {volume}");
    }
    summary
}

#[cfg(test)]
mod tests;
