//! Owner rights follow this host's identity, independently of listener binds.
//!
//! Interface addresses are enough to offer a listener, but never to grant
//! ownership. Only an answered local status and WhoIs of its own addresses
//! establish a login. Brief failures retain the last proof; changed host
//! identity, logout, or a tagged node retires it immediately.

use fsonos_tailscale::{Identity, Probe, Source, TailnetStatus, Unavailable, WhoIs};
use std::net::IpAddr;

use super::UNANSWERED_LIMIT;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Host {
    addresses: Vec<IpAddr>,
    name: Option<String>,
    tailnet: Option<String>,
}

impl Host {
    fn observed(status: &TailnetStatus) -> Option<Self> {
        let tailnet = status.running()?;
        if tailnet.source != Source::Cli || !tailnet.logged_in {
            return None;
        }
        let mut addresses: Vec<_> = tailnet
            .ipv4
            .iter()
            .copied()
            .map(IpAddr::V4)
            .chain(tailnet.ipv6.iter().copied().map(IpAddr::V6))
            .filter(|ip| fsonos_tailscale::is_tailnet_ip(*ip))
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.is_empty() {
            return None;
        }
        let normalized = |name: &str| name.trim_end_matches('.').to_ascii_lowercase();
        Some(Self {
            addresses,
            name: tailnet.magic_dns_name.as_deref().map(normalized),
            tailnet: tailnet.tailnet.as_deref().map(normalized),
        })
    }

    /// One own address per family bounds a failed observation to two WhoIs
    /// attempts and permits IPv6-only hosts or an unavailable IPv4 answer.
    fn candidates(&self) -> impl Iterator<Item = IpAddr> + '_ {
        self.addresses
            .iter()
            .find(|ip| ip.is_ipv4())
            .into_iter()
            .chain(self.addresses.iter().find(|ip| ip.is_ipv6()))
            .copied()
    }
}

#[derive(Default)]
pub(super) struct Owner {
    host: Option<Host>,
    login: Option<String>,
    unanswered: u32,
}

impl Owner {
    pub(super) fn new(status: &TailnetStatus, login: Option<String>) -> Self {
        let host = Host::observed(status);
        Self {
            login: login.filter(|_| host.is_some()),
            host,
            unanswered: 0,
        }
    }

    pub(super) fn probe(
        &mut self,
        status: &TailnetStatus,
        probe: &Probe,
        mut publish: impl FnMut(Option<String>),
    ) {
        // The ordinary peer cache can outlive an account switch. Ownership
        // always asks afresh, using only this local status's own addresses.
        let whois = WhoIs::new(probe.clone());
        let expected = Host::observed(status);
        let mut confirmed = false;
        let mut changed = None;
        self.refresh(
            status,
            |ip| {
                if confirmed {
                    return None;
                }
                let identity = whois.resolve(ip)?;
                confirmed = true;
                // These observations can only revoke authority. Do not
                // retain an old owner through a second probe failure after
                // WhoIs already reported a tagged or unowned node.
                if !identity.tags.is_empty()
                    || identity
                        .login
                        .as_deref()
                        .is_none_or(|login| login.trim().is_empty())
                {
                    return Some(identity);
                }
                // An account switch during WhoIs could make the previous
                // own address name a peer in the new network. Only a fresh,
                // matching local status can confirm this host's identity.
                let current = probe.detect();
                if !current.answered() {
                    return None;
                }
                let host = Host::observed(&current);
                if host != expected {
                    changed = Some(host);
                    return None;
                }
                Some(identity)
            },
            &mut publish,
        );
        if let Some(host) = changed {
            self.host = host;
            self.unanswered = 0;
            self.set(None, &mut publish);
        }
    }

    fn set(&mut self, login: Option<String>, publish: &mut impl FnMut(Option<String>)) {
        if self.login != login {
            self.login = login;
            publish(self.login.clone());
        }
    }

    fn missed(&mut self, publish: &mut impl FnMut(Option<String>)) {
        self.unanswered = self.unanswered.saturating_add(1);
        if self.unanswered >= UNANSWERED_LIMIT {
            self.set(None, publish);
        }
    }

    pub(super) fn refresh(
        &mut self,
        status: &TailnetStatus,
        mut resolve: impl FnMut(IpAddr) -> Option<Identity>,
        mut publish: impl FnMut(Option<String>),
    ) {
        if !status.answered() {
            if matches!(status, TailnetStatus::Unavailable(Unavailable::Disabled)) {
                self.host = None;
                self.unanswered = 0;
                self.set(None, &mut publish);
            } else {
                self.missed(&mut publish);
            }
            return;
        }
        let Some(host) = Host::observed(status) else {
            self.host = None;
            self.unanswered = 0;
            self.set(None, &mut publish);
            return;
        };
        if self.host.as_ref() != Some(&host) {
            self.host = Some(host.clone());
            self.unanswered = 0;
            // Publish the revocation before a possibly slow WhoIs. The old
            // account has no grace period once the local host changed.
            self.set(None, &mut publish);
        }
        for ip in host.candidates() {
            if let Some(identity) = resolve(ip) {
                self.unanswered = 0;
                let login = identity
                    .login
                    .filter(|_| identity.tags.is_empty())
                    .map(|login| login.trim().to_owned())
                    .filter(|login| !login.is_empty());
                self.set(login, &mut publish);
                return;
            }
        }
        self.missed(&mut publish);
    }
}

pub(super) fn initial(status: &TailnetStatus) -> Option<String> {
    let mut owner = Owner::default();
    owner.probe(status, &Probe::default(), |_| {});
    owner.login
}

#[cfg(test)]
mod tests;
