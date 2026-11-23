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
use std::collections::HashMap;
use std::fmt::Write as _;

mod native_metadata;
pub use native_metadata::{
    NativeDidlObject, native_account_descriptor, parse_didl_with_metadata,
    with_native_replay_identity,
};

/// Escape a string for inclusion in XML character data / attributes.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Compare supplied DIDL metadata without depending on its XML spelling.
/// Namespace prefixes, attribute order, entity quoting and indentation may
/// change on read-back. Expanded names, values and element order must still
/// match, including unknown service extensions and account descriptors.
/// Missing or malformed documents cannot establish equivalence.
#[must_use]
pub fn equivalent_metadata(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (xml::parse(left), xml::parse(right)) else {
        return false;
    };
    let (left, right) = (left.root_element(), right.root_element());
    if left.tag_name().name() != "DIDL-Lite" || right.tag_name().name() != "DIDL-Lite" {
        return false;
    }
    let mut pending = vec![(left, right)];
    while let Some((left, right)) = pending.pop() {
        if left.tag_name() != right.tag_name() {
            return false;
        }
        let mut left_attributes: Vec<_> = left
            .attributes()
            .map(|attr| (attr.namespace(), attr.name(), attr.value()))
            .collect();
        let mut right_attributes: Vec<_> = right
            .attributes()
            .map(|attr| (attr.namespace(), attr.name(), attr.value()))
            .collect();
        left_attributes.sort_unstable();
        right_attributes.sort_unstable();
        if left_attributes != right_attributes {
            return false;
        }
        let (left, right) = (metadata_children(left), metadata_children(right));
        if left.len() != right.len() {
            return false;
        }
        for (left, right) in left.into_iter().zip(right) {
            match (left, right) {
                (MetadataChild::Element(left), MetadataChild::Element(right)) => {
                    pending.push((left, right));
                }
                (MetadataChild::Text(left), MetadataChild::Text(right)) if left == right => {}
                _ => return false,
            }
        }
    }
    true
}

enum MetadataChild<'a, 'input> {
    Element(roxmltree::Node<'a, 'input>),
    Text(String),
}

fn metadata_children<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
) -> Vec<MetadataChild<'a, 'input>> {
    let preserve_space = node
        .ancestors()
        .find_map(|parent| parent.attribute(("http://www.w3.org/XML/1998/namespace", "space")))
        == Some("preserve");
    // Only element-only content has ignorable formatting whitespace. Keep
    // text-only and mixed content verbatim, including vendor extensions.
    let indentation = !preserve_space
        && node.children().any(|child| child.is_element())
        && node
            .children()
            .filter(roxmltree::Node::is_text)
            .all(|child| child.text().is_none_or(|text| text.trim().is_empty()));
    let mut children = Vec::new();
    for child in node.children() {
        if child.is_element() {
            children.push(MetadataChild::Element(child));
        } else if child.is_text() && !indentation {
            let text = child.text().unwrap_or_default();
            // Comments and CDATA boundaries do not change character data.
            if let Some(MetadataChild::Text(previous)) = children.last_mut() {
                previous.push_str(text);
            } else if !text.is_empty() {
                children.push(MetadataChild::Text(text.into()));
            }
        }
    }
    children
}

