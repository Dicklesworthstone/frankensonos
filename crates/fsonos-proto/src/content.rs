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

/// Resource bounds for a complete browse, not limits imposed by the protocol.
/// A changing or malformed container must not hold a caller indefinitely.
pub const MAX_BROWSE_OBJECTS: u32 = 100_000;
pub const MAX_BROWSE_PAGES: usize = 4096;
/// A changed container is read again from the beginning, at most three times.
pub const SNAPSHOT_ATTEMPTS: usize = 3;

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

/// Every child of a container at one observed revision. All pages agreed on
/// both UpdateID and TotalMatches; no partial walk is returned as a snapshot.
/// The container can change again after the last response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseSnapshot {
    pub objects: Vec<DidlObject>,
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

/// Read every child of `object_id` from a consistent set of pages. A changed
/// UpdateID or TotalMatches discards the whole attempt and restarts at zero.
/// Short pages are valid; stalled or inconsistent counters are not. Network
/// and parsing failures propagate without publishing a partial snapshot.
/// Work is bounded across all attempts by [`MAX_BROWSE_PAGES`].
pub fn browse_snapshot<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    object_id: &str,
) -> Result<BrowseSnapshot, ProtoError> {
    let mut requests = 0;
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let mut objects = Vec::new();
        let mut revision = None;
        loop {
            if requests == MAX_BROWSE_PAGES {
                return Err(ProtoError::Malformed(format!(
                    "browse of {object_id} exceeded the page limit"
                )));
            }
            requests += 1;
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
            let signature = (page.update_id, page.total_matches);
            if revision.is_some_and(|previous| previous != signature) {
                break;
            }
            revision = Some(signature);
            if page.total_matches > MAX_BROWSE_OBJECTS {
                return Err(ProtoError::Malformed(format!(
                    "browse of {object_id} exceeds the object limit"
                )));
            }
            let got = u32::try_from(page.objects.len())
                .map_err(|_| ProtoError::Malformed("browse page exceeds u32 objects".into()))?;
            let end = start
                .checked_add(got)
                .ok_or_else(|| ProtoError::Malformed("browse position overflow".into()))?;
            if got != page.number_returned || got > PAGE_SIZE || end > page.total_matches {
                return Err(ProtoError::Malformed(format!(
                    "browse of {object_id} returned inconsistent paging counters"
                )));
            }
            if got == 0 && end < page.total_matches {
                return Err(ProtoError::Malformed(format!(
                    "browse of {object_id} made no progress before TotalMatches"
                )));
            }
            objects.extend(page.objects);
            if end == page.total_matches {
                return Ok(BrowseSnapshot {
                    objects,
                    update_id: page.update_id,
                });
            }
        }
    }
    Err(ProtoError::Malformed(format!(
        "browse of {object_id} kept changing across {SNAPSHOT_ATTEMPTS} attempts"
    )))
}

