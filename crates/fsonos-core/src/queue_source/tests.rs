use super::{Queue, appended, play};
use crate::favorites::{self, Favorite, FavoriteKind};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::control::{self as soap, Enqueued};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const OLD: [&str; 2] = [
    "x-file-cifs://example.invalid/old-a.flac",
    "x-file-cifs://example.invalid/old-b.flac",
];
const OTHER: &str = "x-file-cifs://example.invalid/other.flac";

#[derive(Debug, Clone, Copy)]
enum Fault {
    None,
    RefusedAdd,
    LostAdd,
    EmptyAdd,
    WrongFirst,
    Unreadable,
    ChangedPrefix,
    ConcurrentRemoval,
    LostRemoval,
    RefusedRemoval,
    ZeroRevision,
}

struct Replies {
    inner: SimLan,
    fault: Fault,
    added: Cell<bool>,
    calls: RefCell<Vec<String>>,
}

fn field(body: &str, name: &str, value: &str) -> String {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open).unwrap() + open.len();
    let end = start + body[start..].find(&close).unwrap();
    let mut changed = body.to_string();
    changed.replace_range(start..end, value);
    changed
}

impl Replies {
    fn new(sim: &SimHandle, fault: Fault) -> Self {
        Self {
            inner: sim.lan(),
            fault,
            added: Cell::new(false),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost queue-source reply".into(),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|name| {
                [
                    "AddURIToQueue",
                    "RemoveTrackRangeFromQueue",
                    "RemoveAllTracksFromQueue",
                    "SetAVTransportURI",
                    "Seek",
                    "Play",
                ]
                .contains(&name.as_str())
            })
            .cloned()
            .collect()
    }
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if (name == "AddURIToQueue" && matches!(self.fault, Fault::RefusedAdd))
            || (name == "RemoveTrackRangeFromQueue"
                && matches!(self.fault, Fault::RefusedRemoval))
        {
            return Err(ProtoError::SoapFault {
                code: 800,
                reason: "injected definite refusal".into(),
            });
        }
        if name == "Browse" && self.added.get() && matches!(self.fault, Fault::Unreadable) {
            return Err(Self::lost(host));
        }
        if name == "RemoveTrackRangeFromQueue"
            && matches!(self.fault, Fault::ConcurrentRemoval)
        {
            // Another controller inserts before our observed prefix. The
            // simulator's native revision check must reject our old range.
            soap::enqueue_uri(&self.inner, host, OTHER, "", 1, false)?;
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == "AddURIToQueue" {
            self.added.set(true);
            match self.fault {
                Fault::LostAdd => return Err(Self::lost(host)),
                Fault::EmptyAdd => return Ok(field(&response, "NumTracksAdded", "0")),
                Fault::WrongFirst => {
                    return Ok(field(&response, "FirstTrackNumberEnqueued", "1"));
                }
                Fault::ChangedPrefix => {
                    soap::remove_track_range_from_queue(&self.inner, host, 0, 1, 1)?;
                    soap::enqueue_uri(&self.inner, host, OTHER, "", 1, false)?;
                }
                _ => {}
            }
        }
        if name == "Browse" && self.added.get() && matches!(self.fault, Fault::ZeroRevision) {
            return Ok(field(&response, "UpdateID", "0"));
        }
        if name == "RemoveTrackRangeFromQueue" && matches!(self.fault, Fault::LostRemoval) {
            return Err(Self::lost(host));
        }
        Ok(response)
    }
}

fn fixture(
    generation: u8,
    populated: bool,
) -> (SimHandle, Vec<HouseholdState>, PlayerId, IpAddr, Favorite) {
    let builder = SimHousehold::builder();
    let builder = if generation == 1 {
        builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
    } else {
        builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
    };
    let sim = builder.spawn().unwrap();
    let player = sim.player("Den").unwrap();
    let host = player.ip;
    let id = PlayerId(player.uuid.clone());
    let lan = sim.lan();
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let houses = vec![house];
    if populated {
        for uri in OLD {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        control::play_queue_from(&lan, &houses, &id, 1).unwrap();
    }
    let favorite = favorites::list(&lan, &houses, &id)
        .unwrap()
        .into_iter()
        .find(|favorite| favorite.kind == FavoriteKind::Container)
        .unwrap();
    (sim, houses, id, host, favorite)
}

fn queue(sim: &SimHandle, host: IpAddr) -> Vec<String> {
    super::read(&sim.lan(), host).unwrap().uris
}

fn still_playing_old(sim: &SimHandle, houses: &[HouseholdState], id: &PlayerId) {
    let playback = control::playback(&sim.lan(), houses, id).unwrap();
    assert_eq!(playback.transport.state, TransportState::Playing);
    assert_eq!(playback.position.uri, OLD[0]);
}

#[test]
fn favorite_replacement_plays_from_empty_and_populated_s1_and_s2_queues() {
    for generation in [1, 2] {
        for populated in [false, true] {
            let (sim, houses, id, host, favorite) = fixture(generation, populated);
            let replies = Replies::new(&sim, Fault::None);
            favorites::play(&replies, &houses, &id, &favorite).unwrap();
            let mut expected = vec!["AddURIToQueue"];
            if populated {
                expected.push("RemoveTrackRangeFromQueue");
            }
            expected.extend(["SetAVTransportURI", "Seek", "Play"]);
            assert_eq!(replies.writes(), expected);
            let current = queue(&sim, host);
            assert!(!current.is_empty());
            assert!(current.iter().all(|uri| !OLD.contains(&uri.as_str())));
            let playback = control::playback(&sim.lan(), &houses, &id).unwrap();
            assert_eq!(playback.transport.state, TransportState::Playing);
            assert_eq!(playback.position.track, 1);
            assert_eq!(playback.position.uri, current[0]);
        }
    }
}

#[test]
fn rejected_replacements_preserve_the_existing_music_on_both_generations() {
    for generation in [1, 2] {
        let (sim, houses, id, host, favorite) = fixture(generation, true);
        let replies = Replies::new(&sim, Fault::RefusedAdd);
        let error = play(
            &replies,
            &houses,
            &id,
            favorite.uri.as_deref().unwrap(),
            &favorite.metadata,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CoreError::Proto(ProtoError::SoapFault { code: 800, .. })
        ));
        assert_eq!(replies.writes(), ["AddURIToQueue"]);
        assert_eq!(queue(&sim, host), OLD);
        still_playing_old(&sim, &houses, &id);
    }
}

