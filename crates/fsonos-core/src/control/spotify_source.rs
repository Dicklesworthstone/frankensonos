//! Render containers from one coherent favorite, not a track account combined
//! with an unrelated album or playlist prefix. Track learning stays separate:
//! a container's flags and prefix must never be used to render a DJ movement.

use fsonos_proto::didl::{
    DidlObject, SpotifyContainer, SpotifyRenderParams, learn_spotify_params, parse_didl,
    spotify_container_didl, spotify_container_uri, spotify_uri_from_renderer_uri,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Template {
    params: SpotifyRenderParams,
    query: String,
}

impl Template {
    fn from_favorite(favorite: &DidlObject, kind: SpotifyContainer) -> Option<Self> {
        let resource = &favorite.res.as_ref()?.uri;
        let (id, query) = resource
            .strip_prefix("x-rincon-cpcontainer:")?
            .split_once('?')?;
        let objects = parse_didl(favorite.res_md.as_deref()?).ok()?;
        let [item] = objects.as_slice() else {
            return None;
        };
        let class = match kind {
            SpotifyContainer::Album => "object.container.album.musicAlbum",
            SpotifyContainer::Playlist => "object.container.playlistContainer",
        };
        // Metadata and resource must describe the SAME item. Percent-escape
        // case is insignificant, but Spotify identifiers are case-sensitive.
        if item.class != class || id.replace("%3A", "%3a") != item.id.replace("%3A", "%3a") {
            return None;
        }
        let source = spotify_uri_from_renderer_uri(resource)?;
        if container_kind(&source) != Some(kind) {
            return None;
        }
        let at = id.to_ascii_lowercase().find("spotify%3a")?;
        let prefix = &id[..at];
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let desc = item.desc.as_ref()?;
        if desc.id != "cdudn" || desc.value.trim().is_empty() {
            return None;
        }
        // Keep the observed query, including service-specific arguments. Do
        // not silently choose one of two conflicting account parameters.
        if !query.bytes().all(|b| b.is_ascii_graphic() && b != b'#') {
            return None;
        }
        Some(Self {
            params: SpotifyRenderParams {
                sid: argument(query, "sid")?,
                flags: argument(query, "flags")?,
                sn: argument(query, "sn")?,
                cdudn: desc.value.clone(),
                item_id_prefix: prefix.to_string(),
            },
            query: query.to_string(),
        })
    }

    fn render(&self, kind: SpotifyContainer, source: &str, title: &str) -> (String, String) {
        let prefix = &self.params.item_id_prefix;
        (
            format!(
                "x-rincon-cpcontainer:{prefix}{}?{}",
                source.replace(':', "%3a"),
                self.query
            ),
            spotify_container_didl(source, title, kind, prefix, &self.params),
        )
    }
}

fn argument(query: &str, name: &str) -> Option<u32> {
    let mut values = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value);
    let value = values.next()?;
    if values.next().is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn container_kind(source: &str) -> Option<SpotifyContainer> {
    let parts: Vec<_> = source.split(':').collect();
    let (kind, id) = match parts.as_slice() {
        ["spotify", "album", id] => (SpotifyContainer::Album, *id),
        ["spotify", "playlist", id] => (SpotifyContainer::Playlist, *id),
        ["spotify", "user", owner, "playlist", id] if !owner.is_empty() => {
            (SpotifyContainer::Playlist, *id)
        }
        _ => return None,
    };
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(kind)
}

/// Prefer a complete favorite of the requested kind. Vote on entire observed
/// templates, with the first favorite breaking ties: never synthesize an
/// account/flags/prefix combination no favorite uses. With no usable container
/// favorite, retain the established track-account/default-prefix fallback.
pub(super) fn container(
    favorites: &[DidlObject],
    kind: SpotifyContainer,
    source: &str,
    title: &str,
) -> Option<(String, String)> {
    choose(favorites, kind, source, title, None)
}

/// A definite 800 permits one fresh learning pass. Prefer another complete
/// observed template (or the established track fallback) over the rejected
/// one. With no alternative, retain the existing one-retry behavior: an 800
/// can also be transient. Ambiguous transport failures never call this path.
pub(super) fn after_rejection(
    favorites: &[DidlObject],
    kind: SpotifyContainer,
    source: &str,
    title: &str,
    rejected: &(String, String),
) -> Option<(String, String)> {
    choose(favorites, kind, source, title, Some(rejected))
        .or_else(|| container(favorites, kind, source, title))
}

fn same_source(candidate: &(String, String), rejected: &(String, String)) -> bool {
    if candidate.1 != rejected.1 {
        return false;
    }
    let Some((path, query)) = candidate.0.split_once('?') else {
        return candidate == rejected;
    };
    let Some((old_path, old_query)) = rejected.0.split_once('?') else {
        return false;
    };
    let mut pairs: Vec<_> = query.split('&').collect();
    let mut old_pairs: Vec<_> = old_query.split('&').collect();
    pairs.sort_unstable();
    old_pairs.sort_unstable();
    path == old_path && pairs == old_pairs
}

fn choose(
    favorites: &[DidlObject],
    kind: SpotifyContainer,
    source: &str,
    title: &str,
    rejected: Option<&(String, String)>,
) -> Option<(String, String)> {
    let templates: Vec<_> = favorites
        .iter()
        .filter_map(|favorite| Template::from_favorite(favorite, kind))
        .collect();
    let mut best = None;
    let mut most = 0;
    for template in &templates {
        if rejected.is_some_and(|old| same_source(&template.render(kind, source, title), old)) {
            continue;
        }
        let count = templates.iter().filter(|other| *other == template).count();
        if count > most {
            best = Some(template);
            most = count;
        }
    }
    if let Some(template) = best {
        return Some(template.render(kind, source, title));
    }
    let params = learn_spotify_params(favorites)?;
    // Without a complete container template, do not borrow a prefix from a
    // malformed or differently linked favorite. These are the existing
    // protocol defaults, not values inferred from another household.
    let prefix = kind.prefix_in(&[]);
    let fallback = (
        spotify_container_uri(source, &prefix, &params),
        spotify_container_didl(source, title, kind, &prefix, &params),
    );
    if rejected.is_some_and(|old| same_source(&fallback, old)) {
        None
    } else {
        Some(fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::{spotify_track_didl, spotify_track_uri, xml_escape};

    fn params(sn: u32) -> SpotifyRenderParams {
        SpotifyRenderParams {
            sid: 12,
            flags: 8224,
            sn,
            cdudn: format!("SA_RINCON_SIM_ACCOUNT_{sn}"),
            item_id_prefix: "10032020".into(),
        }
    }

    fn favorite(uri: &str, metadata: &str) -> DidlObject {
        let xml = format!(
            "<DIDL-Lite xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">\
             <item id=\"FV:1\"><res>{}</res><r:resMD>{}</r:resMD></item></DIDL-Lite>",
            xml_escape(uri),
            xml_escape(metadata),
        );
        parse_didl(&xml).unwrap().remove(0)
    }

    fn album(p: &SpotifyRenderParams, prefix: &str, query: &str) -> DidlObject {
        let source = "spotify:album:0SimAlbumA";
        favorite(
            &format!(
                "x-rincon-cpcontainer:{prefix}{}?{query}",
                source.replace(':', "%3a")
            ),
            &spotify_container_didl(source, "Album", SpotifyContainer::Album, prefix, p),
        )
    }

    #[test]
    fn an_album_favorite_is_sufficient_and_preserves_its_whole_template() {
        let p = params(7);
        let fav = album(&p, "abcd206c", "sn=7&sid=12&flags=9999&extra=kept");
        assert!(learn_spotify_params(std::slice::from_ref(&fav)).is_none());
        let (uri, metadata) = container(
            &[fav],
            SpotifyContainer::Album,
            "spotify:album:0SimDifferentAlbum",
            "Rock & Roll <Live>",
        )
        .unwrap();
        assert_eq!(
            uri,
            "x-rincon-cpcontainer:abcd206cspotify%3aalbum%3a0SimDifferentAlbum?sn=7&sid=12&flags=9999&extra=kept"
        );
        let item = parse_didl(&metadata).unwrap().remove(0);
        assert_eq!(item.id, "abcd206cspotify%3aalbum%3a0SimDifferentAlbum");
        assert_eq!(item.title, "Rock & Roll <Live>");
        assert_eq!(item.desc.unwrap().value, p.cdudn);
    }

    #[test]
    fn playlists_include_legacy_user_scoped_favorites() {
        for original in [
            "spotify:playlist:0SimList",
            "spotify:user:simuser:playlist:0SimList",
        ] {
            let p = params(2);
            let metadata = spotify_container_didl(
                original,
                "List",
                SpotifyContainer::Playlist,
                "10062a6c",
                &p,
            );
            let uri = format!(
                "x-rincon-cpcontainer:10062a6c{}?sid=12&flags=8300&sn=2",
                original.replace(':', "%3A")
            );
            let fav = favorite(&uri, &metadata);
            let (rendered, metadata) = container(
                std::slice::from_ref(&fav),
                SpotifyContainer::Playlist,
                "spotify:playlist:0SimNewList",
                "New",
            )
            .unwrap();
            assert!(
                rendered
                    .starts_with("x-rincon-cpcontainer:10062a6cspotify%3aplaylist%3a0SimNewList?")
            );
            assert_eq!(
                parse_didl(&metadata).unwrap()[0]
                    .desc
                    .as_ref()
                    .unwrap()
                    .value,
                p.cdudn
            );
            assert!(
                container(
                    &[fav],
                    SpotifyContainer::Album,
                    "spotify:album:0SimAlbum",
                    "Album"
                )
                .is_none()
            );
        }
    }

    #[test]
    fn a_native_container_account_wins_over_an_unrelated_track_account() {
        let track_p = params(1);
        let track = "spotify:track:0SimTrack";
        let track_fav = favorite(
            &spotify_track_uri(track, &track_p),
            &spotify_track_didl(track, "Track", &track_p),
        );
        let album_p = params(9);
        let fav = album(&album_p, "abcd206c", "sid=12&flags=8300&sn=9");
        let (uri, metadata) = container(
            &[track_fav, fav],
            SpotifyContainer::Album,
            "spotify:album:0SimNew",
            "New",
        )
        .unwrap();
        assert!(uri.ends_with("&sn=9"));
        assert_eq!(
            parse_didl(&metadata).unwrap()[0]
                .desc
                .as_ref()
                .unwrap()
                .value,
            album_p.cdudn
        );
    }

    #[test]
    fn votes_keep_accounts_prefixes_and_flags_together_and_ties_are_stable() {
        let a = album(&params(1), "aaaa206c", "sid=12&flags=8300&sn=1");
        let b = album(&params(2), "bbbb206c", "sid=12&flags=8301&sn=2");
        let render = |favorites: &[DidlObject]| {
            container(
                favorites,
                SpotifyContainer::Album,
                "spotify:album:0SimNew",
                "New",
            )
            .unwrap()
        };
        let (uri, metadata) = render(&[a.clone(), b.clone(), b.clone()]);
        assert_eq!(
            uri,
            "x-rincon-cpcontainer:bbbb206cspotify%3aalbum%3a0SimNew?sid=12&flags=8301&sn=2"
        );
        assert_eq!(
            parse_didl(&metadata).unwrap()[0]
                .desc
                .as_ref()
                .unwrap()
                .value,
            params(2).cdudn
        );
        assert!(render(&[a, b]).0.contains("aaaa206cspotify"));
    }

    #[test]
    fn malformed_or_ambiguous_queries_do_not_supply_an_account() {
        for query in [
            "sid=12&flags=8300",
            "sid=12&flags=8300&sn=1&sn=2",
            "sid=12&sid=13&flags=8300&sn=1",
            "sid=12&flags=8300&flags=8301&sn=1",
            "sid=12&flags=8300&sn=+1",
            "sid=12&flags=8300&sn=4294967296",
            "sid=12&flags=8300&sn=1#fragment",
        ] {
            let fav = album(&params(1), "1004206c", query);
            assert!(
                container(
                    &[fav],
                    SpotifyContainer::Album,
                    "spotify:album:0SimNew",
                    "New"
                )
                .is_none(),
                "{query}"
            );
        }
    }

    #[test]
    fn mismatched_metadata_cannot_contribute_a_prefix_or_descriptor() {
        let good = album(&params(1), "1004206c", "sid=12&flags=8300&sn=1");
        for (from, to) in [
            ("0SimAlbumA", "0SimAlbumB"),
            ("musicAlbum", "playlistContainer"),
            ("cdudn", "other"),
            ("SA_RINCON_SIM_ACCOUNT_1", " "),
        ] {
            let mut bad = good.clone();
            bad.res_md = bad.res_md.map(|md| md.replace(from, to));
            assert!(
                container(
                    &[bad],
                    SpotifyContainer::Album,
                    "spotify:album:0SimNew",
                    "New"
                )
                .is_none(),
                "{from}"
            );
        }
        let mut bad = good;
        let resource = bad.res.as_mut().unwrap();
        resource.uri = resource.uri.replace("x-rincon-cpcontainer:", "http:");
        assert!(
            container(
                &[bad],
                SpotifyContainer::Album,
                "spotify:album:0SimNew",
                "New"
            )
            .is_none()
        );
    }

    #[test]
    fn track_only_households_keep_the_existing_fallback() {
        let p = params(1);
        let source = "spotify:track:0SimTrack";
        let fav = favorite(
            &spotify_track_uri(source, &p),
            &spotify_track_didl(source, "Track", &p),
        );
        for (kind, source) in [
            (SpotifyContainer::Album, "spotify:album:0SimNew"),
            (SpotifyContainer::Playlist, "spotify:playlist:0SimNew"),
        ] {
            let (uri, metadata) =
                container(std::slice::from_ref(&fav), kind, source, "New").unwrap();
            let prefix = kind.prefix_in(&[]);
            assert_eq!(uri, spotify_container_uri(source, &prefix, &p));
            assert_eq!(
                metadata,
                spotify_container_didl(source, "New", kind, &prefix, &p)
            );
        }
    }

    #[test]
    fn retry_skips_a_rejected_tuple_even_when_stale_favorites_outvote_it() {
        let stale = album(&params(1), "aaaa206c", "sid=12&flags=8300&sn=1");
        let reordered = album(&params(1), "aaaa206c", "sn=1&flags=8300&sid=12");
        let fresh = album(&params(2), "bbbb206c", "sid=12&flags=8300&sn=2");
        let favorites = [stale.clone(), stale, reordered, fresh];
        let source = "spotify:album:0SimNew";
        let kind = SpotifyContainer::Album;
        let initial = container(&favorites, kind, source, "New").unwrap();
        assert!(initial.0.ends_with("&sn=1"));
        let retry = after_rejection(&favorites, kind, source, "New", &initial).unwrap();
        assert!(retry.0.starts_with("x-rincon-cpcontainer:bbbb206c"));
        assert!(retry.0.ends_with("&sn=2"));
        assert_eq!(
            parse_didl(&retry.1).unwrap()[0]
                .desc
                .as_ref()
                .unwrap()
                .value,
            params(2).cdudn
        );
    }

    #[test]
    fn a_rejected_native_template_can_fall_back_to_the_observed_track_account() {
        let p = params(1);
        let track = "spotify:track:0SimTrack";
        let track_fav = favorite(
            &spotify_track_uri(track, &p),
            &spotify_track_didl(track, "Track", &p),
        );
        let stale = album(&params(9), "abcd206c", "sid=12&flags=8300&sn=9");
        let favorites = [track_fav, stale];
        let kind = SpotifyContainer::Album;
        let source = "spotify:album:0SimNew";
        let initial = container(&favorites, kind, source, "New").unwrap();
        assert!(initial.0.ends_with("&sn=9"));
        let retry = after_rejection(&favorites, kind, source, "New", &initial).unwrap();
        let prefix = kind.prefix_in(&[]);
        assert_eq!(retry.0, spotify_container_uri(source, &prefix, &p));
        assert_eq!(
            retry.1,
            spotify_container_didl(source, "New", kind, &prefix, &p)
        );
    }

    #[test]
    fn retry_without_an_alternative_preserves_the_existing_transient_fault_behavior() {
        let favorites = [album(&params(1), "1004206c", "sid=12&flags=8300&sn=1")];
        let source = "spotify:album:0SimNew";
        let kind = SpotifyContainer::Album;
        let initial = container(&favorites, kind, source, "New").unwrap();
        assert_eq!(
            after_rejection(&favorites, kind, source, "New", &initial),
            Some(initial.clone())
        );
        // An account removed between attempts must not be recovered from a
        // cached rejected source: only the fresh favorites are authoritative.
        assert!(after_rejection(&[], kind, source, "New", &initial).is_none());
    }
}
