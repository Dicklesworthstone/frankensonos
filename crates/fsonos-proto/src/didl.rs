//! DIDL-Lite metadata and renderer-facing URI construction.
//!
//! Pure string building — no I/O — so it is fully unit-testable today. The
//! exact `x-sonos-spotify:` URI shape and `SA_RINCON...` descriptor differ
//! between households and are resolved empirically from a player's own
//! favorites (see the plan, "Spotify on Sonos"). These helpers take those
//! parameters explicitly rather than hard-coding one household's values.
//!
//! [`parse_didl`] reads the DIDL-Lite documents ContentDirectory returns
//! (favorites, the queue), including the Sonos `r:` extensions favorites carry.

use crate::{ProtoError, xml};
use fsonos_types::Track;

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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Learn a household's Spotify render parameters from its own favorites
/// (ContentDirectory `FV:2`). Every Spotify track favorite carries `sid`,
/// `flags` and `sn` in its URI and the service-account descriptor and item-id
/// prefix in its metadata. One household's favorites vary (flags 8224/8232,
/// prefixes 10032020/00032020/10032028, all accepted by the player), so the
/// result is the set most of them use. It is always a set some favorite
/// really uses: fields chosen one by one could combine into a set none does.
/// Ties go to the set whose values are individually most common, then to the
/// earlier favorite. `None` when the household has no Spotify track favorite
/// to learn from.
#[must_use]
pub fn learn_spotify_params(favorites: &[DidlObject]) -> Option<SpotifyRenderParams> {
    let mut seen: Vec<SpotifyRenderParams> = Vec::new();
    for fav in favorites {
        let Some(res) = &fav.res else { continue };
        let Some(query) = res
            .uri
            .strip_prefix("x-sonos-spotify:")
            .and_then(|rest| rest.split_once('?'))
            .map(|(_, q)| q)
        else {
            continue;
        };
        let param = |name: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
                .and_then(|v| v.parse::<u32>().ok())
        };
        let (Some(sid), Some(flags), Some(sn)) = (param("sid"), param("flags"), param("sn")) else {
            continue;
        };
        let Ok(Some(item)) = fav.res_md_object() else {
            continue;
        };
        let Some(desc) = item.desc.filter(|d| d.id == "cdudn" && !d.value.is_empty()) else {
            continue;
        };
        let Some(prefix_len) = item.id.to_ascii_lowercase().find("spotify%3a") else {
            continue;
        };
        seen.push(SpotifyRenderParams {
            sid,
            flags,
            sn,
            cdudn: desc.value,
            item_id_prefix: item.id[..prefix_len].to_string(),
        });
    }
    let count =
        |same: &dyn Fn(&SpotifyRenderParams) -> bool| seen.iter().filter(|q| same(q)).count();
    let score = |p: &SpotifyRenderParams| {
        (
            count(&|q| q == p),
            count(&|q| q.sid == p.sid)
                + count(&|q| q.flags == p.flags)
                + count(&|q| q.sn == p.sn)
                + count(&|q| q.cdudn == p.cdudn)
                + count(&|q| q.item_id_prefix == p.item_id_prefix),
        )
    };
    let mut best: Option<(&SpotifyRenderParams, (usize, usize))> = None;
    for p in &seen {
        let s = score(p);
        if best.is_none_or(|(_, top)| s > top) {
            best = Some((p, s));
        }
    }
    best.map(|(p, _)| p.clone())
}

/// The DIDL-Lite metadata a `spotify:track:<id>` needs on this household, for
/// `SetAVTransportURI` (with [`spotify_track_uri`]) or `AddURIToQueue`. The
/// `desc` must name the household's own Spotify service account: a wrong one
/// fails with UPnP 800. `title` is display-only; the player swaps in the real
/// metadata once playback starts.
#[must_use]
pub fn spotify_track_didl(spotify_uri: &str, title: &str, p: &SpotifyRenderParams) -> String {
    let encoded = spotify_uri.replace(':', "%3a");
    format!(
        "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
         xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\" \
         xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\
         <item id=\"{prefix}{encoded}\" parentID=\"-1\" restricted=\"true\">\
         <dc:title>{title}</dc:title>\
         <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{cdudn}</desc>\
         </item></DIDL-Lite>",
        prefix = xml_escape(&p.item_id_prefix),
        encoded = xml_escape(&encoded),
        title = xml_escape(title),
        cdudn = xml_escape(&p.cdudn),
    )
}

