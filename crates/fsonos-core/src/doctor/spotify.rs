//! Spotify-on-Sonos checks, one set per household. S1 and S2 are separate
//! households with separate Spotify linkage, so each is checked on its own.
//!
//! * `spotify.favorite.<s1|s2>`: the household has a Spotify track in My
//!   Sonos. Render parameters can only be learned from one.
//! * `spotify.linked.<s1|s2>`: a Spotify favorite names the household's
//!   Spotify service account (`SA_RINCON…` descriptor). Only favorites prove
//!   linkage: MusicServices lists Spotify whether or not it is linked.
//! * `spotify.render_params.<s1|s2>`: parameters learned from the favorites
//!   rebuild an existing favorite's URI and metadata exactly, so a Spotify
//!   track FrankenSonos sends will render (a wrong descriptor fails with
//!   UPnP 800).
//!
//! Read-only: one `Browse FV:2` per household, shared by its three checks.
//! The service-account descriptor is only ever shown masked
//! ([`mask_account`]).

use super::{Check, CheckContext, CheckId, CheckResult, Runner};
use crate::HouseholdState;
use fsonos_proto::Transport;
use fsonos_proto::content;
use fsonos_proto::didl::{
    DidlObject, SpotifyRenderParams, learn_spotify_params, parse_didl, spotify_track_didl,
    spotify_track_uri, spotify_uri_from_renderer_uri,
};
use fsonos_types::Generation;
use serde_json::json;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

/// What a household's favorites say about Spotify on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyFindings {
    /// Spotify track favorites (`x-sonos-spotify:`): render parameters are
    /// learned from these.
    pub tracks: usize,
    /// Other Spotify favorites (albums, playlists, radio).
    pub others: usize,
    /// Distinct service-account descriptors the Spotify favorites name, most
    /// common first.
    pub accounts: Vec<String>,
    /// The parameters learned from the track favorites.
    pub params: Option<SpotifyRenderParams>,
    /// Track favorites whose URI, item id and descriptor the learned
    /// parameters rebuild exactly (out of `tracks`).
    pub rebuilt: usize,
}

/// Read a household's `FV:2` for Spotify: count its Spotify favorites, collect
/// the service accounts they name, learn the render parameters, and rebuild
/// every track favorite from them.
#[must_use]
pub fn assess(favorites: &[DidlObject]) -> SpotifyFindings {
    let mut findings = SpotifyFindings {
        tracks: 0,
        others: 0,
        accounts: Vec::new(),
        params: learn_spotify_params(favorites),
        rebuilt: 0,
    };
    let mut accounts: Vec<(String, usize)> = Vec::new();
    for fav in favorites {
        let Some(uri) = fav.res.as_ref().map(|r| r.uri.as_str()) else {
            continue;
        };
        if spotify_uri_from_renderer_uri(uri).is_none() {
            continue;
        }
        let track = uri.starts_with("x-sonos-spotify:");
        if track {
            findings.tracks += 1;
        } else {
            findings.others += 1;
        }
        let item = fav.res_md_object().ok().flatten();
        if let Some(desc) = item
            .as_ref()
            .and_then(|i| i.desc.as_ref())
            .filter(|d| d.id == "cdudn" && d.value.starts_with("SA_RINCON"))
        {
            match accounts.iter_mut().find(|(a, _)| *a == desc.value) {
                Some((_, n)) => *n += 1,
                None => accounts.push((desc.value.clone(), 1)),
            }
        }
        if track
            && let (Some(p), Some(item)) = (&findings.params, &item)
            && rebuilds(uri, item, p)
        {
            findings.rebuilt += 1;
        }
    }
    accounts.sort_by_key(|a| std::cmp::Reverse(a.1));
    findings.accounts = accounts.into_iter().map(|(a, _)| a).collect();
    findings
}

/// Whether the URI and DIDL FrankenSonos would send for this favorite's
/// track, built from `p`, match the favorite's own URI, item id and
/// descriptor.
fn rebuilds(uri: &str, item: &DidlObject, p: &SpotifyRenderParams) -> bool {
    let Some(spotify) = spotify_uri_from_renderer_uri(uri) else {
        return false;
    };
    if !spotify_track_uri(&spotify, p).eq_ignore_ascii_case(uri) {
        return false;
    }
    let Ok(built) = parse_didl(&spotify_track_didl(&spotify, &item.title, p)) else {
        return false;
    };
    built.first().is_some_and(|b| {
        b.id.eq_ignore_ascii_case(&item.id)
            && b.desc.as_ref().map(|d| &d.value) == item.desc.as_ref().map(|d| &d.value)
    })
}

