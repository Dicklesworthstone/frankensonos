//! Exercise the actual callback listener and event pump, not only read-worker
//! scheduling. Unknown SIDs below are synthetic traffic on localhost only.

use super::{Fixture, WAIT, wait_for};
use crate::events::{Service, Want};
use crate::live::pump;
use fsonos_proto::control::{seek_position, set_volume};
use fsonos_proto::net::EventSink;
use fsonos_sim::GenaEvent;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn listen(fixture: &mut Fixture, services: &[(usize, Service)], now: Instant) {
    let host = fixture.players[0].1;
    let local = fixture.engine.lan.local_address_toward(host).unwrap();
    let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
    let wants: Vec<_> = services
        .iter()
        .map(|(index, service)| Want {
            player: fixture.players[*index].0.clone(),
            host: fixture.players[*index].1,
            service: *service,
        })
        .collect();
    let report = fixture.model.rec.subscriptions.sync(
        &*fixture.engine.lan,
        &wants,
        |service| sink.callback_url(service.tag()),
        now,
    );
    assert_eq!(report.subscribed, wants.len());
    assert!(report.failed.is_empty());
    fixture.model.sink = Some(sink);
    wait_for(|| {
        fixture
            .sim
            .gena_log()
            .iter()
            .filter(|entry| {
                matches!(entry.event, GenaEvent::Delivered { seq: 0, status: 200, .. })
            })
            .count()
            == wants.len()
    });
    while let Some(pushed) = fixture.engine.next_event(&mut fixture.model, Duration::ZERO) {
        fixture.engine.publish(&fixture.model, pushed);
    }
}

/// Valid callback envelopes for an unknown subscription occupy intake but
/// cannot alter the playback baseline. A real seek waits behind this burst.
fn queue_unrouted_event(address: SocketAddr, sequence: u32) {
    let body = "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
                <e:property><Unused>ignored</Unused></e:property></e:propertyset>";
    let mut stream = TcpStream::connect_timeout(&address, WAIT).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    write!(
        stream,
        "NOTIFY /AVTransport HTTP/1.1\r\nHost: {address}\r\n\
         NT: upnp:event\r\nNTS: upnp:propchange\r\n\
         SID: uuid:discarded-for-backlog-test\r\nSEQ: {sequence}\r\n\
         Content-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    )
    .unwrap();
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status).unwrap();
    assert!(
        status.contains(" 200 "),
        "callback refused test event: {status}"
    );
}

#[test]
fn a_finished_sample_cannot_overtake_a_seek_behind_the_callback_burst() {
    for generation in [1, 2] {
        let mut fixture = Fixture::new(generation, false);
        listen(&mut fixture, &[(0, Service::AvTransport)], Instant::now());
        let player = fixture.players[0].0.clone();
        fixture.launch(0);
        fixture.release();
        wait_for(|| fixture.model.progress.reads.pending[0].thread.is_finished());
        let before = fixture.model.playback.of(&player).unwrap().clone();
        let address = fixture.model.sink.as_ref().unwrap().local_addr();
        // Exceed the pump's initial 256-event budget and its blocking receive.
        for sequence in 0..320 {
            queue_unrouted_event(address, sequence);
        }
        seek_position(&*fixture.engine.lan, fixture.players[0].1, 17).unwrap();
        wait_for(|| {
            fixture.sim.gena_log().iter().any(|entry| {
                matches!(entry.event, GenaEvent::Delivered { seq: 1, status: 200, .. })
            })
        });
        pump::turn(&fixture.engine, &mut fixture.model);
        assert_eq!(fixture.model.playback.of(&player), Some(&before));
        assert!(fixture.model.progress.reads.contains(&player));
        let calls = fixture.sim.soap_log().len();
        wait_for(|| {
            pump::turn(&fixture.engine, &mut fixture.model);
            fixture
                .model
                .playback
                .of(&player)
                .unwrap()
                .position_at(Instant::now())
                == Some(17)
        });
        assert!(
            fixture.sim.soap_log()[calls..]
                .iter()
                .all(|call| call.action.starts_with("Get")),
            "catching up with events must not replay a seek or change playback"
        );
        fixture.finish();
    }
}

#[test]
fn live_notifications_and_due_renewals_continue_beside_a_blocked_read() {
    for generation in [1, 2] {
        let mut fixture = Fixture::new(generation, false);
        // Only the client's renewal deadline is old; the simulator's actual
        // lease remains live. No wall-clock sleep through a lease is needed.
        let due = Instant::now()
            .checked_sub(Duration::from_secs(1000))
            .unwrap();
        listen(
            &mut fixture,
            &[(0, Service::AvTransport), (1, Service::RenderingControl)],
            due,
        );
        fixture.launch(0);
        wait_for(|| fixture.transport.gate.entered.load(Ordering::Acquire) == 1);
        set_volume(&*fixture.engine.lan, fixture.players[1].1, 29).unwrap();
        wait_for(|| {
            fixture.sim.gena_log().iter().any(|entry| {
                matches!(entry.event, GenaEvent::Delivered { seq: 1, status: 200, .. })
            })
        });
        let started = Instant::now();
        pump::turn(&fixture.engine, &mut fixture.model);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            fixture.model.playback.of(&fixture.players[1].0).unwrap().volume,
            Some(29)
        );
        assert!(fixture.model.progress.reads.contains(&fixture.players[0].0));
        assert!(!fixture.transport.gate.released.load(Ordering::Acquire));
        assert_eq!(
            fixture
                .sim
                .gena_log()
                .iter()
                .filter(|entry| matches!(entry.event, GenaEvent::Renewed { .. }))
                .count(),
            2
        );
        fixture.finish();
    }
}
