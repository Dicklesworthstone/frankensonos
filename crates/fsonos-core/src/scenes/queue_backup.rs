//! Durable scene queues: stage a complete suffix before removing the original
//! prefix under its native revision. No uncertain mutation is ever replayed.

use super::queue::{self, Group, Playback, QueueReference};
use crate::snapshot::{self, ZoneSnapshot};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, BrowseFlag, BrowseSnapshot, QUEUE};
use fsonos_proto::didl::{
    DidlKind, DidlObject, SpotifyRenderParams, item_metadata, parse_didl, spotify_track_didl,
    spotify_uri_from_renderer_uri,
};
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::Instant;

mod staging;

/// Maximum number of occurrences in one durable scene queue.
pub const MAX_QUEUE_BACKUP_ENTRIES: usize = 1_000;
/// Aggregate UTF-8 bytes of the saved URIs and DIDL-Lite metadata per queue.
pub const MAX_QUEUE_BACKUP_BYTES: usize = 4 * 1024 * 1024;

/// The complete ordered queue and its selected occurrence. The original
/// coordinator and household remain its destination after edits or reboots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueBackup {
    pub reference: QueueReference,
    pub entries: Vec<QueueBackupEntry>,
}

/// One occurrence, including renderer metadata needed to enqueue it again.
/// Repeated URIs remain distinct entries; a backup never deduplicates them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueBackupEntry {
    pub uri: String,
    pub metadata: String,
}

fn invalid(detail: impl std::fmt::Display) -> ProtoError {
    ProtoError::Malformed(format!("saved queue reconstruction refused: {detail}"))
}

fn playable(uri: &str) -> bool {
    !uri.trim().is_empty()
        && uri != "NOT_IMPLEMENTED"
        && !uri.chars().any(char::is_control)
        && !["x-rincon:", "x-rincon-queue:", "x-rincon-cpcontainer:"]
            .iter()
            .any(|prefix| uri.starts_with(prefix))
        && spotify_uri_from_renderer_uri(uri).is_none_or(|source| {
            source.starts_with("spotify:track:") || source.starts_with("spotify:episode:")
        })
}

impl QueueBackup {
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.reference.validate()?;
        let selected = usize::try_from(self.reference.track).unwrap_or(usize::MAX);
        if self.entries.is_empty()
            || self.entries.len() > MAX_QUEUE_BACKUP_ENTRIES
            || selected > self.entries.len()
        {
            return Err(format!(
                "a queue backup needs 1–{MAX_QUEUE_BACKUP_ENTRIES} occurrences and a selected slot within them"
            ));
        }
        let bytes = self.entries.iter().try_fold(0_usize, |total, entry| {
            total
                .checked_add(entry.uri.len())?
                .checked_add(entry.metadata.len())
        });
        if bytes.is_none_or(|bytes| bytes > MAX_QUEUE_BACKUP_BYTES) {
            return Err(format!(
                "a queue backup exceeds its {MAX_QUEUE_BACKUP_BYTES}-byte limit"
            ));
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let object = entry
                .object()
                .map_err(|error| format!("queue occurrence {}: {error}", index + 1))?;
            if !playable(&entry.uri)
                || object.kind != DidlKind::Item
                || !object.class.starts_with("object.item.audioItem")
                || object.res.as_ref().is_none_or(|res| {
                    !playable(&res.uri) || !queue::same_track(&entry.uri, &res.uri)
                })
            {
                return Err(format!(
                    "queue occurrence {} has no complete playable resource",
                    index + 1
                ));
            }
            if spotify_uri_from_renderer_uri(&entry.uri)
                .is_some_and(|uri| uri.starts_with("spotify:track:"))
                && object
                    .desc
                    .as_ref()
                    .is_none_or(|desc| desc.id != "cdudn" || desc.value.trim().is_empty())
            {
                return Err(format!(
                    "queue occurrence {} is missing its Spotify account descriptor",
                    index + 1
                ));
            }
        }
        Ok(())
    }
}

impl QueueBackupEntry {
    fn object(&self) -> Result<DidlObject, ProtoError> {
        let mut objects = parse_didl(&self.metadata)?;
        if objects.len() != 1 {
            return Err(invalid(
                "each saved occurrence needs exactly one metadata item",
            ));
        }
        Ok(objects.remove(0))
    }
}

/// Queue-assigned item IDs and artwork URLs may change on reconstruction.
/// Compare ordered playable identity and the identifying display metadata;
/// service descriptors are sent as saved but not consistently returned by
/// ContentDirectory on either generation of player.
fn same_item(expected: &DidlObject, actual: &DidlObject) -> bool {
    expected.kind == actual.kind
        && expected.class == actual.class
        && expected.title == actual.title
        && expected.creator == actual.creator
        && expected.album == actual.album
        && match (&expected.res, &actual.res) {
            (Some(a), Some(b)) => queue::same_track(&a.uri, &b.uri),
            _ => false,
        }
}