/// `SA_RINCON3079_X_#Svc3079-0-Token` becomes `SA_RINCON3079_X_#Svc3079-…`:
/// enough to see which service, never the account part.
#[must_use]
pub fn mask_account(descriptor: &str) -> String {
    let keep = descriptor
        .find("#Svc")
        .and_then(|i| descriptor[i..].find('-').map(|j| i + j + 1))
        .unwrap_or_else(|| descriptor.len().min(13));
    format!("{}…", descriptor.get(..keep).unwrap_or(""))
}

/// The Sonos app that manages households of `g`.
fn app(g: Generation) -> &'static str {
    match g {
        Generation::S1 => "Sonos S1 Controller app",
        Generation::S2 => "Sonos app",
    }
}

fn add_a_track(g: Generation) -> String {
    format!(
        "In the {}, add any Spotify track to My Sonos (Favorites), then run `fsonos doctor` again.",
        app(g)
    )
}

/// `spotify.favorite.<g>`.
#[must_use]
pub fn favorite_result(f: &SpotifyFindings, g: Generation) -> CheckResult {
    let evidence = json!({ "spotify_tracks": f.tracks, "spotify_others": f.others });
    if f.tracks > 0 {
        CheckResult::pass(format!(
            "{} Spotify track favorite(s), {} other Spotify favorite(s)",
            f.tracks, f.others
        ))
        .with_evidence(evidence)
    } else if f.others > 0 {
        CheckResult::warn(
            format!(
                "{} Spotify album/playlist favorite(s) but no Spotify track; render parameters are learned from a track",
                f.others
            ),
            add_a_track(g),
        )
        .with_evidence(evidence)
    } else {
        CheckResult::fail("no Spotify favorites in this household", add_a_track(g))
            .with_evidence(evidence)
    }
}

/// `spotify.linked.<g>`.
#[must_use]
pub fn linked_result(f: &SpotifyFindings, g: Generation) -> CheckResult {
    let masked: Vec<String> = f.accounts.iter().map(|a| mask_account(a)).collect();
    let evidence = json!({ "accounts": masked });
    match masked.as_slice() {
        [one] => CheckResult::pass(format!("Spotify is linked: favorites name {one}"))
            .with_evidence(evidence),
        [first, rest @ ..] => CheckResult::pass(format!(
            "Spotify is linked: favorites name {} service accounts",
            rest.len() + 1
        ))
        .with_detail(format!(
            "FrankenSonos uses the most common one ({first}); favorites saved from another account may not play."
        ))
        .with_evidence(evidence),
        [] if f.tracks + f.others > 0 => CheckResult::warn(
            "Spotify favorites name no service account",
            format!(
                "In the {}, check that Spotify is still linked (add it as a music service if not), then add a Spotify track to My Sonos again.",
                app(g)
            ),
        )
        .with_evidence(evidence),
        [] => CheckResult::warn(
            "linkage unknown: no Spotify favorite to prove it",
            format!(
                "In the {}, link your Spotify account (add Spotify as a music service), then add any Spotify track to My Sonos.",
                app(g)
            ),
        )
        .with_evidence(evidence),
    }
}

/// `spotify.render_params.<g>`.
#[must_use]
pub fn render_params_result(f: &SpotifyFindings, g: Generation) -> CheckResult {
    let Some(p) = &f.params else {
        return CheckResult::fail(
            "no render parameters: no Spotify track favorite to learn them from",
            add_a_track(g),
        );
    };
    let evidence = json!({
        "sid": p.sid,
        "flags": p.flags,
        "sn": p.sn,
        "item_id_prefix": p.item_id_prefix,
        "account": mask_account(&p.cdudn),
        "rebuilt": f.rebuilt,
        "tracks": f.tracks,
    });
    if f.rebuilt == 0 {
        return CheckResult::fail(
            format!(
                "the learned parameters rebuild none of the {} Spotify track favorite(s)",
                f.tracks
            ),
            format!(
                "Remove and re-add a Spotify track in My Sonos in the {}: a track sent with the wrong service-account descriptor fails with UPnP 800.",
                app(g)
            ),
        )
        .with_evidence(evidence);
    }
    let result = CheckResult::pass(format!(
        "learned sid={} flags={} sn={}; they rebuild {}/{} Spotify track favorite(s) exactly",
        p.sid, p.flags, p.sn, f.rebuilt, f.tracks
    ))
    .with_evidence(evidence);
    if f.rebuilt < f.tracks {
        result.with_detail(
            "The others were saved with other flags or item-id prefixes, which the player also accepts.",
        )
    } else {
        result
    }
}

