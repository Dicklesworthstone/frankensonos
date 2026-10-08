//! Album art for the web remote (`GET /art?zone=<room>`): the image the
//! zone's own player serves for what it plays, fetched through the
//! transport so a browser never needs a LAN URL (or mixed content).
//!
//! The only URL ever fetched is `http://<the zone's coordinator>:1400/getaa?…`,
//! built from the art path the coordinator itself reports. Art anywhere
//! else (a streaming service's CDN, another host or port) is not fetched:
//! the caller names a room, never a URL, so the daemon cannot be made to
//! reach anything on a caller's behalf.

use fsonos_core::control;
use fsonos_core::policy::Client;
use fsonos_proto::net::PLAYER_PORT;
use std::net::IpAddr;

use super::{Surface, resolve, room_view};
use crate::failure::Failure;

/// The largest image passed on (the transport's own limit is higher).
pub const MAX_ART_BYTES: usize = 2 * 1024 * 1024;

/// The path every player serves album art under.
const ART_PATH: &str = "/getaa?";

/// An image a player served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Art {
    /// `image/…`.
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl Surface {
    /// The album art of what `zone` plays, from its coordinator
    /// (`GET /art`); `Ok(None)` when it plays nothing with art that player
    /// serves, or the player did not serve it. A read: authorized as
    /// `get_zone_state`.
    pub fn album_art(&self, client: &Client, zone: &str) -> Result<Option<Art>, Failure> {
        self.guard(client).authorize("get_zone_state", true)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let coordinator = target.coordinator;
        let heard = self
            .live()
            .and_then(|live| live.player(&coordinator.id))
            .filter(|p| p.track_uri.is_some());
        let reported = match heard {
            Some(group) => group.now_playing.and_then(|n| n.art_uri),
            None => control::playback(&*self.transport, &households, &coordinator.id)
                .map_err(Failure::from)
                .inspect_err(|f| self.notice(f))?
                .position
                .metadata
                .and_then(|m| m.album_art_uri),
        };
        let Some(url) = reported.and_then(|r| art_url(coordinator.ip, &r)) else {
            return Ok(None);
        };
        let fetched = match self.transport.http_get_bytes(&url) {
            Ok(fetched) => fetched,
            Err(e) => {
                tracing::debug!("album art for {zone}: {e}");
                return Ok(None);
            }
        };
        let content_type = fetched
            .content_type
            .filter(|t| is_image(t))
            .unwrap_or_default();
        if content_type.is_empty() || fetched.body.is_empty() || fetched.body.len() > MAX_ART_BYTES
        {
            tracing::debug!("album art for {zone}: not an image of at most {MAX_ART_BYTES} bytes");
            return Ok(None);
        }
        Ok(Some(Art {
            content_type,
            bytes: fetched.body,
        }))
    }
}

/// The URL to fetch the art `reported` by the player at `player`: its own
/// `/getaa?…`, as a path or as an absolute URL naming that player's
/// `:1400`. `None` for anything else.
#[must_use]
pub fn art_url(player: IpAddr, reported: &str) -> Option<String> {
    let reported = reported.trim();
    let host = match player {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let own = format!("http://{host}:{PLAYER_PORT}");
    let path = reported.strip_prefix(&own).unwrap_or(reported);
    let clean = path.starts_with(ART_PATH)
        && path.len() <= 2048
        && !path
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '#' | '\\'));
    clean.then(|| format!("{own}{path}"))
}

/// `image/png`, `image/jpeg; …` and the like (not SVG, which can carry
/// script).
fn is_image(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence.starts_with("image/") && essence != "image/svg+xml"
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER: &str = "192.0.2.10";

    fn url(reported: &str) -> Option<String> {
        art_url(PLAYER.parse().unwrap(), reported)
    }

    #[test]
    fn only_the_players_own_art_is_fetched() {
        assert_eq!(
            url("/getaa?s=1&u=x-sonos-spotify%3aspotify%253atrack%253aA").as_deref(),
            Some("http://192.0.2.10:1400/getaa?s=1&u=x-sonos-spotify%3aspotify%253atrack%253aA")
        );
        assert_eq!(
            url("http://192.0.2.10:1400/getaa?s=1&u=x").as_deref(),
            Some("http://192.0.2.10:1400/getaa?s=1&u=x")
        );
        for elsewhere in [
            "https://art.example.invalid/1.jpg",
            "http://192.0.2.11:1400/getaa?s=1&u=x",
            "http://192.0.2.10:8080/getaa?s=1&u=x",
            "http://192.0.2.10:1400@evil.example/getaa?s=1",
            "http://192.0.2.100:1400/getaa?s=1",
            "//evil.example/getaa?s=1",
            "/xml/device_description.xml",
            "/getaa?s=1&u=x#frag",
            "/getaa?s=1&u=a b",
            "/getaa?s=1&u=a\r\nHost: evil.example",
            "",
        ] {
            assert_eq!(url(elsewhere), None, "{elsewhere}");
        }
        let v6 = art_url("fd00::10".parse().unwrap(), "/getaa?s=1&u=x");
        assert_eq!(v6.as_deref(), Some("http://[fd00::10]:1400/getaa?s=1&u=x"));
    }

    #[test]
    fn only_raster_images_pass() {
        assert!(is_image("image/png"));
        assert!(is_image("Image/JPEG; charset=binary"));
        assert!(!is_image("image/svg+xml"));
        assert!(!is_image("text/html"));
        assert!(!is_image(""));
    }
}
