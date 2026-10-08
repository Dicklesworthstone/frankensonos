//! Favorites: classification over the scrubbed S1/S2 `FV:2` fixtures, name
//! lookup, and the SOAP each kind of favorite plays with.

use fsonos_core::HouseholdState;
use fsonos_core::favorites::{self, Favorite, FavoriteError, FavoriteKind};
use fsonos_proto::{ProtoError, Transport, content, soap, topology};
use fsonos_types::PlayerId;
use std::cell::RefCell;
use std::net::IpAddr;

const FAV_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/browse_favorites_s1.xml");
const FAV_S2: &str = include_str!("../../fsonos-proto/tests/fixtures/browse_favorites_s2.xml");
const ZGS_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s1.xml");

fn parsed(body: &str) -> Vec<Favorite> {
    content::parse_browse_response(body)
        .unwrap()
        .objects
        .iter()
        .map(favorites::classify)
        .collect()
}

#[test]
fn every_fixture_favorite_gets_the_kind_its_uri_implies() {
    for body in [FAV_S1, FAV_S2] {
        let objects = content::parse_browse_response(body).unwrap().objects;
        for (o, f) in objects.iter().zip(parsed(body)) {
            let uri = o.res.as_ref().map_or("", |r| r.uri.trim());
            let expected = if uri.is_empty() {
                FavoriteKind::Unplayable
            } else if uri.starts_with("x-rincon-cpcontainer:") {
                FavoriteKind::Container
            } else if uri.starts_with("x-rincon-mp3radio:") {
                FavoriteKind::Stream
            } else {
                FavoriteKind::Track
            };
            assert_eq!(f.kind, expected, "{} ({uri})", f.id);
            assert_eq!(f.uri.is_none(), expected == FavoriteKind::Unplayable);
            if f.kind != FavoriteKind::Unplayable {
                assert!(
                    !f.metadata.is_empty(),
                    "{}: playable favorites carry resMD",
                    f.id
                );
            }
        }
    }
    // The fixtures cover every kind between them.
    let all: Vec<FavoriteKind> = [FAV_S1, FAV_S2]
        .iter()
        .flat_map(|b| parsed(b))
        .map(|f| f.kind)
        .collect();
    for kind in [
        FavoriteKind::Track,
        FavoriteKind::Stream,
        FavoriteKind::Container,
        FavoriteKind::Unplayable,
    ] {
        assert!(all.contains(&kind), "{kind:?} missing from the fixtures");
    }
}

fn fav(title: &str, kind: FavoriteKind) -> Favorite {
    Favorite {
        id: format!("FV:2/{title}"),
        title: title.into(),
        kind,
        uri: (kind != FavoriteKind::Unplayable).then(|| format!("x-file:{title}")),
        metadata: String::new(),
        description: None,
        art_uri: None,
    }
}

fn shelf() -> Vec<Favorite> {
    vec![
        fav("Nocturne in E-flat", FavoriteKind::Track),
        fav("Aria", FavoriteKind::Track),
        fav("Sim Symphonies", FavoriteKind::Container),
        fav("Sim Quartets", FavoriteKind::Container),
        fav("Sim Radio", FavoriteKind::Stream),
        fav("Dvořák Recordings", FavoriteKind::Unplayable),
    ]
}

#[test]
fn favorites_are_found_by_position_title_prefix_and_words() {
    let s = shelf();
    let title = |q: &str| favorites::find(&s, q).map(|f| f.title.clone()).unwrap();
    assert_eq!(title("2"), "Aria", "1-based position");
    assert_eq!(title("ARIA"), "Aria", "exact, case-insensitive");
    assert_eq!(title("noct"), "Nocturne in E-flat", "unique prefix");
    assert_eq!(
        title("quartets sim"),
        "Sim Quartets",
        "every word, any order"
    );
    assert_eq!(title("dvorak"), "Dvořák Recordings", "accents ignored");
    match favorites::find(&s, "sim") {
        Err(FavoriteError::Ambiguous { candidates, .. }) => assert_eq!(candidates.len(), 3),
        other => panic!("expected ambiguity, got {other:?}"),
    }
    match favorites::find(&s, "sim cantatas") {
        Err(FavoriteError::Unknown { suggestions, .. }) => {
            assert_eq!(suggestions, ["Sim Symphonies", "Sim Quartets", "Sim Radio"]);
        }
        other => panic!("expected no match, got {other:?}"),
    }
    assert!(matches!(
        favorites::find(&s, "99"),
        Err(FavoriteError::Unknown { .. })
    ));
}

