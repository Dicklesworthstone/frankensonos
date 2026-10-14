//! Discovery of the virtual players without multicast: a unicast SSDP
//! responder on `127.0.0.1:<port>` that answers an `M-SEARCH` for
//! ZonePlayers (or `ssdp:all`) the way real players answer the multicast
//! one, one datagram per powered-on player, with each player's advertised
//! LOCATION. Loopback unicast behaves the same on macOS and Linux.

use crate::SimModel;
use crate::model::State;
use fsonos_proto::ssdp::SONOS_ST;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// A running responder.
#[derive(Debug)]
pub(crate) struct Responder {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Responder {
    pub(crate) fn start(state: Arc<Mutex<State>>) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        let addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("fsonos-sim-ssdp".into())
            .spawn(move || {
                let mut buf = [0u8; 2048];
                while !stopping.load(Ordering::SeqCst) {
                    let Ok((n, from)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    if !is_zoneplayer_search(&buf[..n]) {
                        continue;
                    }
                    let replies = state.lock().map(|s| responses(&s)).unwrap_or_default();
                    for reply in replies {
                        let _ = socket.send_to(reply.as_bytes(), from);
                    }
                }
            })?;
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Whether `datagram` is an `M-SEARCH` a ZonePlayer answers.
fn is_zoneplayer_search(datagram: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(datagram) else {
        return false;
    };
    if !text.starts_with("M-SEARCH * HTTP/1.1") {
        return false;
    }
    text.lines().any(|line| {
        line.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("ST") && matches!(v.trim(), SONOS_ST | "ssdp:all")
        })
    })
}

/// The `SERVER` model token real players send (`ZPS5`, `BR100`, ...).
fn server_token(model: SimModel) -> &'static str {
    match model {
        SimModel::Play5Gen1 => "ZPS5",
        SimModel::Bridge => "BR100",
        SimModel::One => "ZPS13",
        SimModel::Play1 => "ZPS1",
    }
}

/// One response datagram per powered-on player, shaped like a real reply.
pub(crate) fn responses(state: &State) -> Vec<String> {
    state
        .players
        .iter()
        .filter(|p| !p.offline)
        .map(|p| {
            let household = &state.households[p.household];
            format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age = 1800\r\nEXT:\r\n\
                 LOCATION: {location}\r\nSERVER: Linux UPnP/1.0 Sonos/{version} ({token})\r\n\
                 ST: {SONOS_ST}\r\nUSN: uuid:{uuid}::{SONOS_ST}\r\n\
                 X-RINCON-HOUSEHOLD: {household}\r\nX-RINCON-BOOTSEQ: {boot}\r\n\
                 BOOTID.UPNP.ORG: {boot}\r\n\r\n",
                location = p.location(),
                version = crate::docs::software_version(household.sw_gen),
                token = server_token(p.model),
                uuid = p.uuid,
                household = household.id,
                boot = p.boot_seq,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_zoneplayer_and_all_searches_only() {
        assert!(is_zoneplayer_search(
            fsonos_proto::ssdp::m_search(1).as_bytes()
        ));
        let all = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: ssdp:all\r\n\r\n";
        assert!(is_zoneplayer_search(all.as_bytes()));
        let other =
            "M-SEARCH * HTTP/1.1\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert!(!is_zoneplayer_search(other.as_bytes()));
        assert!(!is_zoneplayer_search(
            b"NOTIFY * HTTP/1.1\r\nST: ssdp:all\r\n\r\n"
        ));
    }
}