/// One household's favorites, read once and shared by its three checks.
struct Probe<T: ?Sized> {
    t: Arc<T>,
    generation: Generation,
    /// Players to browse through, in order (any visible player answers for
    /// its household; zone bridges are not in the model).
    hosts: Vec<IpAddr>,
    findings: OnceLock<Result<SpotifyFindings, String>>,
}

impl<T: Transport + ?Sized> Probe<T> {
    fn findings(&self) -> &Result<SpotifyFindings, String> {
        self.findings.get_or_init(|| {
            let mut last = format!("no {:?} household on this network", self.generation);
            for &host in &self.hosts {
                match content::browse_all(&*self.t, host, "FV:2") {
                    Ok(favorites) => return Ok(assess(&favorites)),
                    Err(e) => last = format!("Browse FV:2 via {host}: {e}"),
                }
            }
            Err(last)
        })
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Favorite,
    Linked,
    RenderParams,
}

struct SpotifyCheck<T: ?Sized> {
    kind: Kind,
    probe: Arc<Probe<T>>,
    /// Whether a household of this generation is on the network.
    present: bool,
}

/// The check ids for generation `g`: (favorite, linked, render params).
#[must_use]
pub fn ids(g: Generation) -> (CheckId, CheckId, CheckId) {
    match g {
        Generation::S1 => (
            CheckId("spotify.favorite.s1"),
            CheckId("spotify.linked.s1"),
            CheckId("spotify.render_params.s1"),
        ),
        Generation::S2 => (
            CheckId("spotify.favorite.s2"),
            CheckId("spotify.linked.s2"),
            CheckId("spotify.render_params.s2"),
        ),
    }
}

impl<T: Transport + Send + Sync + ?Sized> Check for SpotifyCheck<T> {
    fn id(&self) -> CheckId {
        let (favorite, linked, render) = ids(self.probe.generation);
        match self.kind {
            Kind::Favorite => favorite,
            Kind::Linked => linked,
            Kind::RenderParams => render,
        }
    }

    fn title(&self) -> &str {
        match (self.kind, self.probe.generation) {
            (Kind::Favorite, Generation::S1) => "Spotify favorite (S1)",
            (Kind::Favorite, Generation::S2) => "Spotify favorite (S2)",
            (Kind::Linked, Generation::S1) => "Spotify linked (S1)",
            (Kind::Linked, Generation::S2) => "Spotify linked (S2)",
            (Kind::RenderParams, Generation::S1) => "Spotify render parameters (S1)",
            (Kind::RenderParams, Generation::S2) => "Spotify render parameters (S2)",
        }
    }

    fn requires(&self) -> &[CheckId] {
        match (self.kind, self.probe.generation) {
            (Kind::RenderParams, Generation::S1) => &[CheckId("spotify.favorite.s1")],
            (Kind::RenderParams, Generation::S2) => &[CheckId("spotify.favorite.s2")],
            _ => &[],
        }
    }