#[test]
fn unconfirmed_append_does_not_discard_old_music_or_replay_the_write() {
    for generation in [1, 2] {
        for fault in [
            Fault::LostAdd,
            Fault::EmptyAdd,
            Fault::WrongFirst,
            Fault::Unreadable,
            Fault::ZeroRevision,
        ] {
            let (sim, houses, id, host, favorite) = fixture(generation, true);
            let replies = Replies::new(&sim, fault);
            let error = play(
                &replies,
                &houses,
                &id,
                favorite.uri.as_deref().unwrap(),
                &favorite.metadata,
            )
            .unwrap_err();
            assert!(matches!(error, CoreError::Proto(ProtoError::Malformed(_))));
            assert_eq!(replies.writes(), ["AddURIToQueue"], "{fault:?}");
            let current = queue(&sim, host);
            assert_eq!(&current[..OLD.len()], OLD);
            assert!(current.len() > OLD.len(), "accepted items remain visible");
            still_playing_old(&sim, &houses, &id);
        }
    }
}

#[test]
fn a_changed_prefix_is_not_mistaken_for_the_original_queue() {
    let (sim, houses, id, host, favorite) = fixture(2, true);
    let replies = Replies::new(&sim, Fault::ChangedPrefix);
    assert!(
        play(
            &replies,
            &houses,
            &id,
            favorite.uri.as_deref().unwrap(),
            &favorite.metadata,
        )
        .is_err()
    );
    assert_eq!(replies.writes(), ["AddURIToQueue"]);
    assert_eq!(queue(&sim, host)[0], OTHER);
}

#[test]
fn removal_races_and_refusals_do_not_clear_restart_or_repeat_the_source() {
    for generation in [1, 2] {
        for fault in [Fault::ConcurrentRemoval, Fault::RefusedRemoval] {
            let (sim, houses, id, host, favorite) = fixture(generation, true);
            let replies = Replies::new(&sim, fault);
            let error = play(
                &replies,
                &houses,
                &id,
                favorite.uri.as_deref().unwrap(),
                &favorite.metadata,
            )
            .unwrap_err();
            assert!(matches!(error, CoreError::Proto(ProtoError::Malformed(_))));
            assert_eq!(
                replies.writes(),
                ["AddURIToQueue", "RemoveTrackRangeFromQueue"]
            );
            let current = queue(&sim, host);
            assert!(OLD.iter().all(|uri| current.iter().any(|item| item.as_str() == *uri)));
            if matches!(fault, Fault::ConcurrentRemoval) {
                assert_eq!(current[0], OTHER);
            }
            still_playing_old(&sim, &houses, &id);
        }
    }
}

#[test]
fn a_lost_removal_reply_completes_by_readback_without_another_mutation() {
    for generation in [1, 2] {
        let (sim, houses, id, host, favorite) = fixture(generation, true);
        let replies = Replies::new(&sim, Fault::LostRemoval);
        play(
            &replies,
            &houses,
            &id,
            favorite.uri.as_deref().unwrap(),
            &favorite.metadata,
        )
        .unwrap();
        assert_eq!(
            replies.writes(),
            [
                "AddURIToQueue",
                "RemoveTrackRangeFromQueue",
                "SetAVTransportURI",
                "Seek",
                "Play",
            ]
        );
        let current = queue(&sim, host);
        assert!(current.iter().all(|uri| !OLD.contains(&uri.as_str())));
        let playback = control::playback(&sim.lan(), &houses, &id).unwrap();
        assert_eq!(playback.transport.state, TransportState::Playing);
        assert_eq!(playback.position.uri, current[0]);
    }
}

#[test]
fn append_verification_requires_exact_nonempty_insertion_and_unchanged_prefix() {
    let image = |revision, uris: &[&str]| Queue {
        revision,
        uris: uris.iter().map(|uri| (*uri).to_string()).collect(),
    };
    let before = image(7, &["a", "b"]);
    let after = image(8, &["a", "b", "c"]);
    let added = Enqueued { first: 3, count: 1 };
    assert!(appended(&before, &after, added));
    for invalid in [
        Enqueued { first: 0, count: 1 },
        Enqueued { first: 1, count: 1 },
        Enqueued { first: 3, count: 0 },
        Enqueued { first: 3, count: 2 },
        Enqueued {
            first: u32::MAX,
            count: u32::MAX,
        },
    ] {
        assert!(!appended(&before, &after, invalid));
    }
    for invalid in [
        image(7, &["a", "b", "c"]),
        image(8, &["b", "a", "c"]),
        image(8, &["other", "b", "c"]),
        image(8, &["a", "b"]),
        image(8, &["a", "b", "c", "d"]),
    ] {
        assert!(!appended(&before, &invalid, added));
    }
    assert!(appended(
        &image(0, &[]),
        &image(1, &["a"]),
        Enqueued { first: 1, count: 1 },
    ));
}
