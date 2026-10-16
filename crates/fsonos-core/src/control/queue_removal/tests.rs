use super::*;
use fsonos_proto::didl::xml_escape;
use std::cell::RefCell;
use std::collections::VecDeque;

fn snapshot(revision: u32, uris: &[&str]) -> Snapshot {
    Snapshot {
        revision,
        uris: uris.iter().map(|uri| (*uri).to_string()).collect(),
    }
}

fn host() -> IpAddr {
    "127.0.0.1".parse().unwrap()
}

fn owner() -> PlayerId {
    PlayerId("RINCON_SYNTHETIC_A".into())
}

fn response(action: &str, args: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <s:Body><u:{action}Response xmlns:u=\"urn:x\">{args}\
         </u:{action}Response></s:Body></s:Envelope>"
    )
}

fn topology(current: bool) -> String {
    let id = if current {
        "RINCON_SYNTHETIC_A"
    } else {
        "RINCON_SYNTHETIC_B"
    };
    let xml = format!(
        "<ZoneGroups><ZoneGroup Coordinator=\"{id}\" ID=\"test:1\">\
         <ZoneGroupMember UUID=\"{id}\" ZoneName=\"Synthetic\" \
         Location=\"http://127.0.0.1:1400/xml/device_description.xml\"/>\
         </ZoneGroup></ZoneGroups>"
    );
    response(
        "GetZoneGroupState",
        &format!("<ZoneGroupState>{}</ZoneGroupState>", xml_escape(&xml)),
    )
}

fn browse(revision: u32, uris: &[&str]) -> String {
    let items = uris.iter().enumerate().map(|(index, uri)| {
        format!(
            "<item id=\"Q:0/{index}\" parentID=\"Q:0\" restricted=\"true\">\
             <dc:title>Track {index}</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>\
             <res protocolInfo=\"test:*:audio/flac:*\">{}</res></item>", xml_escape(uri)
        )
    }).collect::<String>();
    let didl = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{items}</DIDL-Lite>"
    );
    response(
        "Browse",
        &format!(
            "<Result>{}</Result><NumberReturned>{}</NumberReturned>\
         <TotalMatches>{}</TotalMatches><UpdateID>{revision}</UpdateID>",
            xml_escape(&didl),
            uris.len(),
            uris.len()
        ),
    )
}

fn lost() -> ProtoError {
    ProtoError::Network {
        target: "synthetic-player".into(),
        detail: "reply lost".into(),
    }
}

