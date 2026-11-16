//! Guarded native edits to saved playlist contents. The complete saved order
//! is bounded and verified; no live queue, source or playback setting is read
//! or changed. A failed mutation is never sent again.

use super::contents::{self, CompletePlaylist};
use super::{invalid, native_id};
use crate::favorites::Favorite;
use crate::{CoreError, HouseholdState};
use fsonos_proto::didl::DidlObject;
use fsonos_proto::{ProtoError, Transport, queue};
use fsonos_types::PlayerId;

/// A complete playlist must fit this bound before any edit is attempted.
pub const MAX_PLAYLIST_EDIT_TRACKS: u32 = queue::MAX_SAVED_QUEUE_EDIT_TRACKS;
/// Aggregate retained metadata limit for a complete saved playlist edit.
pub const MAX_PLAYLIST_EDIT_BYTES: usize = 4 * 1024 * 1024;

/// A saved order confirmed by complete read-back, or a validated no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistTrackChange {
    pub playlist: Favorite,
    pub revision: String,
    pub previous_total: u32,
    pub total: u32,
    pub changed: bool,
}

/// Remove `count` consecutive saved occurrences starting at one-based `first`.
/// The exact native `SQ:` id and current contents revision are mandatory.
/// Removing every occurrence retains an empty saved playlist. Playing copies
/// of its tracks and the current source are independent and remain untouched.
pub fn remove_tracks<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    expected_revision: &str,
    first: u32,
    count: u32,
) -> Result<PlaylistTrackChange, CoreError> {
    edit(
        transport,
        households,
        coordinator,
        playlist_id,
        expected_revision,
        Edit {
            first,
            count,
            insert_before: None,
        },
    )
}

/// Move a consecutive saved block before a one-based position in the ORIGINAL
/// playlist. `insert_before = total + 1` appends; a boundary inside the block
/// or immediately after it is a verified no-op. Duplicate tracks retain their
/// individual occurrences and the block keeps its internal order.
///
/// One native CSV operation uses the observed content revision. A refusal is
/// returned unchanged; a lost/malformed response or unconfirmed order is an
/// uncertain result, never a reason to replay the edit.
#[allow(clippy::too_many_arguments)]
pub fn move_tracks<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    expected_revision: &str,
    first: u32,
    count: u32,
    insert_before: u32,
) -> Result<PlaylistTrackChange, CoreError> {
    edit(
        transport,
        households,
        coordinator,
        playlist_id,
        expected_revision,
        Edit {
            first,
            count,
            insert_before: Some(insert_before),
        },
    )
}

#[derive(Clone, Copy)]
struct Edit {
    first: u32,
    count: u32,
    insert_before: Option<u32>,
}

impl Edit {
    fn validate(self, playlist_id: &str, expected_revision: &str) -> Result<(), CoreError> {
        if !native_id(playlist_id)
            || playlist_id.len() > 1024
            || expected_revision.trim().is_empty()
            || expected_revision.len() > 4096
            || expected_revision.chars().any(char::is_control)
        {
            return Err(invalid(
                "editing saved tracks requires an exact SQ: id and observed revision",
            ));
        }
        if self.first == 0
            || self.count == 0
            || self.count > MAX_PLAYLIST_EDIT_TRACKS
            || self.first.checked_add(self.count - 1).is_none()
            || self.insert_before == Some(0)
        {
            return Err(invalid(
                "saved playlist edits require a bounded nonempty one-based range",
            ));
        }
        Ok(())
    }