/// Browse every child of `object_id`, with the consistency and resource
/// bounds of [`browse_snapshot`]. Never returns a mixed-revision or partial
/// list: queue ownership and favorite learning need a complete view.
pub fn browse_all<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    object_id: &str,
) -> Result<Vec<DidlObject>, ProtoError> {
    browse_snapshot(transport, host, object_id).map(|snapshot| snapshot.objects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::didl::xml_escape;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::fmt::Write as _;

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

    struct Scripted {
        pages: RefCell<VecDeque<(u32, String)>>,
    }

    impl Scripted {
        fn new(pages: Vec<(u32, String)>) -> Self {
            Self {
                pages: RefCell::new(pages.into()),
            }
        }

        fn complete(&self) {
            assert!(self.pages.borrow().is_empty());
        }
    }

    impl Transport for Scripted {
        fn soap_post(
            &self,
            _: IpAddr,
            _: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            assert!(action.ends_with("#Browse\""));
            let (start, response) = self.pages.borrow_mut().pop_front().expect("bounded walk");
            assert!(body.contains(&format!("<StartingIndex>{start}</StartingIndex>")));
            assert!(body.contains("<RequestedCount>100</RequestedCount>"));
            Ok(response)
        }
    }

    fn page(names: &[&str], total: u32, update: u32) -> String {
        let mut didl = String::from(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\">",
        );
        for name in names {
            let name = xml_escape(name);
            let _ = write!(
                didl,
                "<item id=\"{name}\" parentID=\"Q:0\" restricted=\"1\">\
                 <dc:title>{name}</dc:title></item>"
            );
        }
        didl.push_str("</DIDL-Lite>");
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
             <Result>{}</Result><NumberReturned>{}</NumberReturned>\
             <TotalMatches>{total}</TotalMatches><UpdateID>{update}</UpdateID>\
             </u:BrowseResponse></s:Body></s:Envelope>",
            xml_escape(&didl),
            names.len()
        )
    }

    fn host() -> IpAddr {
        std::net::Ipv4Addr::LOCALHOST.into()
    }

    fn titles(snapshot: &BrowseSnapshot) -> Vec<&str> {
        snapshot.objects.iter().map(|o| o.title.as_str()).collect()
    }

    #[test]
    fn short_pages_form_one_complete_snapshot() {
        let transport = Scripted::new(vec![
            (0, page(&["one", "two"], 3, 9)),
            (2, page(&["three"], 3, 9)),
        ]);
        let snapshot = browse_snapshot(&transport, host(), QUEUE).unwrap();
        assert_eq!(titles(&snapshot), ["one", "two", "three"]);
        assert_eq!(snapshot.update_id, 9);
        transport.complete();
    }

    #[test]
    fn same_length_replacement_restarts_instead_of_mixing_revisions() {
        let transport = Scripted::new(vec![
            (0, page(&["old-one"], 2, 9)),
            (1, page(&["new-two"], 2, 10)),
            (0, page(&["new-one", "new-two"], 2, 10)),
        ]);
        let snapshot = browse_snapshot(&transport, host(), QUEUE).unwrap();
        assert_eq!(titles(&snapshot), ["new-one", "new-two"]);
        assert_eq!(snapshot.update_id, 10);
        transport.complete();
    }

    #[test]
    fn a_shrink_to_empty_discards_every_old_object() {
        let transport = Scripted::new(vec![
            (0, page(&["removed"], 2, 1)),
            (1, page(&[], 0, 2)),
            (0, page(&[], 0, 2)),
        ]);
        assert!(browse_all(&transport, host(), QUEUE).unwrap().is_empty());
        transport.complete();
    }

    #[test]
    fn changed_totals_restart_even_when_the_player_reuses_update_id() {
        let transport = Scripted::new(vec![
            (0, page(&["old"], 2, 0)),
            (1, page(&["two", "three"], 3, 0)),
            (0, page(&["one", "two", "three"], 3, 0)),
        ]);
        let snapshot = browse_snapshot(&transport, host(), FAVORITES).unwrap();
        assert_eq!(titles(&snapshot), ["one", "two", "three"]);
        transport.complete();
    }

    #[test]
    fn a_revision_wrap_is_a_change_not_an_older_page() {
        let transport = Scripted::new(vec![
            (0, page(&["old"], 2, u32::MAX)),
            (1, page(&["two"], 2, 0)),
            (0, page(&["one", "two"], 2, 0)),
        ]);
        let snapshot = browse_snapshot(&transport, host(), QUEUE).unwrap();
        assert_eq!(titles(&snapshot), ["one", "two"]);
        assert_eq!(snapshot.update_id, 0);
        transport.complete();
    }

    #[test]
    fn endless_edits_exhaust_the_retry_budget_without_a_partial_result() {
        let mut pages = Vec::new();
        for update in 0..u32::try_from(SNAPSHOT_ATTEMPTS).unwrap() {
            pages.push((0, page(&["one"], 2, update * 2)));
            pages.push((1, page(&["two"], 2, update * 2 + 1)));
        }
        let transport = Scripted::new(pages);
        let error = browse_all(&transport, host(), QUEUE).unwrap_err();
        assert!(error.to_string().contains("kept changing"));
        transport.complete();
    }

    #[test]
    fn stalled_and_inconsistent_pages_are_not_successful_snapshots() {
        for response in [
            page(&[], 1, 7),
            page(&["one"], 0, 7),
            page(&["one"], 1, 7).replace(
                "<NumberReturned>1</NumberReturned>",
                "<NumberReturned>2</NumberReturned>",
            ),
            page(&["one"], MAX_BROWSE_OBJECTS + 1, 7),
            page(&["one"; 101], 101, 7),
        ] {
            let transport = Scripted::new(vec![(0, response)]);
            assert!(browse_all(&transport, host(), QUEUE).is_err());
            transport.complete();
        }
        let transport = Scripted::new(vec![
            (0, page(&["one"], 2, 7)),
            (1, page(&[], 2, 7)),
        ]);
        assert!(browse_all(&transport, host(), QUEUE).is_err());
        transport.complete();
    }

    #[test]
    fn malformed_later_pages_propagate_without_publishing_a_prefix() {
        let transport = Scripted::new(vec![
            (0, page(&["one"], 2, 7)),
            (1, "<not-xml".into()),
        ]);
        assert!(browse_all(&transport, host(), QUEUE).is_err());
        transport.complete();
    }

    #[test]
    fn tiny_pages_cannot_exceed_the_total_request_budget() {
        let total = u32::try_from(MAX_BROWSE_PAGES + 1).unwrap();
        let pages = (0..MAX_BROWSE_PAGES)
            .map(|start| (u32::try_from(start).unwrap(), page(&["item"], total, 1)))
            .collect();
        let transport = Scripted::new(pages);
        let error = browse_snapshot(&transport, host(), QUEUE).unwrap_err();
        assert!(error.to_string().contains("page limit"));
        transport.complete();
    }
}
