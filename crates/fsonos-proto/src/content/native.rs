//! Opt-in native metadata retention for selections that replay or copy tracks.

use super::{BrowseFlag, PAGE_SIZE, browse_args};
use crate::didl::{NativeDidlObject, parse_didl_with_metadata};
use crate::{ProtoError, Transport, soap};
use std::net::IpAddr;

/// A bounded Browse page whose objects also retain original native DIDL.
/// Ordinary catalog display can continue using the smaller `BrowsePage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBrowsePage {
    pub objects: Vec<NativeDidlObject>,
    pub number_returned: u32,
    pub total_matches: u32,
    pub update_id: u32,
}

/// Parse one response with caller-specified object and retained XML budgets.
/// The budgets apply before constructing per-object metadata documents.
pub fn parse_browse_response_with_metadata(
    body: &str,
    max_objects: u32,
    max_bytes: usize,
) -> Result<NativeBrowsePage, ProtoError> {
    page_from(
        &soap::parse_response(body, "Browse")?,
        max_objects,
        max_bytes,
    )
}

fn page_from(
    response: &soap::SoapResponse,
    max_objects: u32,
    max_bytes: usize,
) -> Result<NativeBrowsePage, ProtoError> {
    let number_returned = response.require_u32("NumberReturned")?;
    if max_objects > PAGE_SIZE || number_returned > max_objects {
        return Err(ProtoError::Malformed(
            "native Browse exceeds the requested page size".into(),
        ));
    }
    let objects = parse_didl_with_metadata(
        response.require("Result")?,
        usize::try_from(max_objects).expect("one native page fits in usize"),
        max_bytes,
    )?;
    if objects.len() != usize::try_from(number_returned).expect("one native page fits in usize") {
        return Err(ProtoError::Malformed(
            "native Browse returned inconsistent object counters".into(),
        ));
    }
    Ok(NativeBrowsePage {
        objects,
        number_returned,
        total_matches: response.require_u32("TotalMatches")?,
        update_id: response.require_u32("UpdateID")?,
    })
}

/// Browse one native page, preserving metadata for each returned occurrence.
/// The requested count must fit one Sonos page. `max_bytes` bounds the sum of
/// self-contained DIDL documents, including each repeated root wrapper.
pub fn browse_with_metadata<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    object_id: &str,
    flag: BrowseFlag,
    start: u32,
    count: u32,
    max_bytes: usize,
) -> Result<NativeBrowsePage, ProtoError> {
    if count == 0 || count > PAGE_SIZE {
        return Err(ProtoError::Malformed(
            "invalid native Browse page size".into(),
        ));
    }
    let response = soap::call(
        transport,
        host,
        &soap::CONTENT_DIRECTORY,
        "Browse",
        &browse_args(object_id, flag, start, count),
    )?;
    page_from(&response, count, max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::didl::xml_escape;

    fn response(count: u32) -> String {
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\">\
             <Result>{}</Result><NumberReturned>{count}</NumberReturned>\
             <TotalMatches>8</TotalMatches><UpdateID>17</UpdateID>\
             </u:BrowseResponse></s:Body></s:Envelope>",
            xml_escape(include_str!("../../tests/fixtures/didl_native_items.xml")),
        )
    }

    #[test]
    fn native_pages_keep_counters_and_original_occurrence_metadata() {
        let page = parse_browse_response_with_metadata(&response(2), 2, 16_384).unwrap();
        assert_eq!(
            (page.number_returned, page.total_matches, page.update_id),
            (2, 8, 17)
        );
        assert_eq!(page.objects[0].object.id, "SQ:7/1");
        assert!(
            page.objects[0]
                .metadata
                .contains("refID=\"service-original-1\"")
        );
        assert!(!page.objects[0].metadata.contains("Other occurrence"));
        assert_eq!(page.objects[1].object.id, "SQ:7/2");
    }

    #[test]
    fn native_page_item_and_byte_budgets_cannot_be_evaded_by_response_counters() {
        assert!(parse_browse_response_with_metadata(&response(2), 1, 16_384).is_err());
        assert!(parse_browse_response_with_metadata(&response(1), 1, 16_384).is_err());
        assert!(parse_browse_response_with_metadata(&response(2), 2, 128).is_err());
        assert!(parse_browse_response_with_metadata(&response(3), 3, 16_384).is_err());
    }
}
