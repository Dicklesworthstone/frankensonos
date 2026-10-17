use super::*;
use fsonos_proto::didl::parse_didl;

fn selected() -> BrowsePage {
    BrowsePage {
        objects: parse_didl(
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
             <item id=\"Q:0/1\" parentID=\"Q:0\" restricted=\"1\">\
             <dc:title>Selected</dc:title>\
             <res protocolInfo=\"http-get:*:audio/mpeg:*\">test:selected</res>\
             </item></DIDL-Lite>",
        )
        .unwrap(),
        number_returned: 1,
        total_matches: 3,
        update_id: 7,
    }
}

fn request() -> QueuePlayRequest {
    QueuePlayRequest {
        zone: "Room".into(),
        position: 1,
        expected_uri: Some("test:selected".into()),
        expected_revision: None,
    }
}

#[test]
fn revision_checks_coordinator_and_counter_even_when_uri_and_size_match() {
    let id = PlayerId("test:one".into());
    let mut page = selected();
    let mut req = request();
    req.expected_revision = Some(snapshot::revision(&id, &page));
    assert_eq!(req.check(&id, &page).unwrap(), "Selected");
    assert_eq!(
        req.check(&PlayerId("test:two".into()), &page)
            .unwrap_err()
            .code,
        ErrorCode::QueueChanged
    );
    page.update_id += 1;
    assert_eq!(
        req.check(&id, &page).unwrap_err().code,
        ErrorCode::QueueChanged
    );
}

#[test]
fn revision_does_not_disable_the_independent_renderer_identity_check() {
    let id = PlayerId("test:one".into());
    let page = selected();
    let mut req = request();
    req.expected_revision = Some(snapshot::revision(&id, &page));
    req.expected_uri = Some("test:other".into());
    assert_eq!(
        req.check(&id, &page).unwrap_err().code,
        ErrorCode::QueueChanged
    );
}

#[test]
fn old_json_requests_remain_valid_and_new_tokens_round_trip_without_normalization() {
    let old: QueuePlayRequest = serde_json::from_str(r#"{"zone":"Room","position":1}"#).unwrap();
    assert!(old.expected_revision.is_none());
    assert!(old.expected_uri.is_none());
    assert!(old.validate().is_ok());
    let mut req = request();
    req.expected_revision = Some("q1:9:test:room:0:3".into());
    let encoded = serde_json::to_value(&req).unwrap();
    assert_eq!(encoded["expected_revision"], "q1:9:test:room:0:3");
    assert_eq!(
        serde_json::from_value::<QueuePlayRequest>(encoded).unwrap(),
        req
    );
}

#[test]
fn blank_and_excessive_tokens_fail_validation() {
    let mut req = request();
    for token in [String::new(), " \t".into(), "x".repeat(1025)] {
        req.expected_revision = Some(token);
        assert_eq!(req.validate().unwrap_err().code, ErrorCode::InvalidArgument);
    }
    req.expected_revision = Some("x".repeat(1024));
    assert!(req.validate().is_ok());
}