fn same_items(expected: &[DidlObject], actual: &[DidlObject]) -> bool {
    expected.len() == actual.len() && expected.iter().zip(actual).all(|(a, b)| same_item(a, b))
}

/// Read at most the capture limit, refusing a partial, changed or oversized
/// container before constructing a scene that could later be persisted.
pub(crate) fn bounded_snapshot<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
) -> Result<BrowseSnapshot, ProtoError> {
    let limit = u32::try_from(MAX_QUEUE_BACKUP_ENTRIES).expect("the queue backup limit fits u32");
    let before = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if before.total_matches > limit {
        return Err(invalid(format!(
            "the queue exceeds {MAX_QUEUE_BACKUP_ENTRIES} occurrences"
        )));
    }
    let snapshot = content::browse_range_snapshot(t, host, QUEUE, 0, limit)?;
    let after = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if before.update_id != snapshot.update_id
        || after.update_id != snapshot.update_id
        || before.total_matches != after.total_matches
        || usize::try_from(after.total_matches).ok() != Some(snapshot.objects.len())
    {
        return Err(invalid(
            "the queue changed or its complete contents could not be read",
        ));
    }
    Ok(snapshot)
}

fn replay_item<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    mut object: DidlObject,
    parameters: &mut Option<SpotifyRenderParams>,
) -> Result<QueueBackupEntry, CoreError> {
    let uri = object
        .res
        .as_ref()
        .map(|res| res.uri.clone())
        .ok_or_else(|| invalid("a queue occurrence has no resource"))?;
    if object.kind != DidlKind::Item
        || !object.class.starts_with("object.item.audioItem")
        || !playable(&uri)
    {
        return Err(invalid("a queue occurrence is not an expanded playable audio item").into());
    }
    // Queue browse commonly omits the descriptor that AddURIToQueue
    // requires. Resolve that missing replay data from this household's
    // linked account before saving anything, never from another house.
    if let Some(spotify) = spotify_uri_from_renderer_uri(&uri)
        && spotify.starts_with("spotify:track:")
    {
        if parameters.is_none() {
            *parameters = Some(
                control::spotify_params(t, households, coordinator)?.ok_or_else(|| {
                    invalid("Spotify queue entries have no discoverable account metadata")
                })?,
            );
        }
        let metadata = spotify_track_didl(
            &spotify,
            &object.title,
            parameters.as_ref().expect("parameters resolved above"),
        );
        let replay = parse_didl(&metadata)?
            .into_iter()
            .next()
            .ok_or_else(|| invalid("Spotify replay metadata is missing"))?;
        let params = parameters.as_ref().expect("parameters resolved above");
        let same_account = uri.split_once('?').is_none_or(|(_, query)| {
            let number = |name: &str| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
                    .and_then(|value| value.parse::<u32>().ok())
            };
            number("sid") == Some(params.sid) && number("sn") == Some(params.sn)
        });
        if !same_account
            || object
                .desc
                .as_ref()
                .is_some_and(|desc| replay.desc.as_ref() != Some(desc))
        {
            return Err(invalid(
                "a Spotify queue occurrence belongs to an unresolved linked account",
            )
            .into());
        }
        // Browse assigns Q:0/n IDs. Enqueue needs the renderer's service
        // item prefix, while resource/display metadata remains intact.
        object.id = replay.id;
        object.parent_id = replay.parent_id;
        object.desc = replay.desc;
    }
    Ok(QueueBackupEntry {
        uri,
        metadata: item_metadata(&object),
    })
}

pub(crate) fn capture<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    snap: &ZoneSnapshot,
    before_boot: Option<u32>,
) -> Result<Option<QueueBackup>, CoreError> {
    let started = Instant::now();
    let Some(original) = queue::capture(t, households, snap, before_boot)? else {
        return Ok(None);
    };
    let host = control::locate(households, &snap.coordinator)?.ip;
    let contents = bounded_snapshot(t, host)?;
    if contents.update_id != original.update_id {
        return Err(invalid("the queue changed while its backup was read").into());
    }
    let mut parameters = None;
    let mut entries = Vec::with_capacity(contents.objects.len());
    let mut bytes = 0_usize;
    for object in contents.objects {
        let entry = replay_item(t, households, &snap.coordinator, object, &mut parameters)?;
        bytes = bytes
            .checked_add(entry.uri.len())
            .and_then(|size| size.checked_add(entry.metadata.len()))
            .filter(|size| *size <= MAX_QUEUE_BACKUP_BYTES)
            .ok_or_else(|| {
                invalid(format!(
                    "the queue exceeds its {MAX_QUEUE_BACKUP_BYTES}-byte backup limit"
                ))
            })?;
        entries.push(entry);
    }
    // Re-snapshot the current clock after a long browse. A naturally advancing
    // playing item is valid; a new occurrence, seek, mode or transport is not.
    let after = snapshot::capture(t, households, &snap.coordinator, snap.captured_at)?;
    let Some(reference) = queue::capture(t, households, &after, before_boot)? else {
        return Err(invalid("the queue became empty while saving").into());
    };
    let advance = if snap.transport_state == TransportState::Playing {
        u32::try_from(started.elapsed().as_secs())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    } else {
        0
    };
    if reference.update_id != original.update_id
        || reference.track != original.track
        || reference.play_mode != original.play_mode
        || after.transport_state != snap.transport_state
        || reference.position_secs < original.position_secs
        || reference.position_secs > original.position_secs.saturating_add(advance)
    {
        return Err(invalid("queue playback changed while saving its contents").into());
    }
    let backup = QueueBackup { reference, entries };
    backup.validate().map_err(invalid)?;
    Ok(Some(backup))
}

