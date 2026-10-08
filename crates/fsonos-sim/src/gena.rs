//! GENA eventing for the virtual players: SUBSCRIBE / renew / UNSUBSCRIBE,
//! a NOTIFY after every change, expiry on the sim clock, and the event log.
//!
//! What each service sends follows the live captures in
//! `fsonos-proto/tests/fixtures/gena_notify_*.xml`: AVTransport sends its
//! whole `LastChange` state every time, RenderingControl sends everything
//! first and then only the variables that changed, and ZoneGroupTopology
//! sends its properties (the full `ZoneGroupState`) on every change. SEQ
//! starts at 0 (the initial, full-state NOTIFY) and increments per NOTIFY,
//! dropped ones included, so a subscriber can see a gap.

use crate::docs;
use crate::model::{Source, State};
use crate::{GenaEvent, GenaLogEntry, NotifyDrop, SimModel};
use fsonos_proto::didl::xml_escape;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Longest subscription a player grants (seconds).
pub(crate) const MAX_TIMEOUT_SECS: u32 = 86_400;
/// Granted when a SUBSCRIBE asks for none (or for `infinite`).
pub(crate) const DEFAULT_TIMEOUT_SECS: u32 = 1_800;

/// A service whose events a virtual player publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventService {
    AvTransport,
    RenderingControl,
    GroupRenderingControl,
    ZoneGroupTopology,
}

impl EventService {
    /// The service behind event URL `path` on a `model` player.
    pub(crate) fn from_path(path: &str, model: SimModel) -> Option<Self> {
        match path {
            "/MediaRenderer/AVTransport/Event" if model.is_renderer() => Some(Self::AvTransport),
            "/MediaRenderer/RenderingControl/Event" if model.is_renderer() => {
                Some(Self::RenderingControl)
            }
            "/MediaRenderer/GroupRenderingControl/Event" if model.is_renderer() => {
                Some(Self::GroupRenderingControl)
            }
            "/ZoneGroupTopology/Event" => Some(Self::ZoneGroupTopology),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::AvTransport => "AVTransport",
            Self::RenderingControl => "RenderingControl",
            Self::GroupRenderingControl => "GroupRenderingControl",
            Self::ZoneGroupTopology => "ZoneGroupTopology",
        }
    }
}

/// One live subscription.
#[derive(Debug, Clone)]
pub(crate) struct Subscription {
    pub sid: String,
    pub player: usize,
    pub service: EventService,
    /// Where NOTIFYs go (`http://host:port/path`).
    pub callback: String,
    /// [`crate::SimClock`] time it lapses unless renewed.
    pub expires_at_ms: u64,
    /// SEQ of the last NOTIFY issued.
    pub seq: u32,
    /// The variables as last notified (`None` before the initial NOTIFY).
    pub sent: Option<Vec<(String, String)>>,
}

/// A NOTIFY waiting to be delivered.
#[derive(Debug, Clone)]
pub(crate) struct Outgoing {
    pub callback: String,
    pub sid: String,
    pub seq: u32,
    pub body: String,
    pub player: String,
    pub service: &'static str,
}

impl State {
    pub(crate) fn log_gena(&mut self, player: usize, service: &str, event: GenaEvent) {
        let entry = GenaLogEntry {
            at_ms: self.clock.now_ms(),
            player: self.players[player].uuid.clone(),
            service: service.to_string(),
            event,
        };
        self.gena_log.push(entry);
    }

    /// Start a subscription and issue its initial NOTIFY. Returns the SID and
    /// the timeout granted.
    pub(crate) fn subscribe(
        &mut self,
        player: usize,
        service: EventService,
        callback: String,
        requested_secs: Option<u32>,
    ) -> (String, u32) {
        let timeout = requested_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        let sid = format!(
            "uuid:{}_sub{:010}",
            self.players[player].uuid, self.next_sid
        );
        self.next_sid += 1;
        self.subscriptions.push(Subscription {
            sid: sid.clone(),
            player,
            service,
            callback: callback.clone(),
            expires_at_ms: self.clock.now_ms() + u64::from(timeout) * 1000,
            seq: 0,
            sent: None,
        });
        self.log_gena(
            player,
            service.name(),
            GenaEvent::Subscribed {
                sid: sid.clone(),
                callback,
                timeout_secs: timeout,
            },
        );
        self.flush_events();
        (sid, timeout)
    }

