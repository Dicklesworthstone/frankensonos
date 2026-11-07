//! Exact queue identities and the single-revision append fast path.
//!
//! Sonos increments its queue counter for each mutation (docs/PROTOCOL.md
//! sections 4 and 7). A valid one-track acknowledgement plus exactly one
//! observed revision step proves that no other queue edit intervened. Skips,
//! wrap, unknown boot identity and uncertain replies require full read-back.

/// URI bytes retained by one short-lived batch, in addition to the protocol
/// layer's 100,000-occurrence bound. Larger observations use full reads.
pub(super) const MAX_CACHED_URI_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Queue {
    pub(super) revision: u32,
    pub(super) uris: Vec<String>,
    bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Expected {
    pub(super) index: usize,
    pub(super) position: u32,
    pub(super) length: u32,
}

/// One validated, normalized Browse result from exactly this requested slot.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Slot {
    pub(super) offset: u32,
    pub(super) revision: u32,
    pub(super) total: u32,
    pub(super) uri: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Observed {
    Inserted,
    Unchanged,
    Indeterminate,
}

impl Queue {
    pub(super) fn new(revision: u32, uris: Vec<String>) -> Self {
        let bytes = uris
            .iter()
            .try_fold(0usize, |sum, uri| sum.checked_add(uri.len()))
            .unwrap_or(usize::MAX);
        Self {
            revision,
            uris,
            bytes,
        }
    }

    pub(super) fn cacheable(&self) -> bool {
        self.bytes <= MAX_CACHED_URI_BYTES
    }

    pub(super) fn expected(&self, desired: u32) -> Option<Expected> {
        let index = if desired == 0 {
            self.uris.len()
        } else {
            usize::try_from(desired.checked_sub(1)?).ok()?
        };
        (index <= self.uris.len()).then_some(Expected {
            index,
            position: u32::try_from(index.checked_add(1)?).ok()?,
            length: u32::try_from(self.uris.len().checked_add(1)?).ok()?,
        })
    }

    pub(super) fn matches_head(&self, head: &Slot) -> bool {
        head.offset == 0
            && head.revision == self.revision
            && usize::try_from(head.total).ok() == Some(self.uris.len())
            && head.uri.as_ref() == self.uris.first()
    }

    pub(super) fn proves_single_step(&self, slot: &Slot, expected: Expected, uri: &str) -> bool {
        !uri.is_empty()
            && self.revision.checked_add(1) == Some(slot.revision)
            && slot.offset.checked_add(1) == Some(expected.position)
            && slot.total == expected.length
            && slot.uri.as_deref() == Some(uri)
    }

    /// Used only after the observed successor and inserted occurrence agree.
    pub(super) fn inserted(&mut self, expected: Expected, uri: &str, revision: u32) {
        self.bytes = self.bytes.saturating_add(uri.len());
        self.uris.insert(expected.index, uri.to_owned());
        self.revision = revision;
    }
}

impl Expected {
    pub(super) fn accepts(self, acknowledgement: (u32, Option<u32>)) -> bool {
        acknowledgement.0 == self.position
            && acknowledgement.1.is_none_or(|length| length == self.length)
    }
}

/// Preserve both sides of the intended slot, including duplicate URIs.
/// An unchanged counter with changed contents is never confirmation.
pub(super) fn observe(before: &Queue, after: &Queue, index: usize, uri: &str) -> Observed {
    if before == after {
        return Observed::Unchanged;
    }
    if before.revision != after.revision
        && index <= before.uris.len()
        && before.uris.len().checked_add(1) == Some(after.uris.len())
        && after.uris.get(index).is_some_and(|found| found == uri)
        && before.uris[..index] == after.uris[..index]
        && before.uris[index..] == after.uris[index + 1..]
    {
        return Observed::Inserted;
    }
    Observed::Indeterminate
}

#[cfg(test)]
mod tests;
