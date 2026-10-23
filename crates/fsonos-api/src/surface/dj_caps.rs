//! The DJ's own playback starts, held to the house policy. Feeding a group,
//! the DJ sends Play by itself only when its queue ran dry or it carries out
//! a skip: music no one asked for at that moment. As client `dj`
//! (`[clients.dj]` in policy.toml; capped like any agent unless that says
//! otherwise), the group's rooms come down to its caps first, as they do
//! before an agent's play (p7vq.13). Quiet hours hold at 2 a.m. even when the
//! session started at noon. They keep to the caps of whoever started the
//! session as well: when policy.toml exempts the DJ (`[clients.dj] capped =
//! false`), a session a capped agent started still restarts within the caps.

use fsonos_core::HouseholdState;
use fsonos_core::policy::Client;
use fsonos_proto::{Discovered, HttpBody, ProtoError, Transport, mdns, ssdp};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::Surface;
use crate::plan::{Command, DjAction, TransportAction};

/// The policy client the DJ's own actions are held to, and logged as.
const DJ_CLIENT: &str = "dj";

/// The transport the DJ feeds `coordinator`'s group through.
pub(super) struct DjCaps<'a> {
    pub(super) surface: &'a Surface,
    pub(super) households: &'a [HouseholdState],
    pub(super) coordinator: &'a PlayerId,
    /// Who started the session ([`Starters`]), held to its caps too.
    pub(super) starter: Option<Client>,
}

/// Who started the DJ in each group, by coordinator. Kept in memory: a
/// session the daemon resumes after a restart has no starter, and its own
/// starts are held to the DJ's caps alone.
#[derive(Default)]
pub(super) struct Starters(Mutex<HashMap<PlayerId, Client>>);

impl Starters {
    fn map(&self) -> MutexGuard<'_, HashMap<PlayerId, Client>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `client` started the DJ in the group `coordinator` leads.
    pub(super) fn started(&self, coordinator: &PlayerId, client: &Client) {
        self.map().insert(coordinator.clone(), client.clone());
    }

    /// `client` carried `command` out: a start makes it the session's
    /// starter, a stop forgets the session.
    pub(super) fn carried_out(&self, command: &Command, client: &Client) {
        match command {
            Command::Dj {
                coordinator,
                action: DjAction::Start,
            }
            | Command::DjSteer {
                coordinator,
                start: true,
                ..
            } => self.started(coordinator, client),
            Command::Dj {
                coordinator,
                action: DjAction::Stop,
            } => {
                self.map().remove(coordinator);
            }
            _ => {}
        }
    }

    /// The session `from` led plays on under `to`.
    pub(super) fn moved(&self, from: &PlayerId, to: &PlayerId) {
        let mut map = self.map();
        if let Some(client) = map.remove(from) {
            map.insert(to.clone(), client);
        }
    }

    /// Who started the session `coordinator` leads, when known.
    pub(super) fn of(&self, coordinator: &PlayerId) -> Option<Client> {
        self.map().get(coordinator).cloned()
    }
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

    /// Lower the group's rooms to the DJ's caps, and to its starter's,
    /// before it plays. Each pass lowers the rooms above that client's cap
    /// (none for an uncapped client), so a room ends within both.
    fn lower(&self) -> Result<(), ProtoError> {
        let dj = Client::Tailnet(DJ_CLIENT.to_owned());
        let start = Command::Transport {
            coordinator: self.coordinator.clone(),
            action: TransportAction::Resume,
        };
        for client in std::iter::once(&dj).chain(self.starter.as_ref()) {
            self.surface
                .lower_before_playing(self.households, &self.surface.guard(client), &start)
                .map_err(|f| ProtoError::Network {
                    target: "the DJ's volume caps".to_owned(),
                    detail: f.detail,
                })?;
        }
        Ok(())
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
            starter: None,
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

    #[test]
    fn a_sessions_starter_is_kept_until_it_stops_and_follows_a_move() {
        let starters = Starters::default();
        let (den, kitchen) = (id("RINCON_DEN"), id("RINCON_KIT1"));
        let agent = Client::McpStdio;
        let start = Command::Dj {
            coordinator: den.clone(),
            action: DjAction::Start,
        };
        starters.carried_out(&start, &agent);
        let skip = Command::Dj {
            coordinator: den.clone(),
            action: DjAction::Skip,
        };
        starters.carried_out(&skip, &Client::Cli);
        assert_eq!(
            starters.of(&den),
            Some(agent.clone()),
            "a skip starts nothing"
        );
        starters.moved(&den, &kitchen);
        assert_eq!(starters.of(&den), None);
        assert_eq!(starters.of(&kitchen), Some(agent));
        let stop = Command::Dj {
            coordinator: kitchen.clone(),
            action: DjAction::Stop,
        };
        starters.carried_out(&stop, &Client::Cli);
        assert_eq!(starters.of(&kitchen), None);
    }
}