type Reply = (&'static str, Result<String, ProtoError>);

struct Wire {
    replies: RefCell<VecDeque<Reply>>,
    calls: RefCell<Vec<(String, String)>>,
}

impl Wire {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: RefCell::new(replies.into_iter().collect()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn writes(&self) -> usize {
        self.calls
            .borrow()
            .iter()
            .filter(|(name, _)| name == "RemoveTrackRangeFromQueue")
            .count()
    }

    fn finished(&self) {
        assert!(
            self.replies.borrow().is_empty(),
            "some expected observations did not run"
        );
    }
}

impl Transport for Wire {
    fn soap_post(
        &self,
        _: IpAddr,
        _: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push((action.into(), body.into()));
        let (expected, result) = self
            .replies
            .borrow_mut()
            .pop_front()
            .expect("no unexpected I/O");
        assert_eq!(action, expected);
        result
    }
}

fn successful_tail(reply: Result<String, ProtoError>) -> Wire {
    Wire::new([
        ("GetZoneGroupState", Ok(topology(true))),
        ("RemoveTrackRangeFromQueue", reply),
        ("Browse", Ok(browse(8, &["a", "d"]))),
        ("GetZoneGroupState", Ok(topology(true))),
    ])
}

#[test]
fn exact_deletion_checks_both_sides_and_never_panics_on_invalid_ranges() {
    let before = snapshot(7, &["a", "b", "c", "d"]);
    assert!(removed(&before, &snapshot(8, &["a", "d"]), 1, 3));
    assert!(removed(&before, &snapshot(8, &[]), 0, 4));
    for after in [
        snapshot(7, &["a", "d"]),
        snapshot(8, &["x", "d"]),
        snapshot(8, &["a", "x"]),
        snapshot(8, &["a"]),
        snapshot(8, &["a", "d", "x"]),
    ] {
        assert!(!removed(&before, &after, 1, 3));
    }
    for (start, end) in [(0, 0), (3, 2), (0, 5), (usize::MAX, usize::MAX)] {
        assert!(!removed(&before, &snapshot(8, &[]), start, end));
    }
}

#[test]
fn duplicate_uris_and_revision_wrap_are_valid_observations() {
    let before = snapshot(u32::MAX, &["x", "x", "x"]);
    assert!(removed(&before, &snapshot(0, &["x", "x"]), 1, 2));
    assert!(!removed(&before, &snapshot(0, &["x"]), 1, 2));
}

#[test]
fn sends_the_observed_revision_and_reads_the_entire_result() {
    let wire = successful_tail(Ok(response("RemoveTrackRangeFromQueue", "")));
    let done = remove(
        &wire,
        host(),
        &owner(),
        &snapshot(7, &["a", "b", "c", "d"]),
        2,
        2,
    )
    .unwrap();
    assert_eq!(
        done,
        Removed {
            remaining: 2,
            recovered: false
        }
    );
    let calls = wire.calls.borrow();
    let write = &calls[1].1;
    assert!(write.contains("<UpdateID>7</UpdateID>"));
    assert!(write.contains("<StartingIndex>2</StartingIndex>"));
    assert!(write.contains("<NumberOfTracks>2</NumberOfTracks>"));
    assert!(calls[2].1.contains("<StartingIndex>0</StartingIndex>"));
    assert_eq!(wire.writes(), 1);
    wire.finished();
}

#[test]
fn lost_and_malformed_replies_recover_without_repeating_deletion() {
    for error in [
        lost(),
        ProtoError::Malformed("unreadable acknowledgement".into()),
    ] {
        let wire = successful_tail(Err(error));
        let done = remove(
            &wire,
            host(),
            &owner(),
            &snapshot(7, &["a", "b", "c", "d"]),
            2,
            2,
        )
        .unwrap();
        assert_eq!(
            done,
            Removed {
                remaining: 2,
                recovered: true
            }
        );
        assert_eq!(wire.writes(), 1);
        wire.finished();
    }
}

#[test]
fn explicit_faults_remain_faults_without_readback_inference() {
    for code in [701, 800, 1000] {
        let wire = Wire::new([
            ("GetZoneGroupState", Ok(topology(true))),
            (
                "RemoveTrackRangeFromQueue",
                Err(ProtoError::SoapFault {
                    code,
                    reason: "refused".into(),
                }),
            ),
        ]);
        assert!(matches!(
            remove(&wire, host(), &owner(), &snapshot(7, &["a"]), 1, 1),
            Err(RemovalError::Refused(ProtoError::SoapFault { code: actual, .. })) if actual == code
        ));
        assert_eq!(wire.writes(), 1);
        wire.finished();
    }
}

#[test]
fn zero_revision_bad_identities_and_ranges_do_no_io() {
    let wire = Wire::new([]);
    for before in [
        snapshot(0, &["a"]),
        snapshot(7, &[""]),
        snapshot(7, &["NOT_IMPLEMENTED"]),
    ] {
        assert!(matches!(
            remove(&wire, host(), &owner(), &before, 1, 1),
            Err(RemovalError::Invalid(_))
        ));
    }
    for (first, count) in [
        (0, 1),
        (1, 0),
        (u32::MAX, 2),
        (2, 1),
        (1, MAX_BROWSE_OBJECTS + 1),
    ] {
        assert!(matches!(
            remove(&wire, host(), &owner(), &snapshot(7, &["a"]), first, count),
            Err(RemovalError::Invalid(_))
        ));
    }
    assert!(wire.calls.borrow().is_empty());
}

#[test]
fn changed_coordinator_before_dispatch_never_writes() {
    let wire = Wire::new([("GetZoneGroupState", Ok(topology(false)))]);
    assert!(matches!(
        remove(&wire, host(), &owner(), &snapshot(7, &["a"]), 1, 1),
        Err(RemovalError::Invalid(_))
    ));
    assert_eq!(wire.writes(), 0);
    wire.finished();
}

#[test]
fn matching_contents_on_a_changed_coordinator_do_not_confirm_success() {
    let wire = Wire::new([
        ("GetZoneGroupState", Ok(topology(true))),
        ("RemoveTrackRangeFromQueue", Err(lost())),
        ("Browse", Ok(browse(8, &[]))),
        ("GetZoneGroupState", Ok(topology(false))),
    ]);
    let error = remove(&wire, host(), &owner(), &snapshot(7, &["a"]), 1, 1).unwrap_err();
    assert!(matches!(error, RemovalError::Uncertain(_)));
    assert!(error.to_string().contains("reply lost"));
    assert_eq!(wire.writes(), 1);
    wire.finished();
}

#[test]
fn acknowledged_but_incorrect_changes_are_uncertain() {
    for (revision, uris) in [
        (7, vec!["a", "d"]),
        (8, vec!["a", "d", "other"]),
        (8, vec!["a", "b", "c", "d"]),
    ] {
        let wire = Wire::new([
            ("GetZoneGroupState", Ok(topology(true))),
            (
                "RemoveTrackRangeFromQueue",
                Ok(response("RemoveTrackRangeFromQueue", "")),
            ),
            ("Browse", Ok(browse(revision, &uris))),
        ]);
        assert!(matches!(
            remove(
                &wire,
                host(),
                &owner(),
                &snapshot(7, &["a", "b", "c", "d"]),
                2,
                2
            ),
            Err(RemovalError::Uncertain(_))
        ));
        assert_eq!(wire.writes(), 1);
        wire.finished();
    }
}

#[test]
fn failed_readback_retains_the_original_write_diagnostic() {
    let wire = Wire::new([
        ("GetZoneGroupState", Ok(topology(true))),
        ("RemoveTrackRangeFromQueue", Err(lost())),
        (
            "Browse",
            Err(ProtoError::Malformed("bad observation".into())),
        ),
    ]);
    let error = remove(&wire, host(), &owner(), &snapshot(7, &["a"]), 1, 1).unwrap_err();
    assert!(error.to_string().contains("reply lost"));
    assert!(error.to_string().contains("bad observation"));
    assert_eq!(wire.writes(), 1);
    wire.finished();
}

#[test]
fn undo_reads_and_removes_only_the_recorded_range() {
    let wire = Wire::new([
        ("Browse", Ok(browse(7, &["a", "b", "c", "d"]))),
        ("GetZoneGroupState", Ok(topology(true))),
        ("RemoveTrackRangeFromQueue", Err(lost())),
        ("Browse", Ok(browse(8, &["a", "d"]))),
        ("GetZoneGroupState", Ok(topology(true))),
    ]);
    assert!(undo(&wire, host(), &owner(), 2, &["b".into(), "c".into()]).unwrap());
    assert_eq!(wire.writes(), 1);
    wire.finished();
}

#[test]
fn undo_does_not_search_for_a_shifted_or_partially_missing_selection() {
    for uris in [
        vec!["a", "x", "b", "c"],
        vec!["a", "b"],
        vec!["b", "c", "a", "d"],
    ] {
        let wire = Wire::new([("Browse", Ok(browse(7, &uris)))]);
        assert!(!undo(&wire, host(), &owner(), 2, &["b".into(), "c".into()]).unwrap());
        assert_eq!(wire.writes(), 0);
        wire.finished();
    }
}

#[test]
fn undo_invalid_input_is_rejected_before_browsing() {
    let wire = Wire::new([]);
    for (first, uris) in [
        (0, vec!["a".into()]),
        (1, vec!["NOT_IMPLEMENTED".into()]),
        (u32::MAX, vec!["a".into(), "b".into()]),
    ] {
        assert!(undo(&wire, host(), &owner(), first, &uris).is_err());
    }
    assert!(wire.calls.borrow().is_empty());
}

#[test]
fn read_rejects_unavailable_track_identities() {
    for uri in ["", " ", "NOT_IMPLEMENTED"] {
        let wire = Wire::new([("Browse", Ok(browse(7, &[uri])))]);
        assert!(read(&wire, host()).is_err());
        wire.finished();
    }
}
