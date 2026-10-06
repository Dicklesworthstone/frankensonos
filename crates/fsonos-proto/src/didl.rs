//! DIDL-Lite metadata and renderer-facing URI construction.
//!
//! Pure string building — no I/O — so it is fully unit-testable today. The
//! exact `x-sonos-spotify:` URI shape and `SA_RINCON...` descriptor differ
//! between households and are resolved empirically from a player's own
//! favorites (see the plan, "Spotify on Sonos"). These helpers take those
//! parameters explicitly rather than hard-coding one household's values.

/// Escape a string for inclusion in XML character data / attributes.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Parameters needed to render a Spotify track on a specific household. These
/// are discovered from the household's own favorites, never guessed.
#[derive(Debug, Clone)]
pub struct SpotifyRenderParams {
    pub sid: u32,
    pub flags: u32,
    pub sn: u32,
    /// The `SA_RINCON<service_type>_X_#Svc<service_type>-0-Token` descriptor.
    pub cdudn: String,
    /// The item-id prefix observed in the household's favorites (e.g. 10032020).
    pub item_id_prefix: String,
}

/// Build the renderer-facing `x-sonos-spotify:` URI for a `spotify:track:<id>`.
#[must_use]
pub fn spotify_track_uri(spotify_uri: &str, p: &SpotifyRenderParams) -> String {
    let encoded = spotify_uri.replace(':', "%3a");
    format!(
        "x-sonos-spotify:{encoded}?sid={sid}&flags={flags}&sn={sn}",
        sid = p.sid,
        flags = p.flags,
        sn = p.sn
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml() {
        assert_eq!(xml_escape("a&b<c>"), "a&amp;b&lt;c&gt;");
    }

    #[test]
    fn builds_spotify_uri() {
        let p = SpotifyRenderParams {
            sid: 12,
            flags: 8224,
            sn: 1,
            cdudn: "SA_RINCON3079_X_#Svc3079-0-Token".into(),
            item_id_prefix: "10032020".into(),
        };
        let u = spotify_track_uri("spotify:track:abc123", &p);
        assert_eq!(
            u,
            "x-sonos-spotify:spotify%3atrack%3aabc123?sid=12&flags=8224&sn=1"
        );
    }
}