    fn run(&self, _ctx: &CheckContext) -> CheckResult {
        let g = self.probe.generation;
        if !self.present {
            return CheckResult::skip(format!("no {g:?} household on this network"));
        }
        let f = match self.probe.findings() {
            Ok(f) => f,
            Err(e) => {
                return match self.kind {
                    Kind::Favorite => CheckResult::fail(
                        format!("could not read the favorites: {e}"),
                        "Check that the household's players are on and reachable, then run `fsonos doctor` again.",
                    ),
                    _ => CheckResult::skip(format!("the favorites could not be read: {e}")),
                };
            }
        };
        match self.kind {
            Kind::Favorite => favorite_result(f, g),
            Kind::Linked => linked_result(f, g),
            Kind::RenderParams => render_params_result(f, g),
        }
    }
}

/// Register the Spotify checks for S1 and S2. Each household's favorites
/// are read through `t`, once; a generation with no household in
/// `households` reports its checks as skipped.
pub fn register<T: Transport + Send + Sync + ?Sized + 'static>(
    runner: &mut Runner,
    t: &Arc<T>,
    households: &[HouseholdState],
) {
    for g in [Generation::S1, Generation::S2] {
        let household = households.iter().find(|h| h.generation() == Some(g));
        let probe = Arc::new(Probe {
            t: Arc::clone(t),
            generation: g,
            hosts: household
                .map(|h| h.players.iter().map(|p| p.ip).collect())
                .unwrap_or_default(),
            findings: OnceLock::new(),
        });
        for kind in [Kind::Favorite, Kind::Linked, Kind::RenderParams] {
            runner.register(SpotifyCheck {
                kind,
                probe: Arc::clone(&probe),
                present: household.is_some(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn favorites(body: &str) -> Vec<DidlObject> {
        content::parse_browse_response(body).unwrap().objects
    }

    const S1: &str = include_str!("../../../fsonos-proto/tests/fixtures/browse_favorites_s1.xml");
    const S2: &str = include_str!("../../../fsonos-proto/tests/fixtures/browse_favorites_s2.xml");

    #[test]
    fn both_fixture_households_pass_every_check() {
        for (body, g) in [(S1, Generation::S1), (S2, Generation::S2)] {
            let f = assess(&favorites(body));
            assert!(f.tracks > 0, "{g:?}: {f:?}");
            assert_eq!(f.accounts.len(), 1, "{g:?}: one linked account");
            assert!(f.rebuilt > 0, "{g:?}: {f:?}");
            assert!(f.rebuilt <= f.tracks);
            for result in [
                favorite_result(&f, g),
                linked_result(&f, g),
                render_params_result(&f, g),
            ] {
                assert_eq!(result.status, Status::Pass, "{g:?}: {result:?}");
            }
            let rendered = serde_json::to_string(&render_params_result(&f, g)).unwrap()
                + &serde_json::to_string(&linked_result(&f, g)).unwrap();
            assert!(
                !rendered.contains(&f.accounts[0]),
                "{g:?}: the descriptor is never shown in full"
            );
        }
    }

    #[test]
    fn a_wrong_descriptor_is_caught() {
        let mut f = assess(&favorites(S1));
        let mut p = f.params.clone().unwrap();
        p.cdudn = "SA_RINCON3079_X_#Svc3079-wrong-Token".into();
        f.params = Some(p.clone());
        f.rebuilt = favorites(S1)
            .iter()
            .filter(|fav| {
                let uri = fav.res.as_ref().map_or("", |r| r.uri.as_str());
                uri.starts_with("x-sonos-spotify:")
                    && rebuilds(uri, &fav.res_md_object().unwrap().unwrap(), &p)
            })
            .count();
        assert_eq!(f.rebuilt, 0);
        let r = render_params_result(&f, Generation::S1);
        assert_eq!(r.status, Status::Fail);
        assert!(r.remedy.unwrap().contains("UPnP 800"));
    }

    #[test]
    fn no_spotify_favorite_fails_and_linkage_is_unknown() {
        let others: Vec<DidlObject> = favorites(S2)
            .into_iter()
            .filter(|f| {
                f.res
                    .as_ref()
                    .is_none_or(|r| spotify_uri_from_renderer_uri(&r.uri).is_none())
            })
            .collect();
        let f = assess(&others);
        assert_eq!((f.tracks, f.others, f.params.is_none()), (0, 0, true));
        let fav = favorite_result(&f, Generation::S2);
        assert_eq!(fav.status, Status::Fail);
        assert!(fav.remedy.unwrap().contains("Sonos app"));
        let linked = linked_result(&f, Generation::S2);
        assert_eq!(linked.status, Status::Warn);
        let remedy = linked.remedy.unwrap();
        assert!(remedy.contains("link your Spotify account") && remedy.contains("My Sonos"));
    }

    #[test]
    fn albums_alone_warn_and_learn_nothing() {
        let containers: Vec<DidlObject> = favorites(S1)
            .into_iter()
            .filter(|f| {
                f.res.as_ref().is_some_and(|r| {
                    !r.uri.starts_with("x-sonos-spotify:")
                        && spotify_uri_from_renderer_uri(&r.uri).is_some()
                })
            })
            .collect();
        assert!(
            !containers.is_empty(),
            "the S1 fixture has Spotify containers"
        );
        let f = assess(&containers);
        assert_eq!(f.tracks, 0);
        assert_eq!(favorite_result(&f, Generation::S1).status, Status::Warn);
        assert_eq!(linked_result(&f, Generation::S1).status, Status::Pass);
        assert_eq!(
            render_params_result(&f, Generation::S1).status,
            Status::Fail
        );
    }

    #[test]
    fn descriptors_are_masked_after_the_service() {
        assert_eq!(
            mask_account("SA_RINCON3079_X_#Svc3079-0-Token"),
            "SA_RINCON3079_X_#Svc3079-…"
        );
        assert_eq!(mask_account("SA_RINCON3079_abcdef"), "SA_RINCON3079…");
        assert_eq!(mask_account("short"), "short…");
    }
}