/// Serialize the playback metadata represented by one item or container in
/// the protocol model, including its service descriptor and resource attributes. Queue
/// IDs remain metadata; callers must not use them as stable track identities.
#[must_use]
pub fn item_metadata(item: &DidlObject) -> String {
    let element = match item.kind {
        DidlKind::Item => "item",
        DidlKind::Container => "container",
    };
    let mut xml = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
         xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">\
         <{element} id=\"{}\" parentID=\"{}\" restricted=\"{}\">",
        xml_escape(&item.id),
        xml_escape(&item.parent_id),
        item.restricted,
    );
    for (tag, value) in [
        ("dc:title", Some(item.title.as_str())),
        ("upnp:class", Some(item.class.as_str())),
        ("dc:creator", item.creator.as_deref()),
        ("upnp:album", item.album.as_deref()),
        ("upnp:albumArtURI", item.album_art_uri.as_deref()),
    ] {
        if let Some(value) = value {
            let _ = write!(xml, "<{tag}>{}</{tag}>", xml_escape(value));
        }
    }
    if let Some(res) = &item.res {
        xml.push_str("<res");
        for (name, value) in [
            ("protocolInfo", res.protocol_info.as_deref()),
            ("duration", res.duration.as_deref()),
        ] {
            if let Some(value) = value {
                let _ = write!(xml, " {name}=\"{}\"", xml_escape(value));
            }
        }
        let _ = write!(xml, ">{}</res>", xml_escape(&res.uri));
    }
    if let Some(desc) = &item.desc {
        let _ = write!(
            xml,
            "<desc id=\"{}\" nameSpace=\"{}\">{}</desc>",
            xml_escape(&desc.id),
            xml_escape(&desc.name_space),
            xml_escape(&desc.value)
        );
    }
    let _ = write!(xml, "</{element}></DIDL-Lite>");
    xml
}

/// Keep the source's native metadata bytes while removing artwork elements
/// from a favorite's nested `r:resMD`. Sonos stores art on the outer favorite;
/// repeating long, doubly escaped URLs in resMD can exceed its native limit.
/// Structural ranges preserve service descriptors and unknown extensions.
pub fn favorite_metadata(metadata: &str) -> Result<String, ProtoError> {
    let document = xml::parse(metadata)?;
    if document.root_element().tag_name().name() != "DIDL-Lite" {
        return Err(ProtoError::Malformed(
            "favorite metadata must be DIDL-Lite".into(),
        ));
    }
    let mut ranges: Vec<_> = document
        .descendants()
        .filter(|node| {
            node.has_tag_name(("urn:schemas-upnp-org:metadata-1-0/upnp/", "albumArtURI"))
        })
        .filter(|node| {
            !node.ancestors().skip(1).any(|parent| {
                parent.has_tag_name(("urn:schemas-upnp-org:metadata-1-0/upnp/", "albumArtURI"))
            })
        })
        .map(|node| node.range())
        .collect();
    ranges.sort_by_key(|range| range.start);
    let mut result = metadata.to_owned();
    for range in ranges.into_iter().rev() {
        result.replace_range(range, "");
    }
    Ok(result)
}

/// Parameters needed to render a Spotify track on a specific household. These
/// are discovered from the household's own favorites, never guessed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    spotify_track_templates(favorites).into_iter().next()
}

/// Every distinct track-rendering template observed in this household's
/// favorites, in the same preference order as [`learn_spotify_params`]. A
/// definite renderer refusal can select the next complete template without
/// mixing account fields or repeatedly choosing a stale majority. Ties retain
/// favorite order. Parsing and frequency counting happen once per observation.
#[must_use]
pub fn spotify_track_templates(favorites: &[DidlObject]) -> Vec<SpotifyRenderParams> {
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
    let mut sets = HashMap::new();
    let mut services = HashMap::new();
    let mut flags = HashMap::new();
    let mut accounts = HashMap::new();
    let mut descriptors = HashMap::new();
    let mut prefixes = HashMap::new();
    for (index, params) in seen.iter().enumerate() {
        sets.entry(params).or_insert((index, 0_usize)).1 += 1;
        *services.entry(params.sid).or_insert(0_usize) += 1;
        *flags.entry(params.flags).or_insert(0_usize) += 1;
        *accounts.entry(params.sn).or_insert(0_usize) += 1;
        *descriptors.entry(params.cdudn.as_str()).or_insert(0_usize) += 1;
        *prefixes
            .entry(params.item_id_prefix.as_str())
            .or_insert(0_usize) += 1;
    }
    let mut ranked: Vec<_> = sets
        .into_iter()
        .map(|(params, (first, count))| {
            let shared = services[&params.sid]
                + flags[&params.flags]
                + accounts[&params.sn]
                + descriptors[params.cdudn.as_str()]
                + prefixes[params.item_id_prefix.as_str()];
            (params, (count, shared), first)
        })
        .collect();
    ranked.sort_unstable_by_key(|(_, score, first)| (std::cmp::Reverse(*score), *first));
    ranked
        .into_iter()
        .map(|(params, _, _)| params.clone())
        .collect()
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

/// Spotify kinds the speaker renders from the bare URI plus the household
/// account id, without the per-track `flags`/`sn`/descriptor parameters:
/// an artist, an episode or a show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotifyBare {
    Artist,
    Episode,
    Show,
}