/// The `AddURIToQueue` URI for a `spotify:track:<id>`: the bare,
/// percent-encoded Spotify URI, with no `x-sonos-spotify:` scheme or query
/// (the form verified live on S1; see docs/PROTOCOL.md, "Queue flow"). Pair
/// it with [`spotify_track_didl`].
#[must_use]
pub fn spotify_queue_uri(spotify_uri: &str) -> String {
    spotify_uri.replace(':', "%3a")
}

/// Recover the service-facing `spotify:…` URI from a renderer URI such as
/// `x-sonos-spotify:spotify%3atrack%3a<id>?sid=…` or
/// `x-rincon-cpcontainer:1004206cspotify%3aalbum%3a<id>?sid=…` (the inverse
/// of [`spotify_track_uri`]). `None` for non-Spotify URIs.
#[must_use]
pub fn spotify_uri_from_renderer_uri(uri: &str) -> Option<String> {
    let (_, rest) = uri.split_once(':')?;
    let path = rest.split('?').next()?;
    let start = path.to_ascii_lowercase().find("spotify%3a")?;
    let decoded = percent_decode(&path[start..])?;
    decoded.starts_with("spotify:").then_some(decoded)
}

/// Decode `%XX` escapes. `None` on a malformed escape or non-UTF-8 result.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether a DIDL-Lite object is an `<item>` or a `<container>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DidlKind {
    Item,
    Container,
}

/// A `<res>` element: the renderer-facing URI of an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DidlRes {
    pub uri: String,
    pub protocol_info: Option<String>,
    /// `H:MM:SS` as Sonos reports it on queue items.
    pub duration: Option<String>,
}

impl DidlRes {
    /// [`Self::duration`] in whole seconds.
    #[must_use]
    pub fn duration_secs(&self) -> Option<u32> {
        let d = self.duration.as_deref()?;
        let whole = d.split('.').next()?;
        let mut parts = whole.split(':').map(str::parse::<u32>);
        let (h, m, s) = (
            parts.next()?.ok()?,
            parts.next()?.ok()?,
            parts.next()?.ok()?,
        );
        if parts.next().is_some() || m >= 60 || s >= 60 {
            return None;
        }
        // `h` is unbounded network input; avoid a debug panic / release wrap.
        // (m*60+s is bounded by the <60 checks above, so only h*3600 can overflow.)
        h.checked_mul(3600)?.checked_add(m * 60 + s)
    }
}

/// A `<desc>` element. On Sonos favorites the `cdudn` descriptor names the
/// music-service account (e.g. `SA_RINCON3079_X_#Svc3079-0-Token`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DidlDesc {
    pub id: String,
    pub name_space: String,
    pub value: String,
}

/// One DIDL-Lite `<item>` or `<container>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DidlObject {
    pub kind: DidlKind,
    pub id: String,
    pub parent_id: String,
    pub restricted: bool,
    pub title: String,
    /// `upnp:class`, e.g. `object.item.audioItem.musicTrack`.
    pub class: String,
    pub creator: Option<String>,
    pub album: Option<String>,
    pub album_art_uri: Option<String>,
    /// `None` when absent or empty (favorite shortcuts carry `<res/>`).
    pub res: Option<DidlRes>,
    pub desc: Option<DidlDesc>,
    /// Favorites only (`r:type`): `instantPlay` or `shortcut`.
    pub favorite_type: Option<String>,
    /// Favorites only (`r:description`), e.g. `By <artist>`.
    pub favorite_description: Option<String>,
    /// Favorites only (`r:ordinal`): position in the favorites list.
    pub ordinal: Option<u32>,
    /// Favorites only (`r:resMD`): the DIDL-Lite metadata Sonos replays the
    /// favorite with, entity-decoded once. Parse it with [`Self::res_md_object`].
    pub res_md: Option<String>,
}