    fn plan(self, total: u32) -> Result<Plan, CoreError> {
        let start = self.first - 1;
        let end = start + self.count;
        if end > total || self.insert_before.is_some_and(|before| before > total + 1) {
            return Err(invalid(
                "saved playlist edit positions exceed its observed length",
            ));
        }
        let mut order: Vec<usize> = (0..total as usize).collect();
        let tracks: Vec<u32> = (start..end).rev().collect();
        let Some(before) = self.insert_before else {
            order.drain(start as usize..end as usize);
            return Ok(Plan {
                order,
                tracks,
                positions: None,
                changed: true,
            });
        };
        let destination = before - 1;
        if (start..=end).contains(&destination) {
            return Ok(Plan {
                order,
                tracks,
                positions: None,
                changed: false,
            });
        }
        // The native CSV maps original occurrences to final indexes. The
        // descending source order follows SoCo's documented multi-item form.
        let final_start = if destination < start {
            destination
        } else {
            destination - self.count
        };
        let positions = (final_start..final_start + self.count).rev().collect();
        let moved: Vec<_> = order.drain(start as usize..end as usize).collect();
        order.splice(final_start as usize..final_start as usize, moved);
        Ok(Plan {
            order,
            tracks,
            positions: Some(positions),
            changed: true,
        })
    }
}

struct Plan {
    order: Vec<usize>,
    tracks: Vec<u32>,
    positions: Option<Vec<u32>>,
    changed: bool,
}

fn edit<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    expected_revision: &str,
    edit: Edit,
) -> Result<PlaylistTrackChange, CoreError> {
    edit.validate(playlist_id, expected_revision)?;
    let before = contents::complete(
        transport,
        households,
        coordinator,
        playlist_id,
        Some(expected_revision),
    )?;
    let plan = edit.plan(before.total)?;
    if !plan.changed {
        return Ok(result(before, false, None));
    }
    if before.update_id == 0 {
        return Err(invalid(
            "a zero native playlist revision cannot guard an edit; refresh its contents",
        ));
    }
    before.group.confirm(transport, before.host, coordinator)?;
    let reply = match queue::reorder_saved_tracks(
        transport,
        before.host,
        playlist_id,
        before.update_id,
        &plan.tracks,
        plan.positions.as_deref(),
    ) {
        Ok(reply) => reply,
        Err(ProtoError::SoapFault {
            code: queue::STALE_QUEUE_FAULT,
            ..
        }) => {
            return Err(CoreError::PlaylistChanged(
                "the saved playlist changed before its native edit; refresh its contents".into(),
            ));
        }
        Err(error @ ProtoError::SoapFault { .. }) => return Err(error.into()),
        Err(error) => return Err(uncertain(error)),
    };
    let expected_total = u32::try_from(plan.order.len()).expect("saved edits are bounded");
    let expected_change = i64::from(expected_total) - i64::from(before.total);
    if reply.total != expected_total
        || i64::from(reply.length_change) != expected_change
        || reply.update_id == before.update_id
    {
        return Err(uncertain(
            "the native acknowledgement did not match the requested edit",
        ));
    }
    let after = contents::complete(transport, households, coordinator, playlist_id, None)
        .map_err(uncertain)?;
    if before.group != after.group
        || before.playlist != after.playlist
        || after.update_id != reply.update_id
        || after.total != expected_total
        || !plan
            .order
            .iter()
            .zip(&after.items)
            .all(|(&index, actual)| same_occurrence(&before.items[index], actual))
    {
        return Err(uncertain(
            "the saved playlist did not confirm the complete requested order",
        ));
    }
    Ok(result(after, true, Some(before.total)))
}

fn same_occurrence(before: &DidlObject, after: &DidlObject) -> bool {
    // Native occurrence ids include their position and can be renumbered.
    // Every other field, including unavailable resources, must agree.
    let mut expected = before.clone();
    expected.id.clone_from(&after.id);
    expected == *after
}

fn result(
    after: CompletePlaylist,
    changed: bool,
    previous_total: Option<u32>,
) -> PlaylistTrackChange {
    PlaylistTrackChange {
        playlist: after.playlist,
        revision: after.revision,
        previous_total: previous_total.unwrap_or(after.total),
        total: after.total,
        changed,
    }
}

fn uncertain(detail: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "saved playlist track edit could not be confirmed: {detail}; its contents may have changed; \
         refresh the original saved playlist before another edit; the mutation was not replayed"
    ))
}