impl SpotifyBare {
    /// The kind a `spotify:artist:<id>`, `spotify:episode:<id>` or
    /// `spotify:show:<id>` names.
    #[must_use]
    pub fn of(spotify_uri: &str) -> Option<Self> {
        match spotify_uri.strip_prefix("spotify:")?.split(':').next()? {
            "artist" => Some(Self::Artist),
            "episode" => Some(Self::Episode),
            "show" => Some(Self::Show),
            _ => None,
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Artist => "object.container.person.musicArtist",
            Self::Episode => "object.item.audioItem.musicTrack",
            Self::Show => "object.container",
        }
    }
}

/// The renderer-facing URI for a bare Spotify kind: the `x-sonos-spotify:`
/// scheme, the percent-encoded Spotify URI and the household's service id.
/// The speaker refuses these kinds with 402 when `flags`, `sn` or the
/// descriptor are also present, so only `sid` is appended. All three are
/// accepted by `SetAVTransportURI` and `AddURIToQueue` on S1 57.23.
#[must_use]
pub fn spotify_bare_uri(spotify_uri: &str, sid: u32) -> String {
    format!(
        "x-sonos-spotify:{}?sid={sid}",
        spotify_uri.replace(':', "%3a")
    )
}

/// DIDL metadata for a bare Spotify kind, with the class the renderer
/// expects for it. No descriptor is carried: these kinds render on the URI's
/// account id alone.
#[must_use]
pub fn spotify_bare_didl(spotify_uri: &str, title: &str, sid: u32) -> String {
    let class = SpotifyBare::of(spotify_uri).map_or("object.item", SpotifyBare::class);
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
<item id=\"1\" parentID=\"0\" restricted=\"true\"><dc:title>{title}</dc:title>\
<upnp:class>{class}</upnp:class><res>{uri}</res></item></DIDL-Lite>",
        title = xml_escape(title),
        uri = xml_escape(&spotify_bare_uri(spotify_uri, sid)),
    )
}

/// A Spotify album or playlist: a container the speaker expands into its
/// tracks when it is enqueued (docs/PROTOCOL.md, "Container playback").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotifyContainer {
    Album,
    Playlist,
}

/// The `flags` every Spotify container favorite carries.
const CONTAINER_FLAGS: u32 = 8300;

impl SpotifyContainer {
    /// The container a `spotify:album:<id>` or `spotify:playlist:<id>` names.
    #[must_use]
    pub fn of(spotify_uri: &str) -> Option<Self> {
        let rest = spotify_uri.strip_prefix("spotify:")?;
        match rest.split(':').next()? {
            "album" => Some(Self::Album),
            "playlist" => Some(Self::Playlist),
            _ => None,
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Album => "object.container.album.musicAlbum",
            Self::Playlist => "object.container.playlistContainer",
        }
    }

