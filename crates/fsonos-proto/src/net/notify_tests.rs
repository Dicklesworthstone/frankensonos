//! Real callback HTTP admission: only explicit, unambiguous event keys enter
//! the state stream. Invalid input cannot masquerade as the initial event.

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

const WAIT: Duration = Duration::from_secs(5);
const BODY: &[u8] = b"<e:propertyset xmlns:e='urn:schemas-upnp-org:event-1-0'>\
    <e:property><Volume>17</Volume></e:property></e:propertyset>";
const HEADERS: [(&str, &str); 4] = [
    ("SID", "uuid:callback-test"),
    ("NT", "upnp:event"),
    ("NTS", "upnp:propchange"),
    ("SEQ", "1"),
];

fn post(sink: &EventSink, headers: &[(&str, &str)], body: &[u8]) -> u16 {
    let addr = sink.local_addr();
    let mut stream = TcpStream::connect_timeout(&addr, WAIT).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    write!(
        stream,
        "NOTIFY /RenderingControl HTTP/1.1\r\nHost: {addr}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    )
    .unwrap();
    for (key, value) in headers {
        write!(stream, "{key}: {value}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    stream.write_all(body).unwrap();
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status).unwrap();
    status.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn sink() -> EventSink {
    EventSink::start("127.0.0.1:0".parse().unwrap()).unwrap()
}

#[test]
fn only_explicit_decimal_event_keys_are_admitted() {
    let sink = sink();
    for sequence in ["", "-1", "+0", "1.0", "0x0", "1 2", "4294967296", "unknown"] {
        let mut headers = HEADERS;
        headers[3].1 = sequence;
        assert_eq!(post(&sink, &headers, BODY), 400, "SEQ: {sequence:?}");
    }
    assert_eq!(post(&sink, &HEADERS[..3], BODY), 400, "missing SEQ");
    assert!(sink.recv_event(Duration::ZERO).unwrap().is_none());

    // The first real delta keeps its key. Downstream subscriptions can see
    // that the initial full-state notification was missed and recover it.
    assert_eq!(post(&sink, &HEADERS, BODY), 200);
    let event = sink.recv_timeout(WAIT).unwrap();
    assert_eq!(event.seq, 1);
    assert_eq!(event.sid, "uuid:callback-test");
    assert_eq!(event.property("Volume"), Some("17"));
}

#[test]
fn initial_wrap_boundary_and_mixed_case_header_names_remain_compatible() {
    let sink = sink();
    for (sequence, expected) in [("0", 0), ("4294967295", u32::MAX), ("1", 1)] {
        let headers = [
            ("sId", " uuid:callback-test "),
            ("nt", "upnp:event"),
            ("nTs", "upnp:propchange"),
            ("seq", sequence),
        ];
        assert_eq!(post(&sink, &headers, BODY), 200);
        let event = sink.recv_timeout(WAIT).unwrap();
        assert_eq!(event.seq, expected);
        assert_eq!(event.sid, "uuid:callback-test");
        assert_eq!(event.path, "/RenderingControl");
    }
}

#[test]
fn required_event_headers_have_distinct_missing_and_invalid_responses() {
    let sink = sink();
    for (index, absent, invalid) in [(0, 412, 412), (1, 400, 412), (2, 400, 412)] {
        let mut headers = HEADERS.to_vec();
        let name = headers.remove(index).0;
        assert_eq!(post(&sink, &headers, BODY), absent, "missing {name}");
        for value in ["", "wrong:event"] {
            let mut headers = HEADERS;
            headers[index].1 = value;
            // The subscription registry, not the sink, resolves nonempty SIDs.
            if index == 0 && !value.is_empty() {
                continue;
            }
            assert_eq!(post(&sink, &headers, BODY), invalid, "{name}: {value:?}");
        }
    }
    assert!(sink.recv_event(Duration::ZERO).unwrap().is_none());
}

#[test]
fn duplicate_identity_type_or_sequence_headers_cannot_choose_an_event() {
    let sink = sink();
    for (name, value) in HEADERS {
        let lowercase = name.to_ascii_lowercase();
        for repeated in [value, "0"] {
            let mut headers = HEADERS.to_vec();
            headers.push((&lowercase, repeated));
            assert_eq!(post(&sink, &headers, BODY), 400, "duplicate {name}");
        }
    }
    assert!(sink.recv_event(Duration::ZERO).unwrap().is_none());
    assert_eq!(post(&sink, &HEADERS, BODY), 200);
    assert_eq!(sink.recv_timeout(WAIT).unwrap().seq, 1);
}

#[test]
fn invalid_utf8_is_rejected_instead_of_rewriting_event_values() {
    let sink = sink();
    let body =
        b"<propertyset><property><ZoneGroupState>\xff</ZoneGroupState></property></propertyset>";
    assert_eq!(post(&sink, &HEADERS, body), 400);
    assert!(sink.recv_event(Duration::ZERO).unwrap().is_none());
    assert_eq!(post(&sink, &HEADERS, BODY), 200);
    assert_eq!(
        sink.recv_timeout(WAIT).unwrap().property("Volume"),
        Some("17")
    );
}