pub(crate) struct Owner {
    pub(crate) group: Group,
    pub(crate) playback: Playback,
    pub(crate) mode: Option<PlayMode>,
}

impl Owner {
    pub(crate) fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        mode_known: bool,
    ) -> Result<Self, ProtoError> {
        let group = queue::group(t, host, coordinator)?;
        let playback = Playback::read(t, host)?;
        let mode = mode_known.then(|| get_play_mode(t, host)).transpose()?;
        if queue::group(t, host, coordinator)? != group {
            return Err(invalid("the group changed during observation"));
        }
        Ok(Self {
            group,
            playback,
            mode,
        })
    }

    pub(crate) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), ProtoError> {
        let now = Self::read(t, host, coordinator, self.mode.is_some())?;
        if self.group != now.group || self.mode != now.mode || !self.playback.agrees(&now.playback)
        {
            return Err(invalid(
                "the group, boot, source, selection, time, transport or mode changed during reconstruction",
            ));
        }
        Ok(())
    }

    fn after_removal<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        removed: usize,
        remaining: &[DidlObject],
    ) -> Result<Self, ProtoError> {
        let now = Self::read(t, host, coordinator, self.mode.is_some())?;
        if self.group != now.group
            || self.mode != now.mode
            || self.playback.media.uri != now.playback.media.uri
            || self.playback.media.uri_metadata != now.playback.media.uri_metadata
            || !matches!(
                now.playback.state,
                TransportState::Paused | TransportState::Stopped
            )
        {
            return Err(invalid(
                "playback ownership changed during the queue commit",
            ));
        }
        let own_queue = format!("x-rincon-queue:{}#0", coordinator.0);
        if removed == 0 || self.playback.media.uri != own_queue {
            // Editing an inactive queue cannot move the current file's clock
            // or selection. Do not adopt another controller's seek or Stop.
            if !self.playback.agrees(&now.playback) {
                return Err(invalid(
                    "playback changed while committing an inactive queue",
                ));
            }
        } else {
            let before = usize::try_from(self.playback.position.track).map_err(invalid)?;
            let selected = if before == 0 {
                0
            } else {
                before.saturating_sub(removed).max(1)
            };
            let matches_item = selected
                .checked_sub(1)
                .and_then(|at| remaining.get(at))
                .and_then(|item| item.res.as_ref())
                .is_some_and(|res| queue::same_track(&res.uri, &now.playback.position.uri));
            let time_matches = self.playback.time_agrees(&now.playback)
                || (before > 0
                    && before <= removed
                    && now.playback.position.position_secs == Some(0));
            if usize::try_from(now.playback.position.track).ok() != Some(selected)
                || (selected > 0 && !matches_item)
                || (selected == 0 && !now.playback.position.uri.is_empty())
                || !time_matches
            {
                return Err(invalid(
                    "the selected occurrence or time did not match native prefix removal",
                ));
            }
        }
        Ok(now)
    }
}

pub(super) fn restore<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    backup: &QueueBackup,
    start: bool,
) -> Result<bool, String> {
    restore_bound(t, households, coordinator, backup, start, None)
}

/// Undo carries its authorized post-action observation into staging. A fresh
/// read may confirm that observation, but must never replace its ownership.
pub(crate) struct Baseline<'a> {
    pub(crate) owner: &'a Owner,
    pub(crate) revision: u32,
    pub(crate) uris: &'a [String],
}

pub(crate) fn restore_bound<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    backup: &QueueBackup,
    start: bool,
    baseline: Option<&Baseline<'_>>,
) -> Result<bool, String> {
    backup.validate()?;
    if coordinator != &backup.reference.coordinator {
        return Err(
            "the saved queue's original coordinator differs from this room's player".into(),
        );
    }
    let house = households
        .iter()
        .find(|house| house.player(coordinator).is_some())
        .ok_or_else(|| "the original queue coordinator is missing".to_string())?;
    if backup
        .reference
        .household
        .as_ref()
        .is_some_and(|saved| house.id.as_ref() != Some(saved))
    {
        return Err("the saved queue belongs to a different household".into());
    }
    let mut attempted = false;
    let result = staging::restore(
        t,
        households,
        house,
        coordinator,
        backup,
        start,
        &mut attempted,
        baseline,
    );
    result.map_err(|error| {
        if attempted {
            format!("{error}; queue reconstruction stopped without replaying a write; the old source may be paused and a staged suffix may remain; read the queue before trying again")
        } else { error }
    })
}
