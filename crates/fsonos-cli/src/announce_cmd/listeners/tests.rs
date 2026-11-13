use super::*;
use fsonos_core::announce::clip::Chime;
use fsonos_types::{Generation, Player};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn household() -> HouseholdState {
    HouseholdState {
        players: (1..=3)
            .map(|n| Player {
                id: PlayerId(format!("RINCON_ROUTE_{n}")),
                room_name: format!("Room{n}"),
                ip: format!("192.0.2.{n}").parse().unwrap(),
                model: "test player".into(),
                generation: Generation::S2,
            })
            .collect(),
        ..HouseholdState::default()
    }
}

fn media() -> (tempfile::TempDir, MediaStore, MediaFiles) {
    let dir = tempfile::tempdir().unwrap();
    let store = MediaStore::new(dir.path());
    let files = store.clone();
    (dir, store, Arc::new(move |name| files.path(name)))
}

fn fetch(url: &str) -> Vec<u8> {
    let (authority, path) = url.trim_start_matches("http://").split_once('/').unwrap();
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 200"), "media request failed");
    let body = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap()
        + 4;
    bytes[body..].to_vec()
}

#[test]
fn players_sharing_a_local_route_share_one_live_media_listener() {
    let (_dir, store, files) = media();
    let house = household();
    let listeners = Listeners::start(std::slice::from_ref(&house), &files, |_| {
        Ok("127.0.0.1".parse().unwrap())
    })
    .unwrap();
    assert_eq!(listeners.routes.len(), 3);
    let wav = Chime::Beep.wav();
    let stored = store.put(&wav).unwrap();
    let base = listeners.routes.get(&house.players[0].id).unwrap();
    assert!(listeners.routes.values().all(|value| value == base));
    assert_eq!(fetch(&stored.url(base)), wav);
}

#[test]
fn unrelated_lookup_and_bind_failures_do_not_discard_a_healthy_route() {
    let (_dir, store, files) = media();
    let house = household();
    let listeners = Listeners::start(std::slice::from_ref(&house), &files, |address| {
        if address == house.players[0].ip {
            Err(ProtoError::Network {
                target: "test route".into(),
                detail: "no route".into(),
            })
        } else if address == house.players[1].ip {
            // An address absent from this host cannot supply a listener.
            Ok("203.0.113.250".parse().unwrap())
        } else {
            Ok("127.0.0.1".parse().unwrap())
        }
    })
    .unwrap();
    assert_eq!(listeners.routes.len(), 1);
    assert!(!listeners.routes.contains_key(&house.players[0].id));
    assert!(!listeners.routes.contains_key(&house.players[1].id));
    let wav = Chime::Beep.wav();
    let stored = store.put(&wav).unwrap();
    assert_eq!(
        fetch(&stored.url(listeners.routes.get(&house.players[2].id).unwrap())),
        wav
    );
}

#[cfg(target_os = "linux")]
#[test]
fn distinct_local_addresses_serve_the_same_clip_from_distinct_listeners() {
    let (_dir, store, files) = media();
    let house = household();
    let listeners = Listeners::start(std::slice::from_ref(&house), &files, |address| {
        Ok(if address == house.players[0].ip {
            "127.0.0.1"
        } else {
            "127.0.0.2"
        }
        .parse()
        .unwrap())
    })
    .unwrap();
    let first = listeners.routes.get(&house.players[0].id).unwrap();
    let second = listeners.routes.get(&house.players[1].id).unwrap();
    assert!(first.starts_with("http://127.0.0.1:"));
    assert!(second.starts_with("http://127.0.0.2:"));
    let wav = Chime::Beep.wav();
    let stored = store.put(&wav).unwrap();
    assert_eq!(fetch(&stored.url(first)), wav);
    assert_eq!(fetch(&stored.url(second)), wav);
}

#[test]
fn no_listener_is_an_explicit_readiness_failure() {
    let (_dir, _store, files) = media();
    let failure = Listeners::start(&[household()], &files, |_| {
        Err(ProtoError::Network {
            target: "test route".into(),
            detail: "unreachable".into(),
        })
    })
    .err()
    .unwrap();
    assert_eq!(failure.code, ErrorCode::NotReady);
    assert!(
        failure
            .detail
            .contains("no announcement media listener could start")
    );
}