    /// The item-id prefix the household's own favorites of this kind use
    /// most (playlists vary: `1006206c`, `10062a6c`), else the common one.
    #[must_use]
    pub fn prefix_in(self, favorites: &[DidlObject]) -> String {
        let mut seen: Vec<(String, usize)> = Vec::new();
        for fav in favorites {
            let Ok(Some(item)) = fav.res_md_object() else {
                continue;
            };
            if item.class != self.class() {
                continue;
            }
            let Some(at) = item.id.to_ascii_lowercase().find("spotify%3a") else {
                continue;
            };
            let prefix = item.id[..at].to_string();
            match seen.iter_mut().find(|(p, _)| *p == prefix) {
                Some((_, n)) => *n += 1,
                None => seen.push((prefix, 1)),
            }
        }
        let mut best: Option<&(String, usize)> = None;
        for entry in &seen {
            if best.is_none_or(|top| entry.1 > top.1) {
                best = Some(entry);
            }
        }
        best.map_or_else(
            || {
                match self {
                    Self::Album => "1004206c",
                    Self::Playlist => "1006206c",
                }
                .to_string()
            },
            |(prefix, _)| prefix.clone(),
        )
    }
}

/// The `AddURIToQueue` URI for a Spotify album or playlist on this
/// household: `x-rincon-cpcontainer:` with the container's item id and the
/// household's account. Pair it with [`spotify_container_didl`].
#[must_use]
pub fn spotify_container_uri(spotify_uri: &str, prefix: &str, p: &SpotifyRenderParams) -> String {
    let encoded = spotify_uri.replace(':', "%3a");
    format!(
        "x-rincon-cpcontainer:{prefix}{encoded}?sid={sid}&flags={CONTAINER_FLAGS}&sn={sn}",
        sid = p.sid,
        sn = p.sn
    )
}

/// The DIDL-Lite metadata a Spotify album or playlist needs on this
/// household; like a track's, its `desc` must name the household's own
/// Spotify account. `title` is display-only.
#[must_use]
pub fn spotify_container_didl(
    spotify_uri: &str,
    title: &str,
    kind: SpotifyContainer,
    prefix: &str,
    p: &SpotifyRenderParams,
) -> String {
    let encoded = spotify_uri.replace(':', "%3a");
    format!(
        "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
         xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\" \
         xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\
         <item id=\"{prefix}{encoded}\" parentID=\"-1\" restricted=\"true\">\
         <dc:title>{title}</dc:title>\
         <upnp:class>{class}</upnp:class>\
         <desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{cdudn}</desc>\
         </item></DIDL-Lite>",
        prefix = xml_escape(prefix),
        encoded = xml_escape(&encoded),
        title = xml_escape(title),
        class = kind.class(),
        cdudn = xml_escape(&p.cdudn),
    )
}

/// Recover the service-facing `spotify:…` URI from a renderer URI such as
/// `x-sonos-spotify:spotify%3atrack%3a<id>?sid=…` or
/// `x-rincon-cpcontainer:1004206cspotify%3aalbum%3a<id>?sid=…` (the inverse
/// of [`spotify_track_uri`]), or the unescaped
/// `x-sonos-spotify:spotify:track:<id>?sid=…` a player reports for a Spotify
/// Connect track. `None` for non-Spotify URIs, and for the Connect session
/// (`x-sonos-vli:<player>:2,spotify:<session>`), which names no item.
#[must_use]
pub fn spotify_uri_from_renderer_uri(uri: &str) -> Option<String> {
    let (_, rest) = uri.split_once(':')?;
    let path = rest.split('?').next()?;
    if let Some(start) = path.to_ascii_lowercase().find("spotify%3a") {
        let decoded = percent_decode(&path[start..])?;
        return decoded.starts_with("spotify:").then_some(decoded);
    }
    let item = &path[path.find("spotify:")?..];
    let kind = item["spotify:".len()..].split(':').next()?;
    SPOTIFY_KINDS.contains(&kind).then(|| item.to_string())
}

