//! Retain complete native metadata before a queue becomes a durable backup.

use super::{MAX_QUEUE_BACKUP_BYTES, MAX_QUEUE_BACKUP_ENTRIES, invalid};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::didl::NativeDidlObject;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

pub(super) struct Snapshot {
    pub(super) objects: Vec<NativeDidlObject>,
    pub(super) update_id: u32,
}

/// Read a complete bounded queue, then recheck the exact retained pages.
/// The native counter alone cannot prove that service-owned metadata stayed
/// unchanged. Short pages make progress; a changed page discards the capture.
/// At most twice the occurrence limit is read, even with one-item pages.
pub(super) fn snapshot<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
) -> Result<Snapshot, ProtoError> {
    let before = content::browse(transport, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    let limit = u32::try_from(MAX_QUEUE_BACKUP_ENTRIES).expect("the queue limit fits u32");
    if before.total_matches > limit {
        return Err(invalid(format!("the queue exceeds {limit} occurrences")));
    }
    let mut offset = 0;
    let mut bytes = 0_usize;
    let mut pages = Vec::new();
    while offset < before.total_matches {
        let count = (before.total_matches - offset).min(content::PAGE_SIZE);
        let page = content::browse_with_metadata(
            transport,
            host,
            QUEUE,
            BrowseFlag::DirectChildren,
            offset,
            count,
            MAX_QUEUE_BACKUP_BYTES.saturating_sub(bytes),
        )?;
        if page.update_id != before.update_id
            || page.total_matches != before.total_matches
            || page.number_returned == 0
            || page.number_returned > count
        {
            return Err(invalid(
                "the complete native queue changed or made no progress",
            ));
        }
        for native in &page.objects {
            bytes = bytes
                .checked_add(native.metadata.len())
                .and_then(|bytes| {
                    bytes.checked_add(native.object.res.as_ref().map_or(0, |res| res.uri.len()))
                })
                .filter(|bytes| *bytes <= MAX_QUEUE_BACKUP_BYTES)
                .ok_or_else(|| {
                    invalid(format!(
                        "the queue exceeds its {MAX_QUEUE_BACKUP_BYTES}-byte backup limit"
                    ))
                })?;
        }
        let returned = page.number_returned;
        pages.push((offset, count, page));
        offset += returned;
    }
    for (offset, count, page) in &pages {
        let retained = page.objects.iter().map(|item| item.metadata.len()).sum();
        if content::browse_with_metadata(
            transport,
            host,
            QUEUE,
            BrowseFlag::DirectChildren,
            *offset,
            *count,
            retained,
        )? != *page
        {
            return Err(invalid(
                "native queue metadata changed while saving its contents",
            ));
        }
    }
    let after = content::browse(transport, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if after.update_id != before.update_id || after.total_matches != before.total_matches {
        return Err(invalid(
            "the queue changed while its complete metadata was read",
        ));
    }
    Ok(Snapshot {
        update_id: before.update_id,
        objects: pages
            .into_iter()
            .flat_map(|(_, _, page)| page.objects)
            .collect(),
    })
}
