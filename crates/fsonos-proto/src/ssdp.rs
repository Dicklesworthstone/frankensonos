//! SSDP discovery (UDP multicast 239.255.255.250:1900).
//!
//! Finds Sonos ZonePlayers on the LAN via an `M-SEARCH` for
//! `urn:schemas-upnp-org:device:ZonePlayer:1`. The message construction and
//! response parsing are pure and unit-testable here; the socket lives in
//! [`crate::net`]. Replies are unicast back to the searching socket, so no
//! multicast group join is needed to search.

use std::net::{IpAddr, SocketAddr};

/// The SSDP multicast group and port.
pub const SSDP_ADDR: &str = "239.255.255.250:1900";

/// The Sonos ZonePlayer search target.
pub const SONOS_ST: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";

/// Build an SSDP `M-SEARCH` request datagram for Sonos ZonePlayers.
#[must_use]
pub fn m_search(mx_secs: u8) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {mx_secs}\r\n\
         ST: {SONOS_ST}\r\n\r\n"
    )
}

/// A discovered device's advertised location (the `LOCATION:` header: the URL
/// of its device description XML, e.g. `http://192.0.2.10:1400/xml/device_description.xml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    pub location: String,
    pub st: String,
    pub usn: Option<String>,
    /// `X-RINCON-HOUSEHOLD`: the Sonos household the player belongs to.
    pub household: Option<String>,
    /// `X-RINCON-BOOTSEQ`: increments when the player reboots.
    pub boot_seq: Option<u32>,
}

/// The device-description URL of the player at `ip` (players serve it on
/// port 1400), for direct-seed discovery when SSDP is unavailable.
#[must_use]
pub fn description_url(ip: IpAddr) -> String {
    format!(
        "http://{}/xml/device_description.xml",
        SocketAddr::new(ip, 1400)
    )
}

/// Parse an SSDP response datagram into an [`Advert`], if it is a Sonos reply.
#[must_use]
pub fn parse_response(bytes: &[u8]) -> Option<Advert> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut location = None;
    let mut st = None;
    let mut usn = None;
    let mut household = None;
    let mut boot_seq = None;
    // The status line (`HTTP/1.1 200 OK`) carries no colon; skip such lines.
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_uppercase().as_str() {
            "LOCATION" => location = Some(v.trim().to_string()),
            "ST" => st = Some(v.trim().to_string()),
            "USN" => usn = Some(v.trim().to_string()),
            "X-RINCON-HOUSEHOLD" => household = Some(v.trim().to_string()),
            "X-RINCON-BOOTSEQ" => boot_seq = v.trim().parse().ok(),
            _ => {}
        }
    }
    Some(Advert {
        location: location?,
        st: st.unwrap_or_default(),
        usn,
        household,
        boot_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m_search_targets_zoneplayer() {
        let s = m_search(1);
        assert!(s.contains(SONOS_ST));
        assert!(s.starts_with("M-SEARCH * HTTP/1.1"));
    }

    #[test]
    fn parses_location() {
        let resp = b"HTTP/1.1 200 OK\r\nLOCATION: http://192.0.2.10:1400/xml/device_description.xml\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";
        let a = parse_response(resp).unwrap();
        assert_eq!(
            a.location,
            "http://192.0.2.10:1400/xml/device_description.xml"
        );
        assert_eq!(a.household, None);
    }

    #[test]
    fn parses_sonos_headers() {
        let resp = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age = 1800\r\n\
                     LOCATION: http://192.0.2.10:1400/xml/device_description.xml\r\n\
                     ST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\
                     USN: uuid:RINCON_000E58A0000001400::urn:schemas-upnp-org:device:ZonePlayer:1\r\n\
                     X-RINCON-HOUSEHOLD: Sonos_ExampleHousehold0001\r\n\
                     X-RINCON-BOOTSEQ: 42\r\n\r\n";
        let a = parse_response(resp).unwrap();
        assert_eq!(a.household.as_deref(), Some("Sonos_ExampleHousehold0001"));
        assert_eq!(a.boot_seq, Some(42));
        assert_eq!(a.st, SONOS_ST);
    }

    #[test]
    fn seed_description_url_uses_port_1400() {
        assert_eq!(
            description_url("192.0.2.10".parse().unwrap()),
            "http://192.0.2.10:1400/xml/device_description.xml"
        );
        assert_eq!(
            description_url("2001:db8::1".parse().unwrap()),
            "http://[2001:db8::1]:1400/xml/device_description.xml"
        );
    }
}
