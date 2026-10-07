//! Canonicalizing the `source_uri` a caller asks to play.
//!
//! Agents and people paste what they have: a `spotify:track:…` URI, an
//! `https://open.spotify.com/…` share link (often with `?si=` tracking and an
//! `intl-xx` locale segment), or a renderer URI such as a radio stream. This
//! module turns Spotify links into the canonical `spotify:<kind>:<id>` form the
//! core renders, checks Spotify ids strictly so truncated pastes fail here
//! with a clear message, and passes any other well-formed URI through
//! unchanged for the core and the speaker to judge.

use crate::failure::Failure;

/// Longest `source_uri` accepted, in bytes.
pub const MAX_SOURCE_URI_LEN: usize = 2048;

/// Spotify content kinds a `spotify:` URI may name.
const SPOTIFY_KINDS: &[&str] = &["track", "album", "playlist", "artist", "episode", "show"];

/// Canonicalize a caller-supplied source URI. See the module docs.
pub fn normalize_source_uri(raw: &str) -> Result<String, Failure> {
    let uri = raw.trim();
    if uri.is_empty() {
        return Err(Failure::invalid("source_uri is empty"));
    }
    if uri.len() > MAX_SOURCE_URI_LEN {
        return Err(Failure::invalid(format!(
            "source_uri is {} bytes; the limit is {MAX_SOURCE_URI_LEN}",
            uri.len()
        )));
    }
    if uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Failure::invalid(format!(
            "source_uri {uri:?} contains whitespace; pass a single URI"
        )));
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return Err(Failure::invalid(format!(
            "source_uri {uri:?} has no scheme; expected e.g. spotify:track:<id> or an open.spotify.com link"
        )));
    };
    if !is_uri_scheme(scheme) || rest.is_empty() {
        return Err(Failure::invalid(format!(
            "source_uri {uri:?} is not a URI; expected e.g. spotify:track:<id> or an open.spotify.com link"
        )));
    }
    match scheme.to_ascii_lowercase().as_str() {
        "spotify" => spotify_uri(uri, rest),
        "http" | "https" => match spotify_web_link(rest) {
            Some(link) => link.map_err(|why| {
                Failure::invalid(format!(
                    "source_uri {uri:?} is not a playable Spotify link: {why}"
                ))
            }),
            None => Ok(uri.to_string()),
        },
        _ => Ok(uri.to_string()),
    }
}

/// `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` (RFC 3986 §3.1).
fn is_uri_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `spotify:<kind>:<id>`, plus the legacy `spotify:user:<name>:playlist:<id>`.
fn spotify_uri(uri: &str, rest: &str) -> Result<String, Failure> {
    let parts: Vec<&str> = rest.split(':').collect();
    let (kind, id) = match parts.as_slice() {
        [kind, id] => (*kind, *id),
        [user, _, playlist, id]
            if user.eq_ignore_ascii_case("user") && playlist.eq_ignore_ascii_case("playlist") =>
        {
            ("playlist", *id)
        }
        _ => {
            return Err(Failure::invalid(format!(
                "source_uri {uri:?} is not a playable Spotify URI; expected spotify:<kind>:<id>"
            )));
        }
    };
    canonical_spotify(kind, id)
        .map_err(|why| Failure::invalid(format!("source_uri {uri:?}: {why}")))
}

