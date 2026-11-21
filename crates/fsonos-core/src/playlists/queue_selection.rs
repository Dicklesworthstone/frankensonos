//! Resolve exact live-queue occurrences into retained native tracks. Reading
//! the queue does not require it to be selected or change the current source.

use super::contents::evidence_bytes;
use super::selection::track;
use super::{GroupProof, MAX_PLAYLIST_EDIT_BYTES, invalid};
use crate::favorites::Favorite;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_proto::content::{self, BrowseFlag, BrowsePage, QUEUE};
use fsonos_types::PlayerId;
use std::collections::BTreeMap;
use std::net::IpAddr;

/// The existing queue-page equality token, shared with the HTTP queue reader.
/// It binds the coordinator, native update counter and total, not permission.
#[must_use]
pub fn queue_selection_revision(coordinator: &PlayerId, page: &BrowsePage) -> String {
    format!(
        "q1:{}:{}:{}:{}",
        coordinator.0.len(),
        coordinator.0,
        page.update_id,
        page.total_matches
    )
}

/// Select 1–10 one-based queue occurrences in requested order, with repeats.
/// The mandatory revision comes from a prior queue page for this coordinator.
/// Nearby selected positions share bounded pages; each page is read twice at
/// that revision, and the same group and player boot must remain established.
///
/// Only actual native music tracks can become selections. Their resource and
/// metadata come from the speaker, never from supplied renderer arguments.
/// This performs only reads, including while another source plays.
pub fn select_queue_tracks<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    positions: &[u32],
    expected_revision: &str,
) -> Result<Vec<Favorite>, CoreError> {
    if positions.is_empty()
        || positions.len() > control::MAX_TRACK_BATCH
        || positions
            .iter()
            .any(|position| *position == 0 || *position > content::MAX_BROWSE_OBJECTS)
        || expected_revision.trim().is_empty()
        || expected_revision.len() > 4096
        || expected_revision.chars().any(char::is_control)
    {
        return Err(invalid(
            "a queue selection requires 1 to 10 bounded one-based positions and an observed revision",
        ));
    }
    let (host, group) = observed_group(transport, households, coordinator)?;
    let mut pending = positions.to_vec();
    pending.sort_unstable();
    pending.dedup();
    let mut selected = BTreeMap::new();
    let mut pages = Vec::new();
    let mut bytes = 0_usize;
    while let Some(&first) = pending.first() {
        let last = pending
            .iter()
            .copied()
            .take_while(|position| position - first < content::PAGE_SIZE)
            .last()
            .expect("a nonempty selection includes its first position");
        let offset = first - 1;
        let limit = last - first + 1;
        let before = page(transport, host, offset, limit)?;
        if queue_selection_revision(coordinator, &before) != expected_revision {
            return Err(changed("the queue observation is no longer current"));
        }
        if before.objects.is_empty() {
            return Err(invalid("the queue has no track at a selected position"));
        }
        for item in &before.objects {
            bytes = bytes.saturating_add(evidence_bytes(item));
            if bytes > MAX_PLAYLIST_EDIT_BYTES {
                return Err(invalid("queue selection evidence exceeds 4 MiB"));
            }
        }
        let end = first + before.number_returned;
        for &position in pending.iter().take_while(|&&position| position < end) {
            let index = usize::try_from(position - first).expect("queue pages fit in usize");
            selected.insert(position, before.objects[index].clone());
        }
        pending.retain(|position| *position >= end);
        pages.push((offset, limit, before));
    }
    for (offset, limit, before) in pages {
        if page(transport, host, offset, limit)? != before {
            return Err(changed(
                "selected queue occurrences changed while they were read",
            ));
        }
    }
    group
        .confirm(transport, host, coordinator)
        .map_err(|error| changed(&error.to_string()))?;
    positions
        .iter()
        .map(|position| track(&selected[position]))
        .collect()
}

fn observed_group<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(IpAddr, GroupProof), CoreError> {
    let mut houses = households
        .iter()
        .filter(|house| house.player(coordinator).is_some());
    let house = houses
        .next()
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    if houses.next().is_some() {
        return Err(changed(
            "the queue coordinator belongs to more than one household",
        ));
    }
    let mut groups = house
        .groups
        .iter()
        .filter(|group| group.coordinator == *coordinator);
    let expected = groups
        .next()
        .ok_or_else(|| changed("the queue player no longer leads its group"))?;
    if groups.next().is_some() {
        return Err(changed("the queue coordinator's group is ambiguous"));
    }
    let host = control::locate(households, coordinator)?.ip;
    let group = GroupProof::read(transport, host, coordinator)?;
    let mut members = expected.members.clone();
    members.sort_by(|left, right| left.0.cmp(&right.0));
    members.dedup();
    if group.members != members {
        return Err(changed("the queue player's group membership changed"));
    }
    Ok((host, group))
}

fn page<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    offset: u32,
    limit: u32,
) -> Result<BrowsePage, CoreError> {
    let page = content::browse(
        transport,
        host,
        QUEUE,
        BrowseFlag::DirectChildren,
        offset,
        limit,
    )?;
    let returned = u32::try_from(page.objects.len()).unwrap_or(u32::MAX);
    let available = page.total_matches.saturating_sub(offset);
    if page.total_matches > content::MAX_BROWSE_OBJECTS
        || returned != page.number_returned
        || returned > limit
        || returned > available
        || (returned == 0 && available > 0)
    {
        return Err(invalid(
            "the queue returned inconsistent selection paging counters",
        ));
    }
    Ok(page)
}

fn changed(detail: &str) -> CoreError {
    CoreError::QueueChanged(format!(
        "{detail}; read the source queue again before selecting tracks"
    ))
}
