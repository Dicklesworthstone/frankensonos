//! Album art for the web remote (`GET /art?zone=<room>`): the image the
//! zone's own player serves for what it plays, fetched through the
//! transport so a browser never needs a LAN URL (or mixed content).
//!
//! The only URL ever fetched is `http://<the zone's coordinator>:1400/getaa?…`,
//! built from the art path the coordinator itself reports. Art on a
//! streaming service's CDN (a public `https://` image, as real players
//! report most Spotify art) is the browser's to load: `/art` sends it there
//! ([`AlbumArt::Public`]). Anything else (another host or port, plain
//! `http`, an address or name on this network) is none. The caller names a
//! room, never a URL, so the daemon cannot be made to reach anything on a
//! caller's behalf.

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

/// The album art of what a zone plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumArt {
    /// The image its player served.
    Served(Art),
    /// A public image on a service's CDN ([`public_art_url`]), for the
    /// browser to load; the daemon never fetches it.
    Public(String),
}

impl Surface {
    /// The album art of what `zone` plays, as its coordinator reports it
    /// (`GET /art`): the image that player serves, or a public image on a
    /// service's CDN; `Ok(None)` when it plays nothing with such art, or the
    /// player did not serve it. A read: authorized as `get_zone_state`.
    pub fn album_art(&self, client: &Client, zone: &str) -> Result<Option<AlbumArt>, Failure> {
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
        let Some(reported) = reported else {
            return Ok(None);
        };
        if let Some(public) = public_art_url(&reported) {
            return Ok(Some(AlbumArt::Public(public)));
        }
        let Some(url) = art_url(coordinator.ip, &reported) else {
            return Ok(None);
        };
        let fetched = match self.transport.http_get_bytes(&url) {
            Ok(fetched) => fetched,
            Err(e) => {
                tracing::debug!("album art for {zone}: {e}");
                return Ok(None);
            }
        };
        let content_type = match fetched.content_type {
            Some(declared) => Some(declared).filter(|t| is_image(t)),
            // An S1 player serves its art with no Content-Type (seen live).
            None => sniffed_image(&fetched.body).map(str::to_string),
        }
        .unwrap_or_default();
        if content_type.is_empty() || fetched.body.is_empty() || fetched.body.len() > MAX_ART_BYTES
        {
            tracing::debug!("album art for {zone}: not an image of at most {MAX_ART_BYTES} bytes");
            return Ok(None);
        }
        Ok(Some(AlbumArt::Served(Art {
            content_type,
            bytes: fetched.body,
        })))
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

/// `reported` when it is a public `https://` image (a streaming service's
/// CDN, such as `https://i.scdn.co/image/…`), which a browser may load
/// itself; `None` for anything that could point a browser at this network
/// instead: plain `http`, an address, a port, credentials, or a local or
/// single-label name.
#[must_use]
pub fn public_art_url(reported: &str) -> Option<String> {
    let reported = reported.trim();
    let rest = reported.strip_prefix("https://")?;
    let host = rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())].to_ascii_lowercase();
    let named = host.contains('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && host
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.chars().any(|c| c.is_ascii_alphabetic()))
        && !LOCAL_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix) || host == suffix[1..]);
    let clean = reported.len() <= 2048
        && !reported
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '\\' | '"' | '<' | '>'));
    (named && clean).then(|| reported.to_string())
}

/// Names that stay on this network.
const LOCAL_SUFFIXES: [&str; 6] = [
    ".local",
    ".localhost",
    ".lan",
    ".internal",
    ".home.arpa",
    ".home",
];

/// The raster image `body` is, by its first bytes, when the player named no
/// type: JPEG, PNG, GIF or WebP.
fn sniffed_image(body: &[u8]) -> Option<&'static str> {
    if body.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if body.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if body.starts_with(b"RIFF") && body.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
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

    /// Real players report most Spotify art on its CDN: the browser's to
    /// load. Nothing that could reach this network instead.
    #[test]
    fn public_art_is_https_on_a_public_name() {
        for public in [
            "https://i.scdn.co/image/ab67616d0000b273abc",
            " https://is1-ssl.mzstatic.com/image/thumb/a/600x600bb.jpg ",
            "https://i1.sndcdn.com/artworks-abc-t500x500.jpg?x=1",
        ] {
            assert_eq!(
                public_art_url(public).as_deref(),
                Some(public.trim()),
                "{public}"
            );
        }
        for local in [
            "http://i.scdn.co/image/abc",
            "/getaa?s=1&u=x",
            "https://192.0.2.10/cover.jpg",
            "https://[2001:db8::1]/cover.jpg",
            "https://art.example:8443/cover.jpg",
            "https://user@art.example/cover.jpg",
            "https://printer.local/cover.jpg",
            "https://localhost/cover.jpg",
            "https://nas/cover.jpg",
            "https://router.home.arpa/cover.jpg",
            "https://art.example/a b.jpg",
            "https://art.example/a\r\nSet-Cookie: x",
            "https://art.example/\"onerror=\"x",
            "https://",
        ] {
            assert_eq!(public_art_url(local), None, "{local}");
        }
    }

    /// Seen live: an S1 player serves its art with no Content-Type.
    #[test]
    fn untyped_art_is_named_by_its_first_bytes() {
        assert_eq!(
            sniffed_image(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10]),
            Some("image/jpeg")
        );
        assert_eq!(sniffed_image(b"\x89PNG\r\n\x1a\n\0\0"), Some("image/png"));
        assert_eq!(sniffed_image(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniffed_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        for other in [
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".as_slice(),
            b"<html></html>".as_slice(),
            b"RIFF\0\0\0\0WAVEfmt ".as_slice(),
            b"".as_slice(),
        ] {
            assert_eq!(sniffed_image(other), None);
        }
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
