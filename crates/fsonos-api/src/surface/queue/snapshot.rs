//! Read a queue page and its transport attribution at one observed revision.
//! Independent controllers can still change the player after the last read.

use fsonos_core::CoreError;
use fsonos_proto::content::{self, BrowseFlag, BrowsePage, QUEUE, SNAPSHOT_ATTEMPTS};
use fsonos_proto::control::{get_media_info, get_position_info};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{Player, PlayerId};
use std::net::IpAddr;

use super::{QueueDto, QueueItemDto};
use crate::failure::{ErrorCode, Failure};

/// Equal across pages, but different for a changed queue or coordinator.
/// Length-prefix the UUID so punctuation in it cannot alias another token.
/// UpdateID is an equality counter, not a timestamp (and may wrap or reset).
pub(super) fn revision(coordinator: &PlayerId, page: &BrowsePage) -> String {
    format!(
        "q1:{}:{}:{}:{}",
        coordinator.0.len(),
        coordinator.0,
        page.update_id,
        page.total_matches
    )
}

fn protocol(error: ProtoError) -> Failure {
    Failure::from(CoreError::from(error))
}

pub(super) fn page<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    offset: u32,
    count: u32,
) -> Result<BrowsePage, Failure> {
    if count == 0 || count > super::MAX_PAGE || offset.checked_add(count).is_none() {
        return Err(Failure::invalid("invalid queue page range"));
    }
    let page = content::browse(
        transport,
        host,
        QUEUE,
        BrowseFlag::DirectChildren,
        offset,
        count,
    )
    .map_err(protocol)?;
    validate(&page, offset, count)?;
    Ok(page)
}

fn validate(page: &BrowsePage, offset: u32, count: u32) -> Result<(), Failure> {
    let got = u32::try_from(page.objects.len()).unwrap_or(u32::MAX);
    let available = page.total_matches.saturating_sub(offset);
    if got != page.number_returned || got > count || got > available || (got == 0 && available > 0)
    {
        return Err(protocol(ProtoError::Malformed(
            "queue returned inconsistent paging counters".into(),
        )));
    }
    Ok(())
}

fn same_track(expected: Option<&str>, actual: &str) -> bool {
    let Some(expected) = expected.filter(|uri| !uri.is_empty() && *uri != "NOT_IMPLEMENTED") else {
        return false;
    };
    if actual.is_empty() || actual == "NOT_IMPLEMENTED" {
        return false;
    }
    match (
        spotify_uri_from_renderer_uri(expected),
        spotify_uri_from_renderer_uri(actual),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => expected == actual,
    }
}

pub(super) fn read<T: Transport + ?Sized>(
    transport: &T,
    coordinator: &Player,
    offset: u32,
    count: u32,
) -> Result<QueueDto, Failure> {
    let host = coordinator.ip;
    let own_queue = format!("x-rincon-queue:{}#0", coordinator.id.0);
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let before = page(transport, host, offset, count)?;
        let media = get_media_info(transport, host).map_err(protocol)?;
        let mut playing = None;
        if media.uri == own_queue {
            let position = get_position_info(transport, host).map_err(protocol)?;
            if position.track > 0 && position.track <= before.total_matches {
                let index = position.track - 1;
                let within = index
                    .checked_sub(offset)
                    .and_then(|i| usize::try_from(i).ok().and_then(|i| before.objects.get(i)));
                // The playing item can be on a different page. Read that
                // single slot rather than attributing an unverified TrackNr.
                let other;
                let item = if let Some(item) = within {
                    Some(item)
                } else {
                    other = page(transport, host, index, 1)?;
                    if (other.update_id, other.total_matches)
                        != (before.update_id, before.total_matches)
                    {
                        continue;
                    }
                    other.objects.first()
                };
                if same_track(
                    item.and_then(|item| item.res.as_ref())
                        .map(|res| res.uri.as_str()),
                    &position.uri,
                ) {
                    playing = Some(position.track);
                }
            }
        }
        let after_media = get_media_info(transport, host).map_err(protocol)?;
        let after = page(transport, host, offset, count)?;
        if before != after || media.uri != after_media.uri {
            continue;
        }
        let token = revision(&coordinator.id, &before);
        let items = (offset + 1..=offset + count)
            .zip(before.objects)
            .map(|(position, item)| QueueItemDto {
                position,
                title: item.title,
                creator: item.creator,
                album: item.album,
                uri: item.res.map(|res| res.uri),
            })
            .collect();
        return Ok(QueueDto {
            zone: coordinator.room_name.clone(),
            total: before.total_matches,
            revision: Some(token),
            playing,
            items,
        });
    }
    Err(Failure::new(
        ErrorCode::NotReady,
        "the queue or active source kept changing while it was read",
    )
    .with_hint("Read the queue again once the other controller's changes settle."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::parse_didl;

    fn empty(total: u32, update_id: u32) -> BrowsePage {
        BrowsePage {
            objects: Vec::new(),
            number_returned: 0,
            total_matches: total,
            update_id,
        }
    }

    #[test]
    fn revision_is_coordinator_scoped_and_compares_counters_by_equality() {
        let a = PlayerId("test:coordinator:a".into());
        let b = PlayerId("test:coordinator:b".into());
        assert_ne!(revision(&a, &empty(2, 0)), revision(&b, &empty(2, 0)));
        assert_ne!(
            revision(&a, &empty(2, 0)),
            revision(&a, &empty(2, u32::MAX))
        );
        assert_ne!(revision(&a, &empty(2, 7)), revision(&a, &empty(3, 7)));
        assert_eq!(revision(&a, &empty(2, 0)), revision(&a, &empty(2, 0)));
    }

    #[test]
    fn empty_and_out_of_range_pages_are_valid_but_stalled_pages_are_not() {
        assert!(validate(&empty(0, 0), 0, 10).is_ok());
        assert!(validate(&empty(2, 1), 5, 10).is_ok());
        assert!(validate(&empty(2, 1), 0, 10).is_err());
        let mut page = empty(0, 0);
        page.number_returned = 1;
        assert!(validate(&page, 0, 10).is_err());
    }

    #[test]
    fn short_pages_are_valid_but_excess_objects_are_not() {
        let mut page = empty(5, 1);
        page.objects = parse_didl(
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
             <item id=\"1\" parentID=\"Q:0\" restricted=\"1\">\
             <dc:title>Test</dc:title></item></DIDL-Lite>",
        )
        .unwrap();
        page.number_returned = 1;
        assert!(validate(&page, 0, 5).is_ok());
        assert!(validate(&page, 5, 5).is_err());
        assert!(validate(&page, 0, 0).is_err());
    }

    #[test]
    fn attribution_requires_real_identity_and_accepts_spotify_encoding_variants() {
        assert!(same_track(
            Some("x-sonos-spotify:spotify%3atrack%3aExample?sid=1"),
            "x-sonos-spotify:spotify%3Atrack%3AExample?sid=2",
        ));
        assert!(!same_track(Some("test:a"), "test:b"));
        assert!(same_track(Some("test:a"), "test:a"));
        for missing in [None, Some(""), Some("NOT_IMPLEMENTED")] {
            assert!(!same_track(missing, "test:a"));
        }
        assert!(!same_track(Some("test:a"), ""));
    }

    #[test]
    fn older_daemon_responses_without_revision_still_decode() {
        let page: QueueDto =
            serde_json::from_str(r#"{"zone":"Room","total":0,"items":[]}"#).unwrap();
        assert!(page.revision.is_none());
    }
}
