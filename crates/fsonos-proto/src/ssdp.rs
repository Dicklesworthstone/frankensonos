//! SSDP discovery (UDP multicast 239.255.255.250:1900).
//!
//! Finds Sonos ZonePlayers on the LAN via an `M-SEARCH` for
//! `urn:schemas-upnp-org:device:ZonePlayer:1`. The message construction and
//! response parsing are pure and unit-testable here; the multicast socket
//! (asupersync `UdpSocket::join_multicast_v4`) is wired in bead FND-DEPS.

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
/// of its device description XML, e.g. `http://192.168.4.202:1400/xml/device_description.xml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    pub location: String,
    pub st: String,
    pub usn: Option<String>,
}

/// Parse an SSDP response datagram into an [`Advert`], if it is a Sonos reply.
#[must_use]
pub fn parse_response(bytes: &[u8]) -> Option<Advert> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut location = None;
    let mut st = None;
    let mut usn = None;
    for line in text.lines() {
        let (k, v) = line.split_once(':')?;
        match k.trim().to_ascii_uppercase().as_str() {
            "LOCATION" => location = Some(v.trim().to_string()),
            "ST" => st = Some(v.trim().to_string()),
            "USN" => usn = Some(v.trim().to_string()),
            _ => {}
        }
    }
    Some(Advert {
        location: location?,
        st: st.unwrap_or_default(),
        usn,
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
        let resp = b"HTTP/1.1 200 OK\r\nLOCATION: http://192.168.4.202:1400/xml/device_description.xml\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";
        let a = parse_response(resp).unwrap();
        assert_eq!(
            a.location,
            "http://192.168.4.202:1400/xml/device_description.xml"
        );
    }
}