    /// Extend subscription `sid`; `None` when it is unknown or lapsed.
    pub(crate) fn renew(
        &mut self,
        player: usize,
        sid: &str,
        requested_secs: Option<u32>,
    ) -> Option<u32> {
        self.expire_lapsed();
        let now = self.clock.now_ms();
        let sub = self
            .subscriptions
            .iter_mut()
            .find(|s| s.sid == sid && s.player == player)?;
        let timeout = requested_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        sub.expires_at_ms = now + u64::from(timeout) * 1000;
        let service = sub.service.name();
        self.log_gena(
            player,
            service,
            GenaEvent::Renewed {
                sid: sid.to_string(),
                timeout_secs: timeout,
            },
        );
        Some(timeout)
    }

    /// End subscription `sid`; false when it is unknown or lapsed.
    pub(crate) fn unsubscribe(&mut self, player: usize, sid: &str) -> bool {
        self.expire_lapsed();
        let Some(at) = self
            .subscriptions
            .iter()
            .position(|s| s.sid == sid && s.player == player)
        else {
            return false;
        };
        let sub = self.subscriptions.remove(at);
        self.log_gena(
            player,
            sub.service.name(),
            GenaEvent::Unsubscribed { sid: sub.sid },
        );
        true
    }

    /// Forget every subscription to `player` (a reboot or power-off).
    pub(crate) fn drop_subscriptions(&mut self, player: usize) {
        self.subscriptions.retain(|s| s.player != player);
    }

    fn expire_lapsed(&mut self) {
        let now = self.clock.now_ms();
        let (live, lapsed): (Vec<_>, Vec<_>) = std::mem::take(&mut self.subscriptions)
            .into_iter()
            .partition(|s| s.expires_at_ms > now);
        self.subscriptions = live;
        for sub in lapsed {
            self.log_gena(
                sub.player,
                sub.service.name(),
                GenaEvent::Expired { sid: sub.sid },
            );
        }
    }

    /// Issue a NOTIFY to every subscription whose variables changed since it
    /// was last notified (or that has not had its initial NOTIFY yet).
    pub(crate) fn flush_events(&mut self) {
        self.expire_lapsed();
        for i in 0..self.subscriptions.len() {
            let (player, service) = (self.subscriptions[i].player, self.subscriptions[i].service);
            if self.players[player].offline {
                continue;
            }
            let vars = match service {
                EventService::AvTransport => self.avt_vars(player),
                EventService::RenderingControl => self.rcs_vars(player),
                EventService::GroupRenderingControl => self.grc_props(player),
                EventService::ZoneGroupTopology => self.zgt_props(player),
            };
            let send: Vec<&(String, String)> = match &self.subscriptions[i].sent {
                Some(prev) if service == EventService::RenderingControl => {
                    vars.iter().filter(|v| !prev.contains(v)).collect()
                }
                Some(prev) if *prev == vars => Vec::new(),
                // The initial NOTIFY, or a changed AVTransport/topology state.
                _ => vars.iter().collect(),
            };
            if send.is_empty() {
                continue;
            }
            let body = event_body(service, &send);
            let sub = &mut self.subscriptions[i];
            let seq = if sub.sent.is_some() { sub.seq + 1 } else { 0 };
            sub.seq = seq;
            sub.sent = Some(vars.clone());
            let (sid, callback) = (sub.sid.clone(), sub.callback.clone());
            if self.should_drop(player) {
                self.log_gena(player, service.name(), GenaEvent::Dropped { sid, seq });
                continue;
            }
            self.log_gena(
                player,
                service.name(),
                GenaEvent::Notified {
                    sid: sid.clone(),
                    seq,
                },
            );
            let out = Outgoing {
                callback,
                sid,
                seq,
                body,
                player: self.players[player].uuid.clone(),
                service: service.name(),
            };
            if let Some(notifier) = &self.notifier {
                let _ = notifier.send(out);
            }
        }
    }