/// Records each action and answers with `<FirstTrackNumberEnqueued>1`.
#[derive(Default)]
struct Recorder(RefCell<Vec<(IpAddr, String, String)>>);

impl Transport for Recorder {
    fn soap_post(
        &self,
        host: IpAddr,
        _: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap()
            .to_string();
        self.0
            .borrow_mut()
            .push((host, action.clone(), body.into()));
        let out = if action == "AddURIToQueue" {
            "<FirstTrackNumberEnqueued>1</FirstTrackNumberEnqueued>"
        } else {
            ""
        };
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:{action}Response xmlns:u=\"urn:x\">{out}</u:{action}Response></s:Body></s:Envelope>"
        ))
    }
}

impl Recorder {
    fn actions(&self) -> Vec<String> {
        self.0.borrow().iter().map(|(_, a, _)| a.clone()).collect()
    }
}

fn house() -> Vec<HouseholdState> {
    let zgs = topology::parse_zone_group_state(
        soap::parse_response(ZGS_S1, "GetZoneGroupState")
            .unwrap()
            .require("ZoneGroupState")
            .unwrap(),
    )
    .unwrap();
    let mut st = HouseholdState::default();
    st.apply_topology(&zgs);
    vec![st]
}

fn coordinator() -> PlayerId {
    PlayerId("RINCON_000E58A0000201400".into())
}

#[test]
fn each_kind_plays_with_the_right_soap() {
    let h = house();
    let s = shelf();

    let t = Recorder::default();
    favorites::play(&t, &h, &coordinator(), &s[0]).unwrap();
    assert_eq!(
        t.actions(),
        ["SetAVTransportURI", "Play"],
        "a track renders directly"
    );
    assert!(t.0.borrow()[0].2.contains("x-file:Nocturne in E-flat"));

    let t = Recorder::default();
    favorites::play(&t, &h, &coordinator(), &s[4]).unwrap();
    assert_eq!(
        t.actions(),
        ["SetAVTransportURI", "Play"],
        "a stream renders directly"
    );

    let t = Recorder::default();
    favorites::play(&t, &h, &coordinator(), &s[2]).unwrap();
    assert_eq!(
        t.actions(),
        [
            "RemoveAllTracksFromQueue",
            "AddURIToQueue",
            "SetAVTransportURI",
            "Seek",
            "Play"
        ],
        "a container replaces the queue and plays it from the top"
    );
    assert!(
        t.0.borrow()
            .iter()
            .all(|(host, ..)| *host == "192.0.2.13".parse::<IpAddr>().unwrap())
    );

    let t = Recorder::default();
    assert!(matches!(
        favorites::play(&t, &h, &coordinator(), &s[5]),
        Err(FavoriteError::Unplayable { .. })
    ));
    assert!(t.actions().is_empty(), "nothing is sent for a shortcut");
}

#[test]
fn listing_browses_fv2_on_the_coordinator() {
    struct Browse;
    impl Transport for Browse {
        fn soap_post(
            &self,
            host: IpAddr,
            _: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            assert_eq!(host, "192.0.2.13".parse::<IpAddr>().unwrap());
            assert!(action.ends_with("#Browse\"") && body.contains("<ObjectID>FV:2</ObjectID>"));
            Ok(FAV_S1.to_string())
        }
    }
    let listed = favorites::list(&Browse, &house(), &coordinator()).unwrap();
    assert_eq!(listed, parsed(FAV_S1));
}

#[test]
fn a_favorite_is_found_by_the_id_listings_hand_out() {
    for body in [FAV_S1, FAV_S2] {
        let favs = parsed(body);
        for f in &favs {
            let found = favorites::find(&favs, &format!(" {} ", f.id)).unwrap();
            assert_eq!(found.id, f.id, "{}", f.id);
        }
    }
    let favs = parsed(FAV_S1);
    assert!(
        favorites::find(&favs, "FV:2/999").is_err(),
        "an id that is not there is not a match"
    );
}
