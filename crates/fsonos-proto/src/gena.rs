//! GENA event subscriptions.
//!
//! Sonos pushes state changes (transport, volume, topology) to subscribers via
//! GENA: the controller SUBSCRIBEs with a callback URL, and the player POSTs
//! NOTIFY requests carrying a LastChange XML document. Subscriptions must be
//! renewed before their timeout. Header construction and NOTIFY parsing are
//! pure here; the callback HTTP sink (asupersync HTTP server) is wired in bead
//! FND-DEPS.

/// A GENA subscription handle.
#[derive(Debug, Clone)]
pub struct Subscription {
    pub sid: String,
    pub timeout_secs: u32,
}

/// Build the SUBSCRIBE request headers for `event_path` with the given callback
/// URL and requested timeout.
#[must_use]
pub fn subscribe_headers(callback_url: &str, timeout_secs: u32) -> Vec<(String, String)> {
    vec![
        ("CALLBACK".into(), format!("<{callback_url}>")),
        ("NT".into(), "upnp:event".into()),
        ("TIMEOUT".into(), format!("Second-{timeout_secs}")),
    ]
}

/// Parse the `TIMEOUT: Second-N` response header value into seconds.
#[must_use]
pub fn parse_timeout(value: &str) -> Option<u32> {
    value.strip_prefix("Second-")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_subscribe_headers() {
        let h = subscribe_headers("http://192.0.2.1:3400/cb", 300);
        assert!(h.iter().any(|(k, v)| k == "CALLBACK" && v.contains("3400")));
    }

    #[test]
    fn parses_timeout() {
        assert_eq!(parse_timeout("Second-300"), Some(300));
    }
}