/// Recognize a Spotify web link (`rest` is everything after `http:`/`https:`).
/// `None` means "not a Spotify link at all" (pass it through); `Some(Err)`
/// means it is one, but not one we can play.
fn spotify_web_link(rest: &str) -> Option<Result<String, String>> {
    let after_slashes = rest.strip_prefix("//")?;
    let (host, path) = after_slashes.split_once('/').unwrap_or((after_slashes, ""));
    let host = host.to_ascii_lowercase();
    if host == "spotify.link" || host.ends_with(".spotify.link") {
        return Some(Err(
            "short spotify.link URLs need a network lookup; open it and pass the open.spotify.com link it leads to"
                .to_string(),
        ));
    }
    if host != "open.spotify.com" && host != "play.spotify.com" {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.first().is_some_and(|s| s.starts_with("intl-")) {
        segments.remove(0);
    }
    if segments.first() == Some(&"embed") {
        segments.remove(0);
    }
    Some(match segments.as_slice() {
        [kind, id] => canonical_spotify(kind, id),
        ["user", _, "playlist", id] => canonical_spotify("playlist", id),
        _ => Err("expected https://open.spotify.com/<kind>/<id>".to_string()),
    })
}

fn canonical_spotify(kind: &str, id: &str) -> Result<String, String> {
    let kind = kind.to_ascii_lowercase();
    if !SPOTIFY_KINDS.contains(&kind.as_str()) {
        return Err(format!(
            "unsupported Spotify kind {kind:?}; expected one of {}",
            SPOTIFY_KINDS.join(", ")
        ));
    }
    if id.len() != 22 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!(
            "Spotify id {id:?} is malformed; ids are 22 letters and digits"
        ));
    }
    Ok(format!("spotify:{kind}:{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789ABCDEFabcdef";

    fn ok(raw: &str) -> String {
        normalize_source_uri(raw).unwrap_or_else(|f| panic!("{raw:?} rejected: {f}"))
    }

    fn rejected(raw: &str) -> String {
        match normalize_source_uri(raw) {
            Ok(uri) => panic!("{raw:?} accepted as {uri:?}"),
            Err(f) => {
                assert_eq!(f.status, 422, "{raw:?}");
                f.detail
            }
        }
    }

    #[test]
    fn spotify_uris_are_canonical() {
        assert_eq!(
            ok(&format!("spotify:track:{ID}")),
            format!("spotify:track:{ID}")
        );
        assert_eq!(
            ok(&format!("  Spotify:Album:{ID} ")),
            format!("spotify:album:{ID}")
        );
        assert_eq!(
            ok(&format!("spotify:user:someone:playlist:{ID}")),
            format!("spotify:playlist:{ID}")
        );
    }

    #[test]
    fn share_links_become_spotify_uris() {
        for (link, want) in [
            (
                format!("https://open.spotify.com/track/{ID}?si=abc123"),
                format!("spotify:track:{ID}"),
            ),
            (
                format!("https://open.spotify.com/intl-de/album/{ID}"),
                format!("spotify:album:{ID}"),
            ),
            (
                format!("http://open.spotify.com/playlist/{ID}/#top"),
                format!("spotify:playlist:{ID}"),
            ),
            (
                format!("https://OPEN.SPOTIFY.COM/embed/artist/{ID}"),
                format!("spotify:artist:{ID}"),
            ),
            (
                format!("https://open.spotify.com/user/x/playlist/{ID}"),
                format!("spotify:playlist:{ID}"),
            ),
            (
                format!("https://play.spotify.com/episode/{ID}"),
                format!("spotify:episode:{ID}"),
            ),
        ] {
            assert_eq!(ok(&link), want, "{link}");
        }
    }

    #[test]
    fn other_uris_pass_through_untouched() {
        for uri in [
            "x-sonos-spotify:spotify%3atrack%3aabc?sid=12&flags=8224&sn=1",
            "x-rincon-mp3radio://stream.example.org/live.mp3",
            "https://stream.example.org/radio.aac",
            "x-rincon-cpcontainer:1006206cplaylist",
        ] {
            assert_eq!(ok(uri), uri);
        }
    }

    #[test]
    fn malformed_spotify_ids_are_caught_here() {
        assert!(rejected("spotify:track:0123456789ABCDEFabcde").contains("malformed"));
        assert!(rejected(&format!("spotify:track:{ID}x")).contains("malformed"));
        assert!(rejected("https://open.spotify.com/track/01234567").contains("malformed"));
    }

    #[test]
    fn unplayable_spotify_shapes_say_why() {
        assert!(rejected(&format!("spotify:genre:{ID}")).contains("unsupported Spotify kind"));
        assert!(rejected("spotify:user:someone:collection").contains("spotify:<kind>:<id>"));
        assert!(rejected("https://open.spotify.com/").contains("open.spotify.com/<kind>/<id>"));
        assert!(rejected("https://spotify.link/AbCdEf").contains("network lookup"));
    }

    #[test]
    fn non_uris_are_rejected() {
        assert!(rejected("").contains("empty"));
        assert!(rejected("   ").contains("empty"));
        assert!(rejected("Bach cello suites").contains("whitespace"));
        assert!(rejected("bach-cello-suites").contains("no scheme"));
        assert!(rejected("1x:foo").contains("not a URI"));
        assert!(rejected("spotify:").contains("not a URI"));
        assert!(
            rejected(&format!("x-sonos-http:{}", "a".repeat(MAX_SOURCE_URI_LEN))).contains("limit")
        );
    }
}
