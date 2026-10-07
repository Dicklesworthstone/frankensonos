//! ContentDirectory: browse a player's favorites, queue, and playlists.
//!
//! `Browse` returns one page of a container as an escaped DIDL-Lite document
//! plus paging counters. Building the request and parsing the page are pure;
//! [`browse`] and [`browse_all`] dispatch through [`Transport`].

use crate::didl::{DidlObject, parse_didl};
use crate::{ProtoError, Transport, soap};
use std::net::IpAddr;

/// Sonos favorites (the household-wide "My Sonos" list).
pub const FAVORITES: &str = "FV:2";
/// The coordinator's play queue.
pub const QUEUE: &str = "Q:0";
/// Saved Sonos playlists.
pub const SONOS_PLAYLISTS: &str = "SQ:";

/// Page size [`browse_all`] requests. Sonos caps a page at 100 objects.
pub const PAGE_SIZE: u32 = 100;

/// What a `Browse` call returns: an object's children, or the object itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowseFlag {
    DirectChildren,
    Metadata,
}

impl BrowseFlag {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectChildren => "BrowseDirectChildren",
            Self::Metadata => "BrowseMetadata",
        }
    }
}

/// One page of a `Browse` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowsePage {
    pub objects: Vec<DidlObject>,
    pub number_returned: u32,
    pub total_matches: u32,
    /// Container update counter; changes when the container's contents do.
    pub update_id: u32,
}

/// The argument fragment for a `Browse` of `object_id`.
#[must_use]
pub fn browse_args(object_id: &str, flag: BrowseFlag, start: u32, count: u32) -> String {
    soap::args_xml(&[
        ("ObjectID", object_id),
        ("BrowseFlag", flag.as_str()),
        ("Filter", "*"),
        ("StartingIndex", &start.to_string()),
        ("RequestedCount", &count.to_string()),
        ("SortCriteria", ""),
    ])
}

/// Parse a `BrowseResponse` SOAP body.
pub fn parse_browse_response(body: &str) -> Result<BrowsePage, ProtoError> {
    page_from(&soap::parse_response(body, "Browse")?)
}

fn page_from(response: &soap::SoapResponse) -> Result<BrowsePage, ProtoError> {
    Ok(BrowsePage {
        objects: parse_didl(response.require("Result")?)?,
        number_returned: response.require_u32("NumberReturned")?,
        total_matches: response.require_u32("TotalMatches")?,
        update_id: response.require_u32("UpdateID")?,
    })
}

/// Browse one page of `object_id` on the player at `host`.
pub fn browse<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    object_id: &str,
    flag: BrowseFlag,
    start: u32,
    count: u32,
) -> Result<BrowsePage, ProtoError> {
    let response = soap::call(
        transport,
        host,
        &soap::CONTENT_DIRECTORY,
        "Browse",
        &browse_args(object_id, flag, start, count),
    )?;
    page_from(&response)
}

/// Browse every child of `object_id`, following the paging counters. Stops
/// when `TotalMatches` is reached or a page makes no progress (a container
/// that shrinks mid-walk must not loop forever).
pub fn browse_all<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    object_id: &str,
) -> Result<Vec<DidlObject>, ProtoError> {
    let mut objects = Vec::new();
    loop {
        let start = u32::try_from(objects.len())
            .map_err(|_| ProtoError::Malformed("container exceeds u32 objects".into()))?;
        let page = browse(
            transport,
            host,
            object_id,
            BrowseFlag::DirectChildren,
            start,
            PAGE_SIZE,
        )?;
        let got = page.objects.len();
        objects.extend(page.objects);
        if got == 0 || objects.len() >= page.total_matches as usize {
            return Ok(objects);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_args_in_wire_order() {
        assert_eq!(
            browse_args(FAVORITES, BrowseFlag::DirectChildren, 0, 100),
            "<ObjectID>FV:2</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>\
             <Filter>*</Filter><StartingIndex>0</StartingIndex>\
             <RequestedCount>100</RequestedCount><SortCriteria></SortCriteria>"
        );
        assert!(browse_args(QUEUE, BrowseFlag::Metadata, 5, 1).contains("BrowseMetadata"));
    }

    #[test]
    fn non_numeric_counter_is_malformed() {
        let body = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                    <u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
                    <Result></Result><NumberReturned>x</NumberReturned><TotalMatches>0</TotalMatches>\
                    <UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>";
        assert!(matches!(
            parse_browse_response(body),
            Err(ProtoError::Malformed(m)) if m.contains("NumberReturned")
        ));
    }
}
