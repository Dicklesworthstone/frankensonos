//! Complete native queue observations for replay and durable reconstruction.

use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::didl::NativeDidlObject;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

pub(crate) struct Snapshot {
    pub(crate) objects: Vec<NativeDidlObject>,
    pub(crate) update_id: u32,
    pages: Vec<Page>,
}

struct Page {
    offset: u32,
    requested: u32,
    returned: u32,
}

impl Snapshot {
    /// Recheck the exact retained native pages, including service-owned XML
    /// that can change without a new queue counter. Rechecks retain only one
    /// page at a time and cannot exceed the original capture's byte budget.
    pub(crate) fn confirm<T: Transport + ?Sized>(
        &self,
        transport: &T,
        host: IpAddr,
    ) -> Result<(), ProtoError> {
        self.confirm_head(transport, host)?;
        for page in &self.pages {
            let start = usize::try_from(page.offset).expect("a captured offset fits usize");
            let count = usize::try_from(page.returned).expect("one page fits usize");
            let expected = &self.objects[start..start + count];
            let retained = expected.iter().map(|item| item.metadata.len()).sum();
            let actual = content::browse_with_metadata(
                transport,
                host,
                QUEUE,
                BrowseFlag::DirectChildren,
                page.offset,
                page.requested,
                retained,
            )?;
            if actual.update_id != self.update_id
                || usize::try_from(actual.total_matches).ok() != Some(self.objects.len())
                || actual.number_returned != page.returned
                || actual.objects != expected
            {
                return Err(invalid("native queue metadata changed after it was read"));
            }
        }
        self.confirm_head(transport, host)
    }

    fn confirm_head<T: Transport + ?Sized>(
        &self,
        transport: &T,
        host: IpAddr,
    ) -> Result<(), ProtoError> {
        let page = content::browse(transport, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
        if page.update_id != self.update_id
            || usize::try_from(page.total_matches).ok() != Some(self.objects.len())
        {
            return Err(invalid(
                "the queue changed while its complete metadata was read",
            ));
        }
        Ok(())
    }
}

/// Read a complete bounded queue, then recheck the exact retained pages.
/// Short pages make progress; a changed page discards the observation. Count
/// every retained URI and self-contained document, including repeated roots.
/// Each pass is bounded by both the occurrence limit and MAX_BROWSE_PAGES.
pub(crate) fn snapshot<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    max_entries: u32,
    max_bytes: usize,
) -> Result<Snapshot, ProtoError> {
    let before = content::browse(transport, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
    if before.total_matches > max_entries.min(content::MAX_BROWSE_OBJECTS) {
        return Err(invalid(format!(
            "the queue exceeds {max_entries} occurrences"
        )));
    }
    let mut snapshot = Snapshot {
        objects: Vec::new(),
        update_id: before.update_id,
        pages: Vec::new(),
    };
    let mut offset = 0;
    let mut bytes = 0_usize;
    while offset < before.total_matches {
        if snapshot.pages.len() == content::MAX_BROWSE_PAGES {
            return Err(invalid("the native queue exceeds its bounded page limit"));
        }
        let count = (before.total_matches - offset).min(content::PAGE_SIZE);
        let page = content::browse_with_metadata(
            transport,
            host,
            QUEUE,
            BrowseFlag::DirectChildren,
            offset,
            count,
            max_bytes.saturating_sub(bytes),
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
                .filter(|bytes| *bytes <= max_bytes)
                .ok_or_else(|| invalid(format!("the queue exceeds its {max_bytes}-byte limit")))?;
        }
        snapshot.pages.push(Page {
            offset,
            requested: count,
            returned: page.number_returned,
        });
        offset += page.number_returned;
        snapshot.objects.extend(page.objects);
    }
    snapshot.confirm(transport, host)?;
    Ok(snapshot)
}

fn invalid(detail: impl std::fmt::Display) -> ProtoError {
    ProtoError::Malformed(detail.to_string())
}
