//! Bounded, read-only observations of a native saved playlist. Catalog
//! metadata and the requested children are checked again before publication;
//! a continuation must still belong to the same player and native revision.

use super::{GroupProof, native_id, playlist};
use crate::favorites::Favorite;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, BrowseFlag, BrowsePage};
use fsonos_proto::didl::DidlObject;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

/// At most one Sonos page is requested at a time. A short page is continued
/// from the next observed occurrence, without silently skipping entries.
pub const MAX_CONTENTS_PAGE: u32 = content::PAGE_SIZE;

/// Ordered occurrences observed in a saved playlist. Equal track resources
/// remain separate entries; an item's one-based position is `offset + i + 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistContents {
    pub playlist: Favorite,
    /// An opaque equality token, stable across pages of this observation.
    pub revision: String,
    pub total: u32,
    /// Zero-based starting occurrence, including for an empty final page.
    pub offset: u32,
    pub next_offset: Option<u32>,
    pub items: Vec<DidlObject>,
}

/// Browse a saved playlist without changing the live queue or playback.
///
/// `playlist_id` is the exact native `SQ:<n>` id from this household's
/// catalog. Titles and numbered catalog positions must be resolved before
/// calling. Every continuation (`offset > 0`) requires the previous page's
/// revision; stale observations fail instead of returning a mixed playlist.
/// A changed player boot, group, catalog entry, content counter or total
/// invalidates that token. It is not an atomic snapshot or authorization.
///
/// Catalog metadata, topology and the selected page are each read twice.
/// Unavailable items are retained as observations, including absent resource
/// URIs; no track is filtered out or converted into an instruction to play.
pub fn contents<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    offset: u32,
    limit: u32,
    expected_revision: Option<&str>,
) -> Result<PlaylistContents, CoreError> {
    validate_request(playlist_id, offset, limit, expected_revision)?;
    let mut owners = households
        .iter()
        .filter(|house| house.player(coordinator).is_some());
    let house = owners
        .next()
        .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
    if owners.next().is_some() {
        return Err(changed(
            "the playlist player belongs to more than one household",
        ));
    }
    let mut groups = house
        .groups
        .iter()
        .filter(|group| group.coordinator == *coordinator);
    let group = groups
        .next()
        .ok_or_else(|| changed("the playlist player no longer coordinates its group"))?;
    if groups.next().is_some() {
        return Err(changed("the playlist player's group is ambiguous"));
    }
    let host = control::locate(households, coordinator)?.ip;
    let before_group = GroupProof::read(transport, host, coordinator)?;
    let mut members = group.members.clone();
    members.sort_by(|left, right| left.0.cmp(&right.0));
    members.dedup();
    if before_group.members != members {
        return Err(changed("the playlist player's group membership changed"));
    }
    let before_metadata = metadata(transport, host, playlist_id)?;
    let selected = playlist(&before_metadata.objects[0])?;
    let before = page(transport, host, playlist_id, offset, limit)?;
    let revision = revision(
        house,
        coordinator,
        &before_group,
        &selected,
        before_metadata.update_id,
        &before,
    );
    if expected_revision.is_some_and(|expected| expected != revision) {
        return Err(changed(
            "the saved playlist observation is no longer current",
        ));
    }
    let after = page(transport, host, playlist_id, offset, limit)?;
    let after_metadata = metadata(transport, host, playlist_id)?;
    let after_group = GroupProof::read(transport, host, coordinator)?;
    if before != after || before_metadata != after_metadata || before_group != after_group {
        return Err(changed(
            "the saved playlist or its player changed while it was read",
        ));
    }
    let next = offset + before.number_returned;
    Ok(PlaylistContents {
        playlist: selected,
        revision,
        total: before.total_matches,
        offset,
        next_offset: (next < before.total_matches).then_some(next),
        items: before.objects,
    })
}

fn validate_request(
    playlist_id: &str,
    offset: u32,
    limit: u32,
    expected_revision: Option<&str>,
) -> Result<(), CoreError> {
    if !native_id(playlist_id) || playlist_id.len() > 1024 {
        return Err(invalid("playlist contents require an exact native SQ: id"));
    }
    if limit == 0 || limit > MAX_CONTENTS_PAGE || offset.checked_add(limit).is_none() {
        return Err(invalid("invalid saved playlist page range"));
    }
    if offset > 0 && expected_revision.is_none() {
        return Err(invalid(
            "continuing a saved playlist requires its expected revision",
        ));
    }
    if expected_revision.is_some_and(|revision| {
        revision.is_empty() || revision.len() > 4096 || revision.chars().any(char::is_control)
    }) {
        return Err(invalid("invalid saved playlist revision"));
    }
    Ok(())
}

fn metadata<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    playlist_id: &str,
) -> Result<BrowsePage, CoreError> {
    let result = content::browse(transport, host, playlist_id, BrowseFlag::Metadata, 0, 1)?;
    if result.number_returned != 1
        || result.total_matches != 1
        || result.objects.len() != 1
        || result.objects[0].id != playlist_id
    {
        return Err(invalid(
            "saved playlist metadata did not identify exactly the requested entry",
        ));
    }
    // Reuse the catalog's class, parent and resource checks for both reads.
    playlist(&result.objects[0])?;
    Ok(result)
}

fn page<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    playlist_id: &str,
    offset: u32,
    limit: u32,
) -> Result<BrowsePage, CoreError> {
    let page = content::browse(
        transport,
        host,
        playlist_id,
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
            "saved playlist returned inconsistent paging counters",
        ));
    }
    Ok(page)
}

fn revision(
    house: &HouseholdState,
    coordinator: &PlayerId,
    group: &GroupProof,
    playlist: &Favorite,
    catalog_update: u32,
    page: &BrowsePage,
) -> String {
    // Explicit field order plus a fixed hash keeps this equality token stable
    // across processes without disclosing native account descriptors. It does
    // not grant permission or assert that the speaker cannot change again.
    let bytes = serde_json::to_vec(&(
        &house.id,
        coordinator,
        house.generation(),
        group.boot_seq,
        &group.members,
        &playlist.id,
        &playlist.title,
        &playlist.uri,
        &playlist.metadata,
        &playlist.art_uri,
        catalog_update,
        page.update_id,
        page.total_matches,
    ))
    .expect("saved playlist observation fields serialize to JSON");
    let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    });
    format!("sonos-playlist-v1-{digest:016x}")
}

fn invalid(detail: &str) -> CoreError {
    ProtoError::Malformed(detail.into()).into()
}

fn changed(detail: &str) -> CoreError {
    CoreError::PlaylistChanged(format!(
        "{detail}; read the saved playlist again from the beginning"
    ))
}
