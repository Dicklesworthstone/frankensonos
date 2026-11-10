//! Scene source admission reads real S1/S2 catalogs before any planned write.

use fsonos_core::favorites::FavoriteError;
use fsonos_core::scenes::{self, SceneError, SceneOp, ScenePlan};
use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

fn fixture(generation: u8) -> (SimHandle, SimLan, Vec<HouseholdState>) {
    let builder = SimHousehold::builder();
    let sim = if generation == 1 {
        builder.s1([
            SimPlayerSpec::new("Lead", SimModel::Play5Gen1),
            SimPlayerSpec::new("Member", SimModel::Play5Gen1),
        ])
    } else {
        builder.s2([
            SimPlayerSpec::new("Lead", SimModel::One),
            SimPlayerSpec::new("Member", SimModel::Play1),
        ])
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let host = sim.player("Lead").unwrap().ip;
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let lead = PlayerId(sim.player("Lead").unwrap().uuid.clone());
    let member = PlayerId(sim.player("Member").unwrap().uuid.clone());
    control::join(&lan, &[household], &member, &lead).unwrap();
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    soap::set_av_transport_uri(&lan, host, "test:existing-music", "").unwrap();
    soap::play(&lan, host).unwrap();
    (sim, lan, vec![household])
}

fn favorite(player: &PlayerId, name: &str) -> SceneOp {
    SceneOp::Favorite {
        coordinator: player.clone(),
        name: name.into(),
        start: true,
    }
}

fn uri(player: &PlayerId, source: &str) -> SceneOp {
    SceneOp::Uri {
        coordinator: player.clone(),
        uri: source.into(),
        metadata: String::new(),
        start: false,
    }
}

fn plan(source: SceneOp, member: &PlayerId) -> ScenePlan {
    ScenePlan {
        ops: vec![
            SceneOp::Leave {
                room: "Member".into(),
                player: member.clone(),
            },
            SceneOp::Volume {
                room: "Member".into(),
                player: member.clone(),
                level: 1,
            },
            source,
        ],
        notes: Vec::new(),
    }
}

fn only_reads(sim: &SimHandle, since: usize) {
    let log = sim.soap_log();
    assert!(log.len() > since, "preflight must consult the real catalog");
    assert!(
        log[since..]
            .iter()
            .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get")),
        "preflight changed a speaker: {:?}",
        &log[since..]
    );
}

#[test]
fn missing_favorites_refuse_before_a_prospective_coordinator_leaves_or_changes_volume() {
    for generation in [1, 2] {
        let (sim, lan, households) = fixture(generation);
        let member = resolve_room(&households, "Member").unwrap().player.id.clone();
        let proposed = plan(favorite(&member, "Removed station"), &member);
        let before = sim.soap_log().len();
        assert!(matches!(
            scenes::check_sources(&lan, &households, &proposed),
            Err(SceneError::Favorite(FavoriteError::Unknown { .. }))
        ));
        only_reads(&sim, before);
        let host = sim.player("Lead").unwrap().ip;
        assert_eq!(
            soap::get_transport_info(&lan, host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(
            soap::get_media_info(&lan, host).unwrap().uri,
            "test:existing-music"
        );
        sim.shutdown();
    }
}

#[test]
fn ordinary_favorites_and_multiple_sources_share_one_household_catalog_read() {
    for generation in [1, 2] {
        let (sim, lan, households) = fixture(generation);
        let member = resolve_room(&households, "Member").unwrap().player.id.clone();
        let lead = resolve_room(&households, "Lead").unwrap().player.id.clone();
        let mut proposed = plan(favorite(&member, "Sim Radio"), &member);
        proposed.ops.push(favorite(&lead, "Aria"));
        proposed
            .ops
            .push(uri(&member, "https://media.example.invalid/stream.mp3"));
        let before = sim.soap_log().len();
        scenes::check_sources(&lan, &households, &proposed).unwrap();
        only_reads(&sim, before);
        let log = sim.soap_log();
        assert_eq!(log[before..].len(), 1, "one catalog per household");
        assert_eq!(log[before].action, "Browse");
        sim.shutdown();
    }
}

#[test]
fn spotify_readiness_uses_the_requested_kinds_actual_renderer_template() {
    for generation in [1, 2] {
        let (sim, lan, households) = fixture(generation);
        let member = resolve_room(&households, "Member").unwrap().player.id.clone();
        sim.retain_favorites(generation, |source| source.contains("spotify%3aalbum%3a"))
            .unwrap();
        let before = sim.soap_log().len();
        let album = plan(uri(&member, "spotify:album:0SimAlbum"), &member);
        scenes::check_sources(&lan, &households, &album).unwrap();
        for source in [
            "spotify:track:0SimTrack",
            "spotify:episode:0SimEpisode",
            "spotify:playlist:0SimList",
        ] {
            let proposed = plan(uri(&member, source), &member);
            assert!(matches!(
                scenes::check_sources(&lan, &households, &proposed),
                Err(SceneError::Core(CoreError::NoSpotifyFavorite))
            ));
        }
        only_reads(&sim, before);
        sim.retain_favorites(generation, |_| false).unwrap();
        assert!(matches!(
            scenes::check_sources(&lan, &households, &album),
            Err(SceneError::Core(CoreError::NoSpotifyFavorite))
        ));
        sim.shutdown();
    }
}

/// Preserve the real server exchange, but model a native shortcut whose
/// favorite has no renderer resource. Such entries exist beside playable ones.
struct Shortcut<'a>(&'a SimLan);

impl Transport for Shortcut<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.0.soap_post(host, path, action, body).map(|reply| {
            reply.replace("x-rincon-mp3radio://stream.example.invalid/sim.mp3", "")
        })
    }
}

#[test]
fn unplayable_shortcuts_and_failed_catalog_reads_do_not_admit_a_scene() {
    for generation in [1, 2] {
        let (sim, lan, households) = fixture(generation);
        let member = resolve_room(&households, "Member").unwrap().player.id.clone();
        let proposed = plan(favorite(&member, "Sim Radio"), &member);
        let before = sim.soap_log().len();
        assert!(matches!(
            scenes::check_sources(&Shortcut(&lan), &households, &proposed),
            Err(SceneError::Favorite(FavoriteError::Unplayable { .. }))
        ));
        sim.upnp_fault("Member", "Browse", 501).unwrap();
        assert!(matches!(
            scenes::check_sources(&lan, &households, &proposed),
            Err(SceneError::Core(CoreError::Proto(ProtoError::SoapFault {
                code: 501,
                ..
            })))
        ));
        only_reads(&sim, before);
        sim.shutdown();
    }
}