impl DidlObject {
    /// Parse [`Self::res_md`] and return its single object, if any.
    pub fn res_md_object(&self) -> Result<Option<DidlObject>, ProtoError> {
        match &self.res_md {
            Some(md) => Ok(parse_didl(md)?.into_iter().next()),
            None => Ok(None),
        }
    }

    /// View a playable item as a [`Track`]. `source_uri` is the `spotify:…`
    /// URI when the resource is a Spotify one, otherwise the renderer URI.
    #[must_use]
    pub fn to_track(&self) -> Option<Track> {
        let res = self.res.as_ref()?;
        Some(Track {
            title: self.title.clone(),
            artist: self.creator.clone(),
            album: self.album.clone(),
            source_uri: spotify_uri_from_renderer_uri(&res.uri).unwrap_or_else(|| res.uri.clone()),
            uri: Some(res.uri.clone()),
            duration_secs: res.duration_secs(),
        })
    }
}

/// Parse a decoded DIDL-Lite document into its objects, in document order.
/// An empty string (or an empty `<DIDL-Lite/>`) yields no objects.
pub fn parse_didl(doc_text: &str) -> Result<Vec<DidlObject>, ProtoError> {
    if doc_text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let doc = xml::parse(doc_text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "DIDL-Lite" {
        return Err(ProtoError::Malformed(format!(
            "expected DIDL-Lite, got <{}>",
            root.tag_name().name()
        )));
    }
    root.children()
        .filter(roxmltree::Node::is_element)
        .filter_map(|node| match node.tag_name().name() {
            "item" => Some(parse_object(node, DidlKind::Item)),
            "container" => Some(parse_object(node, DidlKind::Container)),
            _ => None,
        })
        .collect()
}

fn parse_object(node: roxmltree::Node<'_, '_>, kind: DidlKind) -> Result<DidlObject, ProtoError> {
    let text = |name| xml::child_text_nonempty(node, name).map(str::to_string);
    let res = xml::child(node, "res").and_then(|r| {
        let uri = r.text().unwrap_or("").trim();
        (!uri.is_empty()).then(|| DidlRes {
            uri: uri.to_string(),
            protocol_info: r.attribute("protocolInfo").map(str::to_string),
            duration: r.attribute("duration").map(str::to_string),
        })
    });
    let desc = xml::child(node, "desc").map(|d| DidlDesc {
        id: d.attribute("id").unwrap_or_default().to_string(),
        name_space: d.attribute("nameSpace").unwrap_or_default().to_string(),
        value: d.text().unwrap_or("").to_string(),
    });
    Ok(DidlObject {
        kind,
        id: xml::require_attr(node, "id")?.to_string(),
        parent_id: node.attribute("parentID").unwrap_or_default().to_string(),
        restricted: matches!(node.attribute("restricted"), Some("true" | "1")),
        title: xml::child_text(node, "title").unwrap_or("").to_string(),
        class: xml::child_text(node, "class").unwrap_or("").to_string(),
        creator: text("creator"),
        album: text("album"),
        album_art_uri: text("albumArtURI"),
        res,
        desc,
        favorite_type: text("type"),
        favorite_description: text("description"),
        ordinal: xml::child_text(node, "ordinal").and_then(|o| o.trim().parse().ok()),
        res_md: text("resMD"),
    })
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

    #[test]
    fn spotify_uri_round_trips_through_renderer_uri() {
        let p = SpotifyRenderParams {
            sid: 12,
            flags: 8224,
            sn: 1,
            cdudn: "SA_RINCON3079_X_#Svc3079-0-Token".into(),
            item_id_prefix: "10032020".into(),
        };
        let src = "spotify:track:abc123";
        assert_eq!(
            spotify_uri_from_renderer_uri(&spotify_track_uri(src, &p)).as_deref(),
            Some(src)
        );
        assert_eq!(
            spotify_uri_from_renderer_uri(
                "x-rincon-cpcontainer:1004206cspotify%3Aalbum%3AXyZ?sid=12&flags=8300&sn=1"
            )
            .as_deref(),
            Some("spotify:album:XyZ")
        );
        assert_eq!(
            spotify_uri_from_renderer_uri("x-rincon-mp3radio://http://192.0.2.1/a.mp3"),
            None
        );
        assert_eq!(
            spotify_uri_from_renderer_uri("x-sonos-spotify:spotify%3atrack%3a%ZZ"),
            None
        );
    }

    #[test]
    fn durations_parse() {
        let res = |d: &str| DidlRes {
            uri: "u".into(),
            protocol_info: None,
            duration: Some(d.into()),
        };
        assert_eq!(res("0:03:59").duration_secs(), Some(239));
        assert_eq!(res("1:00:00.500").duration_secs(), Some(3600));
        assert_eq!(res("0:61:00").duration_secs(), None);
        assert_eq!(res("junk").duration_secs(), None);
    }

    #[test]
    fn parses_items_containers_and_skips_other_elements() {
        let doc = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
            xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
            <container id="SQ:1" parentID="SQ:" restricted="true"><dc:title>Mix</dc:title>
              <upnp:class>object.container.playlistContainer</upnp:class><res>file:///jffs/settings/savedqueues.rsq#1</res></container>
            <note/>
            <item id="Q:0/1" parentID="Q:0"><dc:title>A &amp; B</dc:title><res/></item>
            </DIDL-Lite>"#;
        let objs = parse_didl(doc).unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].kind, DidlKind::Container);
        assert!(objs[0].restricted);
        assert_eq!(objs[1].title, "A & B");
        assert!(!objs[1].restricted);
        assert!(objs[1].res.is_none());
        assert!(objs[1].to_track().is_none());
        assert_eq!(parse_didl("").unwrap().len(), 0);
        assert!(matches!(parse_didl("<x/>"), Err(ProtoError::Malformed(_))));
    }

    fn favorites(body: &str) -> Vec<DidlObject> {
        crate::content::parse_browse_response(body).unwrap().objects
    }

    const FAV_S1: &str = include_str!("../tests/fixtures/browse_favorites_s1.xml");
    const FAV_S2: &str = include_str!("../tests/fixtures/browse_favorites_s2.xml");

    #[test]
    fn learns_render_params_from_each_households_favorites() {
        for body in [FAV_S1, FAV_S2] {
            let p = learn_spotify_params(&favorites(body)).expect("a Spotify track favorite");
            assert!(
                p.cdudn.starts_with("SA_RINCON") && p.cdudn.ends_with("-0-Token"),
                "{p:?}"
            );
            assert!(
                p.item_id_prefix.len() == 8
                    && p.item_id_prefix.chars().all(|c| c.is_ascii_hexdigit())
            );
            assert!(p.sid > 0 && p.flags > 0, "{p:?}");
        }
    }

    #[test]
    fn varying_favorites_yield_their_most_common_values() {
        let favs = favorites(FAV_S2);
        let p = learn_spotify_params(&favs).unwrap();
        // Count what the household's own track favorites use, independently.
        let mut flags: Vec<(u32, usize)> = Vec::new();
        for f in &favs {
            let Some(uri) = f.res.as_ref().map(|r| r.uri.as_str()) else {
                continue;
            };
            let Some(v) = uri
                .strip_prefix("x-sonos-spotify:")
                .and_then(|r| r.split("flags=").nth(1))
                .and_then(|r| r.split('&').next())
                .and_then(|v| v.parse::<u32>().ok())
            else {
                continue;
            };
            match flags.iter_mut().find(|(f, _)| *f == v) {
                Some((_, n)) => *n += 1,
                None => flags.push((v, 1)),
            }
        }
        let top = flags.iter().map(|(_, n)| *n).max().unwrap();
        assert!(
            flags.iter().any(|(f, n)| *f == p.flags && *n == top),
            "{flags:?} vs {}",
            p.flags
        );
    }

    #[test]
    fn the_learned_set_is_one_a_favorite_really_uses() {
        for body in [FAV_S1, FAV_S2] {
            let favs = favorites(body);
            let p = learn_spotify_params(&favs).unwrap();
            let used = favs.iter().any(|f| {
                let Some(uri) = f.res.as_ref().map(|r| r.uri.as_str()) else {
                    return false;
                };
                let Some(spotify) = spotify_uri_from_renderer_uri(uri) else {
                    return false;
                };
                let Ok(Some(item)) = f.res_md_object() else {
                    return false;
                };
                uri.starts_with("x-sonos-spotify:")
                    && spotify_track_uri(&spotify, &p).eq_ignore_ascii_case(uri)
                    && item
                        .id
                        .to_ascii_lowercase()
                        .starts_with(&p.item_id_prefix.to_ascii_lowercase())
                    && item.desc.is_some_and(|d| d.value == p.cdudn)
            });
            assert!(used, "{p:?} is no favorite's set");
        }
    }

    #[test]
    fn a_learned_favorite_round_trips_to_its_renderer_uri() {
        let favs = favorites(FAV_S1);
        let p = learn_spotify_params(&favs).unwrap();
        let matching = favs
            .iter()
            .filter_map(|f| f.res.as_ref().map(|r| r.uri.clone()))
            .filter(|uri| {
                uri.starts_with("x-sonos-spotify:") && uri.contains(&format!("flags={}", p.flags))
            })
            .collect::<Vec<_>>();
        let any_match = !matching.is_empty();
        assert!(any_match, "no favorite carried the learned flags");
        for uri in matching {
            let spotify = spotify_uri_from_renderer_uri(&uri).unwrap();
            assert!(
                spotify_track_uri(&spotify, &p).eq_ignore_ascii_case(&uri),
                "{spotify} -> {} != {uri}",
                spotify_track_uri(&spotify, &p)
            );
        }
    }

    #[test]
    fn track_didl_carries_the_households_descriptor_and_prefix() {
        let p = SpotifyRenderParams {
            sid: 12,
            flags: 8224,
            sn: 1,
            cdudn: "SA_RINCON3079_X_#Svc3079-0-Token".into(),
            item_id_prefix: "10032020".into(),
        };
        let didl = spotify_track_didl(
            "spotify:track:0FixtureSpotify0000001",
            "Aria & Variations",
            &p,
        );
        let item = parse_didl(&didl).unwrap().pop().unwrap();
        assert_eq!(item.id, "10032020spotify%3atrack%3a0FixtureSpotify0000001");
        assert_eq!(item.parent_id, "-1");
        assert_eq!(item.title, "Aria & Variations");
        assert_eq!(item.class, "object.item.audioItem.musicTrack");
        let desc = item.desc.unwrap();
        assert_eq!(
            (desc.id.as_str(), desc.value.as_str()),
            ("cdudn", "SA_RINCON3079_X_#Svc3079-0-Token")
        );
    }

    #[test]
    fn queue_uri_is_the_bare_encoded_spotify_uri() {
        assert_eq!(
            spotify_queue_uri("spotify:track:0FixtureSpotify0000001"),
            "spotify%3atrack%3a0FixtureSpotify0000001"
        );
    }

    #[test]
    fn no_spotify_favorite_means_nothing_to_learn() {
        assert_eq!(learn_spotify_params(&[]), None);
        let radio_only: Vec<DidlObject> = favorites(FAV_S1)
            .into_iter()
            .filter(|f| {
                f.res
                    .as_ref()
                    .is_some_and(|r| !r.uri.starts_with("x-sonos-spotify:"))
            })
            .collect();
        assert_eq!(learn_spotify_params(&radio_only), None);
    }
}