    fn should_drop(&mut self, player: usize) -> bool {
        match self.players[player].faults.drop_notifies {
            None => false,
            Some(NotifyDrop::Next(n)) => {
                self.players[player].faults.drop_notifies =
                    (n > 1).then(|| NotifyDrop::Next(n - 1));
                n > 0
            }
            Some(NotifyDrop::Probability(p)) => {
                // xorshift64: deterministic for a given sequence of events.
                let mut x = self.rng;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.rng = x;
                #[allow(clippy::cast_precision_loss)] // a uniform draw in [0, 1)
                let draw = (x >> 11) as f64 / (1u64 << 53) as f64;
                draw < p
            }
        }
    }

    /// AVTransport `LastChange` variables of `p` (a member reports its
    /// coordinator's transport).
    fn avt_vars(&self, p: usize) -> Vec<(String, String)> {
        let c = self.players[p].coordinator;
        let t = &self.players[c].transport;
        let (track_uri, track_md) = match (t.source, t.current()) {
            (Source::Queue, Some(q)) => (q.uri.clone(), docs::with_art(&q.uri, &q.metadata)),
            (Source::Uri, _) => (t.uri.clone(), t.uri_metadata.clone()),
            _ => (String::new(), String::new()),
        };
        let (av_uri, av_md) = if c == p {
            (t.uri.clone(), t.uri_metadata.clone())
        } else {
            (format!("x-rincon:{}", self.players[c].uuid), String::new())
        };
        let tracks = match t.source {
            Source::Queue => t.queue.len(),
            Source::Uri => 1,
            Source::Nothing => 0,
        };
        [
            ("TransportState", t.state.as_str().to_string()),
            ("CurrentPlayMode", "NORMAL".to_string()),
            ("NumberOfTracks", tracks.to_string()),
            ("CurrentTrack", t.track.to_string()),
            ("CurrentSection", "0".to_string()),
            ("CurrentTrackURI", track_uri),
            ("CurrentTrackDuration", docs::hms(t.duration_ms())),
            ("CurrentTrackMetaData", track_md),
            ("AVTransportURI", av_uri),
            ("AVTransportURIMetaData", av_md),
        ]
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                format!("<{name} val=\"{}\"/>", xml_escape(&value)),
            )
        })
        .collect()
    }

    /// RenderingControl `LastChange` variables of `p`, grouped as Sonos sends
    /// them (every channel of a changed variable together).
    fn rcs_vars(&self, p: usize) -> Vec<(String, String)> {
        let player = &self.players[p];
        let channels = |name: &str, master: String, side: &str| {
            format!(
                "<{name} channel=\"Master\" val=\"{master}\"/><{name} channel=\"LF\" val=\"{side}\"/>\
                 <{name} channel=\"RF\" val=\"{side}\"/>"
            )
        };
        vec![
            (
                "Volume".into(),
                channels("Volume", player.volume.to_string(), "100"),
            ),
            (
                "Mute".into(),
                channels("Mute", u8::from(player.mute).to_string(), "0"),
            ),
            ("Bass".into(), "<Bass val=\"0\"/>".into()),
            ("Treble".into(), "<Treble val=\"0\"/>".into()),
            (
                "Loudness".into(),
                "<Loudness channel=\"Master\" val=\"1\"/>".into(),
            ),
        ]
    }

    /// GroupRenderingControl properties of `p`'s group (plain properties, not
    /// LastChange): the members' rounded average volume, whether all are
    /// muted, and that the group volume can be changed.
    fn grc_props(&self, p: usize) -> Vec<(String, String)> {
        let members = self.members(self.players[p].coordinator);
        let n = u32::try_from(members.len()).unwrap_or(1).max(1);
        let total: u32 = members
            .iter()
            .map(|&m| u32::from(self.players[m].volume))
            .sum();
        let muted = members.iter().all(|&m| self.players[m].mute);
        [
            ("GroupVolume", ((total + n / 2) / n).to_string()),
            ("GroupMute", u8::from(muted).to_string()),
            ("GroupVolumeChangeable", "1".to_string()),
        ]
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                format!("<e:property><{name}>{value}</{name}></e:property>"),
            )
        })
        .collect()
    }

    /// ZoneGroupTopology properties as seen from `p`.
    fn zgt_props(&self, p: usize) -> Vec<(String, String)> {
        let h = self.players[p].household;
        let c = self.players[p].coordinator;
        let members = self.members(c);
        let names: Vec<&str> = members
            .iter()
            .filter(|&&m| self.players[m].pair_primary.is_none())
            .map(|&m| self.players[m].room.as_str())
            .collect();
        let uuids: Vec<&str> = members
            .iter()
            .map(|&m| self.players[m].uuid.as_str())
            .collect();
        [
            ("ZoneGroupState", docs::zone_group_state(self, h)),
            ("ZoneGroupName", names.join(" + ")),
            ("ZoneGroupID", self.players[c].group_id.clone()),
            ("ZonePlayerUUIDsInGroup", uuids.join(",")),
            ("MuseHouseholdId", self.households[h].id.clone()),
        ]
        .into_iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                format!(
                    "<e:property><{name}>{}</{name}></e:property>",
                    xml_escape(&value)
                ),
            )
        })
        .collect()
    }
}

