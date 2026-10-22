//! The DJ's own playback starts, held to the house policy. Feeding a group,
//! the DJ sends Play by itself only when its queue ran dry or it carries out
//! a skip: music no one asked for at that moment. As client `dj`
//! (`[clients.dj]` in policy.toml; capped like any agent unless that says
//! otherwise), the group's rooms come down to its caps first, as they do
//! before an agent's play (p7vq.13). Quiet hours hold at 2 a.m. even when the
//! session started at noon.

use fsonos_core::HouseholdState;
use fsonos_core::policy::Client;
use fsonos_proto::{Discovered, HttpBody, ProtoError, Transport, mdns, ssdp};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::time::Duration;

use super::Surface;
use crate::plan::{Command, TransportAction};

/// The policy client the DJ's own actions are held to, and logged as.
const DJ_CLIENT: &str = "dj";

/// The transport the DJ feeds `coordinator`'s group through.
pub(super) struct DjCaps<'a> {
    pub(super) surface: &'a Surface,
    pub(super) households: &'a [HouseholdState],
    pub(super) coordinator: &'a PlayerId,
}

impl DjCaps<'_> {
    /// Whether `action` to `host` starts the group's playback.
    fn starts(&self, host: IpAddr, action: &str) -> bool {
        action.ends_with("#Play\"")
            && self
                .households
                .iter()
                .find_map(|h| h.player(self.coordinator))
                .is_some_and(|p| p.ip == host)
    }

    /// Lower the group's rooms to the DJ's caps before it plays.
    fn lower(&self) -> Result<(), ProtoError> {
        let client = Client::Tailnet(DJ_CLIENT.to_owned());
        let start = Command::Transport {
            coordinator: self.coordinator.clone(),
            action: TransportAction::Resume,
        };
        self.surface
            .lower_before_playing(self.households, &self.surface.guard(&client), &start)
            .map(drop)
            .map_err(|f| ProtoError::Network {
                target: "the DJ's volume caps".to_owned(),
                detail: f.detail,
            })
    }
}

impl Transport for DjCaps<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.starts(host, soap_action) {
            self.lower()?;
        }
        self.surface
            .transport
            .soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.surface.transport.http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        self.surface.transport.http_get_bytes(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        self.surface.transport.ssdp_search(mx_secs, wait)
    }

    fn mdns_search(&self, wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
        self.surface.transport.mdns_search(wait)
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        self.surface.transport.discover(mx_secs, wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::testing::surface;
    use crate::zones::fixtures::{households, id};

    const PLAY: &str = "\"urn:schemas-upnp-org:service:AVTransport:1#Play\"";
    const ADD: &str = "\"urn:schemas-upnp-org:service:AVTransport:1#AddURIToQueue\"";

    #[test]
    fn the_djs_own_play_comes_down_to_its_caps_first() {
        // Every room reads 80; an agent, the DJ among them, is capped at 70.
        let (s, sent) = surface("<CurrentVolume>80</CurrentVolume>", households());
        let h = households();
        let den = id("RINCON_DEN");
        let caps = DjCaps {
            surface: &s,
            households: &h,
            coordinator: &den,
        };
        let host = h[0].player(&den).unwrap().ip;
        caps.soap_post(host, "/MediaRenderer/AVTransport/Control", ADD, "")
            .unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            ["AddURIToQueue"],
            "a top-up starts nothing"
        );
        caps.soap_post(host, "/MediaRenderer/AVTransport/Control", PLAY, "")
            .unwrap();
        let sent = sent.lock().unwrap().clone();
        // Den's group is Den and Kitchen: both lowered, then the play.
        assert_eq!(
            sent.iter().filter(|a| *a == "SetVolume").count(),
            2,
            "{sent:?}"
        );
        assert_eq!(sent.last().map(String::as_str), Some("Play"), "{sent:?}");
    }
}
