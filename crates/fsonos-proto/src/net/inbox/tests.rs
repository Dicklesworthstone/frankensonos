use super::*;
use crate::net::EventSink;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};

const WAIT: Duration = Duration::from_secs(5);

fn notify(seq: u32) -> Notify {
    Notify {
        sid: "uuid:inbox-test".into(),
        seq,
        path: "/RenderingControl".into(),
        properties: vec![("Volume".into(), "17".into())],
    }
}

#[test]
fn overflow_keeps_the_prefix_then_reports_loss_without_another_notification() {
    let (send, receive) = channel(2, EVENT_BACKLOG_BYTES);
    assert_eq!(send.send(notify(0)), Ok(()));
    assert_eq!(send.send(notify(1)), Ok(()));
    assert_eq!(send.send(notify(2)), Err(SendError::Full));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::Notify(notify(0))));
    // Even with one free slot, do not let newer events cross the loss marker.
    for seq in 3..1003 {
        assert_eq!(send.send(notify(seq)), Err(SendError::Full));
    }
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::Notify(notify(1))));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::ResyncRequired));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Err(RecvTimeoutError::Timeout));
    assert_eq!(receive.0.state.lock().unwrap().bytes, 0);
    assert_eq!(send.send(notify(1003)), Ok(()));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::Notify(notify(1003))));
}

#[test]
fn byte_limits_count_allocations_and_space_is_reclaimed_after_receive() {
    let n = notify(0);
    let bytes = storage(&n);
    let (send, receive) = channel(EVENT_BACKLOG, bytes);
    send.send(n).unwrap();
    assert_eq!(receive.0.state.lock().unwrap().bytes, bytes);
    assert_eq!(send.send(notify(1)), Err(SendError::Full));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::Notify(notify(0))));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::ResyncRequired));
    send.send(notify(2)).unwrap();
    let _ = receive.recv_timeout(Duration::ZERO).unwrap();
    // A tiny string backed by a large allocation must not evade the cap.
    let mut large = notify(3);
    large.properties[0].1.reserve(bytes * 2);
    assert_eq!(send.send(large), Err(SendError::Full));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::ResyncRequired));
    assert_eq!(receive.0.state.lock().unwrap().bytes, 0);
}

#[test]
fn oversize_only_loss_and_last_sender_drop_wake_a_waiting_receiver() {
    for close in [false, true] {
        let (send, receive) = channel(1, 0);
        let other = send.clone();
        drop(send);
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| receive.recv_timeout(WAIT));
            if close {
                drop(other);
                assert_eq!(waiting.join().unwrap(), Err(RecvTimeoutError::Disconnected));
            } else {
                assert_eq!(other.send(notify(0)), Err(SendError::Full));
                assert_eq!(waiting.join().unwrap(), Ok(CallbackEvent::ResyncRequired));
            }
        });
    }
}

#[test]
fn closure_drains_prefix_and_loss_before_reporting_disconnection() {
    let (send, receive) = channel(1, EVENT_BACKLOG_BYTES);
    send.send(notify(0)).unwrap();
    assert_eq!(send.send(notify(1)), Err(SendError::Full));
    drop(send);
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::Notify(notify(0))));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::ResyncRequired));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Err(RecvTimeoutError::Disconnected));
    let (send, receive) = channel(1, EVENT_BACKLOG_BYTES);
    send.send(notify(0)).unwrap();
    drop(receive);
    assert_eq!(send.send(notify(1)), Err(SendError::Closed));
    assert_eq!(send.0.state.lock().unwrap().bytes, 0);
}

#[test]
fn concurrent_producers_share_one_count_and_byte_budget() {
    let (send, receive) = channel(32, EVENT_BACKLOG_BYTES);
    std::thread::scope(|scope| {
        for lane in 0..8 {
            let producer = send.clone();
            scope.spawn(move || {
                for seq in 0..100 {
                    let _ = producer.send(notify(lane * 100 + seq));
                }
            });
        }
    });
    let state = receive.0.state.lock().unwrap();
    assert_eq!(state.queue.len(), 32);
    assert!(state.bytes <= EVENT_BACKLOG_BYTES);
    drop(state);
    for _ in 0..32 {
        assert!(matches!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::Notify(_))));
    }
    assert_eq!(receive.recv_timeout(Duration::ZERO), Ok(CallbackEvent::ResyncRequired));
    assert_eq!(receive.recv_timeout(Duration::ZERO), Err(RecvTimeoutError::Timeout));
}

fn post(addr: SocketAddr, seq: u32) -> String {
    let body = "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\"><e:property><Volume>17</Volume></e:property></e:propertyset>";
    let mut stream = TcpStream::connect_timeout(&addr, WAIT).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    write!(
        stream,
        "NOTIFY /RenderingControl HTTP/1.1\r\nHost: {addr}\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: uuid:inbox-test\r\nSEQ: {seq}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status).unwrap();
    status
}

#[test]
fn real_http_overload_is_refused_and_the_open_listener_delivers_a_loss_marker() {
    let sink = EventSink::start_with_limits(
        "127.0.0.1:0".parse().unwrap(),
        None,
        2,
        EVENT_BACKLOG_BYTES,
    )
    .unwrap();
    for seq in 0..2 {
        assert!(post(sink.local_addr(), seq).contains(" 200 "));
    }
    assert!(post(sink.local_addr(), 2).contains(" 503 "));
    for seq in 0..2 {
        let event = sink.recv_event(Duration::ZERO).unwrap().unwrap();
        assert_eq!(event, CallbackEvent::Notify(notify(seq)));
    }
    assert_eq!(sink.recv_event(Duration::ZERO).unwrap(), Some(CallbackEvent::ResyncRequired));
    assert!(sink.recv_event(Duration::ZERO).unwrap().is_none());
    assert!(post(sink.local_addr(), 3).contains(" 200 "));
    assert_eq!(sink.recv_or_closed(Duration::ZERO).unwrap(), Some(notify(3)));
}

#[test]
fn legacy_receivers_get_an_error_for_loss_rather_than_a_silent_empty_result() {
    let sink = EventSink::start_with_limits(
        "127.0.0.1:0".parse().unwrap(),
        None,
        0,
        EVENT_BACKLOG_BYTES,
    )
    .unwrap();
    assert!(post(sink.local_addr(), 0).contains(" 503 "));
    assert!(sink.recv_or_closed(Duration::ZERO).unwrap_err().to_string().contains("overflow"));
    assert!(sink.recv_or_closed(Duration::ZERO).unwrap().is_none());
}
