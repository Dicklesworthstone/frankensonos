//! Resolve exact live-queue occurrences into retained native tracks. Reading
//! the queue does not require it to be selected or change the current source.

use super::contents::native_evidence_bytes;
use super::selection::{native_item, track};
use super::{GroupProof, MAX_PLAYLIST_EDIT_BYTES, invalid};
use crate::favorites::Favorite;
use crate::queue_source::native::replay::{self, Identity, Selected};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_proto::content::{self, BrowseFlag, BrowsePage, NativeBrowsePage, QUEUE};
use fsonos_proto::didl::{NativeDidlObject, parse_didl, spotify_uri_from_renderer_uri};
use fsonos_types::PlayerId;
use std::collections::BTreeMap;
use std::net::IpAddr;

/// The existing queue-page equality token, shared with the HTTP queue reader.
/// It binds the coordinator, native update counter and total, not permission.
#[must_use]
pub fn queue_selection_revision(coordinator: &PlayerId, page: &BrowsePage) -> String {
    revision(coordinator, page.update_id, page.total_matches)
}

fn revision(coordinator: &PlayerId, update_id: u32, total_matches: u32) -> String {
    format!(
        "q1:{}:{}:{}:{}",
        coordinator.0.len(),
        coordinator.0,
        update_id,
        total_matches
    )
}

/// Exact source occurrences and their replayable native metadata. The original
/// queue evidence stays separate: Spotify queue IDs may need the household's
/// observed service identity before the same tracks can be enqueued elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedQueueSelection {
    coordinator: PlayerId,
    positions: Vec<u32>,
    revision: String,
    group: GroupProof,
    current: Option<Selected>,
    observed: Vec<Favorite>,
    tracks: Vec<Favorite>,
}

impl PreparedQueueSelection {
    /// Read and prepare 1–10 occurrences, preserving order and repetitions.
    /// No queue or playback changes occur. A missing native service identity
    /// must be learned from this household; it is never guessed or borrowed.
    pub fn read<T: Transport + ?Sized>(
        transport: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        positions: &[u32],
        revision: &str,
    ) -> Result<Self, CoreError> {
        validate_selection(positions, revision)?;
        let (host, group) = observed_group(transport, households, coordinator)?;
        let observed =
            select_queue_tracks(transport, households, coordinator, positions, revision)?;
        // Native queue replay needs only the observed account and service
        // identity, not the flags required to render a new direct source.
        // Fully replayable metadata must not depend on an advancing playhead.
        let mut needs_identity = false;
        for selected in &observed {
            if let Some(uri) = selected.uri.as_deref()
                && spotify_uri_from_renderer_uri(uri)
                    .is_some_and(|source| source.starts_with("spotify:track:"))
                && Identity::read(uri, &selected.metadata)?.is_none()
            {
                needs_identity = true;
            }
        }
        let current = if needs_identity {
            Selected::current(transport, host, coordinator)?
        } else {
            None
        };
        let mut parameters = None;
        let mut tracks = Vec::with_capacity(observed.len());
        for selected in &observed {
            let native = NativeDidlObject {
                object: native_item(selected)?,
                metadata: selected.metadata.clone(),
            };
            let replay = replay::item(
                transport,
                households,
                coordinator,
                native,
                &mut parameters,
                current.as_ref(),
            )?;
            let objects = parse_didl(&replay.metadata)?;
            let [item] = objects.as_slice() else {
                return Err(invalid(
                    "queue replay metadata must retain exactly one item",
                ));
            };
            tracks.push(track(item, &replay.metadata)?);
        }
        let selection = Self {
            coordinator: coordinator.clone(),
            positions: positions.to_vec(),
            revision: revision.into(),
            group,
            current,
            observed,
            tracks,
        };
        selection.confirm(transport, households)?;
        Ok(selection)
    }

    #[must_use]
    pub fn coordinator(&self) -> &PlayerId {
        &self.coordinator
    }

    #[must_use]
    pub fn positions(&self) -> &[u32] {
        &self.positions
    }

    #[must_use]
    pub fn tracks(&self) -> &[Favorite] {
        &self.tracks
    }

    /// Confirm the original source, not a destination that now contains copies.
    /// Source-targeted operations must dispatch this group after other targets.
    pub fn confirm<T: Transport + ?Sized>(
        &self,
        transport: &T,
        households: &[HouseholdState],
    ) -> Result<(), CoreError> {
        let host = control::locate(households, &self.coordinator)?.ip;
        self.group
            .confirm(transport, host, &self.coordinator)
            .map_err(|error| changed(&error.to_string()))?;
        let current = select_queue_tracks(
            transport,
            households,
            &self.coordinator,
            &self.positions,
            &self.revision,
        )?;
        if current != self.observed {
            return Err(changed(
                "the selected queue occurrences changed after preparation",
            ));
        }
        if let Some(current) = &self.current {
            current
                .confirm(transport, host)
                .map_err(|error| changed(&error.to_string()))?;
        }
        // Selected deliberately leaves membership to callers that may regroup.
        // Keep this copy's original full group through the final metadata read.
        self.group
            .confirm(transport, host, &self.coordinator)
            .map_err(|error| changed(&error.to_string()))
    }
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
    validate_selection(positions, expected_revision)?;
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
        let before = page(
            transport,
            host,
            offset,
            limit,
            MAX_PLAYLIST_EDIT_BYTES.saturating_sub(bytes),
        )?;
        if revision(coordinator, before.update_id, before.total_matches) != expected_revision {
            return Err(changed("the queue observation is no longer current"));
        }
        if before.objects.is_empty() {
            return Err(invalid("the queue has no track at a selected position"));
        }
        for item in &before.objects {
            bytes = bytes.saturating_add(native_evidence_bytes(item));
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
        if page(transport, host, offset, limit, MAX_PLAYLIST_EDIT_BYTES)? != before {
            return Err(changed(
                "selected queue occurrences changed while they were read",
            ));
        }
    }
    group
        .confirm(transport, host, coordinator)
        .map_err(|error| changed(&error.to_string()))?;
    let selected_bytes = positions.iter().fold(0_usize, |bytes, position| {
        bytes.saturating_add(native_evidence_bytes(&selected[position]))
    });
    if selected_bytes > MAX_PLAYLIST_EDIT_BYTES {
        return Err(invalid("repeated queue selections exceed 4 MiB"));
    }
    positions
        .iter()
        .map(|position| {
            let selected = &selected[position];
            track(&selected.object, &selected.metadata)
        })
        .collect()
}

fn validate_selection(positions: &[u32], expected_revision: &str) -> Result<(), CoreError> {
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
    Ok(())
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
    max_bytes: usize,
) -> Result<NativeBrowsePage, CoreError> {
    let page = content::browse_with_metadata(
        transport,
        host,
        QUEUE,
        BrowseFlag::DirectChildren,
        offset,
        limit,
        max_bytes,
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