/// The kinds of `spotify:<kind>:…` URI that name something to play.
const SPOTIFY_KINDS: [&str; 7] = [
    "track", "album", "artist", "playlist", "show", "episode", "user",
];

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
    fn metadata_equivalence_accepts_xml_normalization_without_losing_extensions() {
        let original = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
            xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:r="urn:vendor">
            <item id="track" parentID="-1" restricted="true">
                <dc:title>A &amp; B</dc:title>
                <desc id="cdudn" nameSpace="urn:account">account-1</desc>
                <r:extra r:token="one &amp; two" enabled="true">preserved</r:extra>
            </item></DIDL-Lite>"#;
        let normalized = r"<d:DIDL-Lite xmlns:d='urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/' xmlns:title='http://purl.org/dc/elements/1.1/' xmlns:vendor='urn:vendor'><d:item restricted='true' parentID='-1' id='track'><title:title>A <!--formatting--><![CDATA[& B]]></title:title><d:desc nameSpace='urn:account' id='cdudn'>account-1</d:desc><vendor:extra enabled='true' vendor:token='one &#38; two'>preserved</vendor:extra></d:item></d:DIDL-Lite>";
        assert!(equivalent_metadata(original, normalized));
        assert!(equivalent_metadata(normalized, original));
        for changed in [
            normalized.replace("account-1", "account-2"),
            normalized.replace("urn:account", "urn:other-account"),
            normalized.replace("urn:vendor", "urn:other-vendor"),
            normalized.replace("one &#38; two", "different token"),
            normalized.replace("preserved", "different extension"),
            normalized.replace("enabled='true'", ""),
        ] {
            assert!(!equivalent_metadata(original, &changed));
        }
    }

    #[test]
    fn metadata_equivalence_requires_complete_content_and_preserves_significant_space() {
        let original = "<DIDL-Lite><item><title> A </title><res>track</res></item></DIDL-Lite>";
        for missing in [
            "",
            "NOT_IMPLEMENTED",
            "<DIDL-Lite",
            "<DIDL-Lite/>",
            "<other/>",
        ] {
            assert!(!equivalent_metadata(original, missing));
        }
        assert!(!equivalent_metadata(
            original,
            &original.replace(" A ", "A")
        ));
        assert!(!equivalent_metadata(
            "<DIDL-Lite><extra>before <value/> after</extra></DIDL-Lite>",
            "<DIDL-Lite><extra>before<value/> after</extra></DIDL-Lite>"
        ));
        assert!(!equivalent_metadata(
            "<DIDL-Lite xml:space='preserve'><item/> <item/></DIDL-Lite>",
            "<DIDL-Lite xml:space='preserve'><item/><item/></DIDL-Lite>"
        ));
        assert!(!equivalent_metadata(
            original,
            "<DIDL-Lite><item><res>track</res><title> A </title></item></DIDL-Lite>"
        ));
    }

    #[test]
    fn escapes_xml() {
        assert_eq!(xml_escape("a&b<c>"), "a&amp;b&lt;c&gt;");
    }

    #[test]
    fn favorite_metadata_moves_art_out_without_rewriting_native_extensions() {
        let metadata = "<DIDL-Lite xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:r=\"urn:example\"><item id=\"track\"><upnp:albumArtURI>https://example.invalid/art?a=1&amp;b=2</upnp:albumArtURI><r:service native=\"keep\">unchanged &amp; literal</r:service><upnp:albumArtURI><upnp:albumArtURI>nested</upnp:albumArtURI></upnp:albumArtURI></item></DIDL-Lite>";
        let stripped = favorite_metadata(metadata).unwrap();
        assert!(!stripped.contains("albumArtURI"));
        assert!(
            stripped.contains("<r:service native=\"keep\">unchanged &amp; literal</r:service>")
        );
        assert_eq!(favorite_metadata(&stripped).unwrap(), stripped);
        assert!(favorite_metadata("not XML").is_err());
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

    /// Seen live: under Spotify Connect a player reports each track with
    /// its colons unescaped, and between tracks the Connect session, which
    /// is no track.
    #[test]
    fn a_spotify_connect_track_is_its_spotify_uri() {
        assert_eq!(
            spotify_uri_from_renderer_uri(
                "x-sonos-spotify:spotify:track:abc123?sid=12&flags=0&sn=1"
            )
            .as_deref(),
            Some("spotify:track:abc123")
        );
        assert_eq!(
            spotify_uri_from_renderer_uri("x-sonos-spotify:spotify:user:someone:playlist:XyZ")
                .as_deref(),
            Some("spotify:user:someone:playlist:XyZ")
        );
        assert_eq!(
            spotify_uri_from_renderer_uri("x-sonos-vli:RINCON_TEST01400:2,spotify:0123abcd"),
            None
        );
        assert_eq!(
            spotify_uri_from_renderer_uri("x-rincon-mp3radio://example.invalid/spotify:x"),
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
    fn spotify_containers_render_with_the_households_account_and_prefixes() {
        use SpotifyContainer::{Album, Playlist};
        assert_eq!(SpotifyContainer::of("spotify:album:abc"), Some(Album));
        assert_eq!(SpotifyContainer::of("spotify:playlist:abc"), Some(Playlist));
        for other in ["spotify:track:abc", "spotify:artist:abc", "album:abc", ""] {
            assert_eq!(SpotifyContainer::of(other), None, "{other}");
        }

        // S1 has an album favorite (1004206c) and no playlist one.
        let s1 = favorites(FAV_S1);
        assert_eq!(Album.prefix_in(&s1), "1004206c");
        assert_eq!(Playlist.prefix_in(&s1), "1006206c", "the common default");
        assert_eq!(Album.prefix_in(&[]), "1004206c");
        // S2's playlists use two prefixes; the one more of them use wins.
        let mut s2 = favorites(FAV_S2);
        let odd = s2
            .iter()
            .find(|f| f.res_md.as_deref().is_some_and(|m| m.contains("10062a6c")))
            .cloned()
            .expect("a 10062a6c playlist favorite");
        assert!(["1006206c", "10062a6c"].contains(&Playlist.prefix_in(&s2).as_str()));
        s2.push(odd);
        assert_eq!(Playlist.prefix_in(&s2), "10062a6c");

        let p = SpotifyRenderParams {
            sid: 12,
            flags: 8224,
            sn: 3,
            cdudn: "SA_RINCON3079_X_#Svc3079-0-Token".into(),
            item_id_prefix: "10032020".into(),
        };
        assert_eq!(
            spotify_container_uri("spotify:album:abc123", "1004206c", &p),
            "x-rincon-cpcontainer:1004206cspotify%3aalbum%3aabc123?sid=12&flags=8300&sn=3"
        );
        assert_eq!(
            spotify_container_didl(
                "spotify:playlist:xyz",
                "Mix & Match",
                Playlist,
                "10062a6c",
                &p
            ),
            "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
             xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\" \
             xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">\
             <item id=\"10062a6cspotify%3aplaylist%3axyz\" parentID=\"-1\" restricted=\"true\">\
             <dc:title>Mix &amp; Match</dc:title>\
             <upnp:class>object.container.playlistContainer</upnp:class>\
             <desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">\
             SA_RINCON3079_X_#Svc3079-0-Token</desc></item></DIDL-Lite>"
        );
        // The renderer URI maps back to the Spotify URI it plays.
        assert_eq!(
            spotify_uri_from_renderer_uri(&spotify_container_uri(
                "spotify:album:abc123",
                "1004206c",
                &p
            ))
            .as_deref(),
            Some("spotify:album:abc123")
        );

        // Bare kinds render on the account id alone: no flags/sn/descriptor.
        assert_eq!(
            SpotifyBare::of("spotify:artist:A"),
            Some(SpotifyBare::Artist)
        );
        assert_eq!(
            SpotifyBare::of("spotify:episode:E"),
            Some(SpotifyBare::Episode)
        );
        assert_eq!(SpotifyBare::of("spotify:show:S"), Some(SpotifyBare::Show));
        assert_eq!(SpotifyBare::of("spotify:track:T"), None);
        assert_eq!(SpotifyBare::of("spotify:album:A"), None);
        assert_eq!(
            spotify_bare_uri("spotify:artist:ABC", 12),
            "x-sonos-spotify:spotify%3aartist%3aABC?sid=12"
        );
        let episode = spotify_bare_didl("spotify:episode:abc", "Ep & 1", 7);
        assert!(episode.contains("?sid=7"));
        assert!(episode.contains("<upnp:class>object.item.audioItem.musicTrack</upnp:class>"));
        assert!(episode.contains("<dc:title>Ep &amp; 1</dc:title>"));
        assert!(!episode.contains("cdudn"));
        assert!(
            spotify_bare_didl("spotify:artist:a", "A", 7)
                .contains("object.container.person.musicArtist")
        );
        assert!(
            spotify_bare_didl("spotify:show:s", "S", 7)
                .contains("<upnp:class>object.container</upnp:class>")
        );
    }

    #[test]
    fn bare_spotify_rendering_preserves_case_sensitive_catalog_ids() {
        for kind in ["artist", "episode", "show"] {
            let source = format!("spotify:{kind}:AbCdEf0123456789GhIjKl");
            let uri = spotify_bare_uri(&source, 7);
            assert_eq!(spotify_uri_from_renderer_uri(&uri), Some(source.clone()));
            let item = parse_didl(&spotify_bare_didl(&source, "Mixed case", 7))
                .unwrap()
                .remove(0);
            assert_eq!(item.res.unwrap().uri, uri);
            assert!(item.desc.is_none());
            assert!(!uri.contains("flags=") && !uri.contains("sn="));
        }
    }

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

    fn template_favorite(params: &SpotifyRenderParams, index: usize) -> DidlObject {
        let source = format!("spotify:track:ExampleTrack{index}");
        let document = format!(
            "<DIDL-Lite xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">\
             <item id=\"FV:2/{index}\"><res>{}</res><r:resMD>{}</r:resMD></item></DIDL-Lite>",
            xml_escape(&spotify_track_uri(&source, params)),
            xml_escape(&spotify_track_didl(&source, "Example", params)),
        );
        parse_didl(&document).unwrap().remove(0)
    }

    #[test]
    fn ranked_track_templates_are_distinct_complete_observed_sets() {
        let first = SpotifyRenderParams {
            sid: 12,
            flags: 8224,
            sn: 1,
            cdudn: "SA_RINCON_SIM_FIRST".into(),
            item_id_prefix: "10032020".into(),
        };
        let second = SpotifyRenderParams {
            sn: 2,
            cdudn: "SA_RINCON_SIM_SECOND".into(),
            ..first.clone()
        };
        let third = SpotifyRenderParams {
            flags: 8232,
            item_id_prefix: "10032028".into(),
            ..second.clone()
        };
        let favorites: Vec<_> = [&first, &second, &third, &first, &first]
            .into_iter()
            .enumerate()
            .map(|(index, params)| template_favorite(params, index))
            .collect();
        assert_eq!(
            spotify_track_templates(&favorites),
            [first.clone(), second, third]
        );
        assert_eq!(learn_spotify_params(&favorites), Some(first));
    }

    #[test]
    fn ranked_track_template_ties_keep_the_original_favorite_order() {
        let first = SpotifyRenderParams {
            sid: 12,
            flags: 8224,
            sn: 1,
            cdudn: "SA_RINCON_SIM_FIRST".into(),
            item_id_prefix: "10032020".into(),
        };
        let second = SpotifyRenderParams {
            sn: 2,
            cdudn: "SA_RINCON_SIM_SECOND".into(),
            ..first.clone()
        };
        for ordered in [[&first, &second], [&second, &first]] {
            let favorites: Vec<_> = ordered
                .iter()
                .enumerate()
                .map(|(index, params)| template_favorite(params, index))
                .collect();
            assert_eq!(
                spotify_track_templates(&favorites),
                ordered.map(Clone::clone)
            );
        }
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
