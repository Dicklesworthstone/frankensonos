//! Observe a command's progress without changing its protocol or its replies.

use fsonos_proto::{Discovered, HttpBody, ProtoError, Transport, mdns, soap, ssdp};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

/// A fresh observer per command attempt, never shared with another request.
/// A later failure must not restart earlier writes that may have taken effect.
pub(super) struct Observed<'a, T: Transport + ?Sized> {
    inner: &'a T,
    writes: Cell<usize>,
    preceding: Cell<usize>,
    accepted: Cell<bool>,
}

impl<'a, T: Transport + ?Sized> Observed<'a, T> {
    pub(super) fn new(inner: &'a T) -> Self {
        Self {
            inner,
            writes: Cell::new(0),
            preceding: Cell::new(0),
            accepted: Cell::new(false),
        }
    }

    fn begin(&self) {
        self.preceding.set(self.writes.get());
        self.accepted.set(false);
    }

    pub(super) fn preceding_writes(&self) -> usize {
        // A semantic error after an accepted response must not replay that
        // write either, even when no subsequent transport call was made.
        if self.accepted.get() {
            self.writes.get()
        } else {
            self.preceding.get()
        }
    }
}

/// Only known observation verbs are exempt. Unknown actions are conservative
/// writes; never infer read-only behavior merely from an action's prefix.
fn read_only(action: &str) -> bool {
    matches!(
        action,
        "Browse"
            | "GetZoneGroupState"
            | "GetTransportInfo"
            | "GetTransportSettings"
            | "GetMediaInfo"
            | "GetPositionInfo"
            | "GetCurrentTransportActions"
            | "GetVolume"
            | "GetMute"
            | "GetGroupVolume"
            | "GetGroupMute"
            | "GetBass"
            | "GetTreble"
            | "GetLoudness"
            | "GetEQ"
            | "GetRemainingSleepTimerDuration"
    )
}

/// `SnapshotGroupVolume` only records the members' volume ratios for the
/// group volume write that follows it: nothing audible changes, and every
/// group volume attempt, a recovery's included, takes its own snapshot first.
/// It is not an earlier effect a later refusal or lost reply leaves behind.
fn prepares_only(action: &str) -> bool {
    action == "SnapshotGroupVolume"
}

impl<T: Transport + ?Sized> Transport for Observed<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.begin();
        let reply = self.inner.soap_post(host, path, header, body);
        let action = header.trim_matches('"').rsplit('#').next().unwrap_or("");
        if !read_only(action) && !prepares_only(action) {
            // Transport implementations may return the fault as an error or
            // as HTTP 500's raw SOAP body. Recognize both with the real parser.
            // Only 800 proves the refusal used by coordinator recovery here:
            // e.g. a Sonos Leave/402 can take effect despite its fault reply.
            let (refused, accepted) = match &reply {
                Err(ProtoError::SoapFault { code: 800, .. }) => (true, false),
                Ok(xml) => match soap::parse_response(xml, action) {
                    Err(ProtoError::SoapFault { code: 800, .. }) => (true, false),
                    Ok(_) => (false, true),
                    Err(_) => (false, false),
                },
                _ => (false, false),
            };
            self.accepted.set(accepted);
            if !refused {
                self.writes.set(self.writes.get().saturating_add(1));
            }
        }
        reply
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.begin();
        self.inner.http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        self.begin();
        self.inner.http_get_bytes(url)
    }

    fn ssdp_search(&self, mx: u8, wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        self.begin();
        self.inner.ssdp_search(mx, wait)
    }

    fn mdns_search(&self, wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
        self.begin();
        self.inner.mdns_search(wait)
    }

    fn discover(&self, mx: u8, wait: Duration) -> Discovered {
        self.begin();
        self.inner.discover(mx, wait)
    }
}

#[cfg(test)]
mod tests;