/// The NOTIFY body for `service` carrying `vars`.
fn event_body(service: EventService, vars: &[&(String, String)]) -> String {
    let mut inner = String::new();
    for (_, fragment) in vars {
        inner.push_str(fragment);
    }
    let open = "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">";
    match service {
        EventService::ZoneGroupTopology | EventService::GroupRenderingControl => {
            format!("{open}{inner}</e:propertyset>")
        }
        EventService::AvTransport | EventService::RenderingControl => {
            let schema = if service == EventService::AvTransport {
                "urn:schemas-upnp-org:metadata-1-0/AVT/\" xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/"
            } else {
                "urn:schemas-upnp-org:metadata-1-0/RCS/"
            };
            let event = format!(
                "<Event xmlns=\"{schema}\"><InstanceID val=\"0\">{inner}</InstanceID></Event>"
            );
            let mut body = String::from(open);
            let _ = write!(
                body,
                "<e:property><LastChange>{}</LastChange></e:property></e:propertyset>",
                xml_escape(&event)
            );
            body
        }
    }
}

/// The first URL of a `CALLBACK` header (`<http://…><http://…>`).
pub(crate) fn first_callback(header: &str) -> Option<String> {
    let start = header.find('<')? + 1;
    let end = start + header[start..].find('>')?;
    let url = header[start..end].trim();
    url.starts_with("http://").then(|| url.to_string())
}

/// Deliver NOTIFYs in order on one thread, logging each outcome, until every
/// sender is gone.
pub(crate) fn start_notifier(
    state: Arc<Mutex<State>>,
    queue: mpsc::Receiver<Outgoing>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("fsonos-sim-notifier".into())
        .spawn(move || {
            for out in queue {
                let result = deliver(&out);
                if let Ok(mut s) = state.lock()
                    && let Some(p) = s.players.iter().position(|p| p.uuid == out.player)
                {
                    let event = match result {
                        Ok(status) => GenaEvent::Delivered {
                            sid: out.sid.clone(),
                            seq: out.seq,
                            status,
                        },
                        Err(error) => GenaEvent::DeliveryFailed {
                            sid: out.sid.clone(),
                            seq: out.seq,
                            error,
                        },
                    };
                    s.log_gena(p, out.service, event);
                }
            }
        })
}

/// POST-style NOTIFY over a plain TCP connection; returns the HTTP status.
fn deliver(out: &Outgoing) -> Result<u16, String> {
    let rest = out
        .callback
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported callback {}", out.callback))?;
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let addr = authority
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| format!("no address for {authority}"))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "NOTIFY {path} HTTP/1.1\r\nHOST: {authority}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         NT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {seq}\r\n\
         CONTENT-LENGTH: {len}\r\nConnection: close\r\n\r\n{body}",
        sid = out.sid,
        seq = out.seq,
        len = out.body.len(),
        body = out.body,
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    let text = String::from_utf8_lossy(&reply);
    text.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("no HTTP status in reply ({} bytes)", reply.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_header_takes_the_first_http_url() {
        assert_eq!(
            first_callback("<http://127.0.0.1:5555/avt><http://x/y>").as_deref(),
            Some("http://127.0.0.1:5555/avt")
        );
        assert_eq!(first_callback("http://no-brackets"), None);
        assert_eq!(first_callback("<ftp://x>"), None);
    }
}
