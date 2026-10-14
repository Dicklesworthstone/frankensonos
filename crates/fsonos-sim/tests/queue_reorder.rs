//! Native queue moves over real localhost SOAP on both generations.

use fsonos_proto::ProtoError;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::queue::reorder_tracks;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::time::Duration;

fn sim(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let specs = [
        SimPlayerSpec::new("Alpha", model),
        SimPlayerSpec::new("Beta", model),
    ];
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2(specs)
    } else {
        builder.s1(specs)
    }
    .spawn()
    .unwrap();
    let lan = sim.lan();
    let host = sim.player("Alpha").unwrap().ip;
    for name in ["a", "b", "c", "d", "e"] {
        soap::enqueue_uri(
            &lan,
            host,
            &format!("x-file-cifs://example.invalid/{name}.flac"),
            "",
            0,
            false,
        )
        .unwrap();
    }
    let queue_uri = format!("x-rincon-queue:{}#0", sim.player("Alpha").unwrap().uuid);
    soap::set_av_transport_uri(&lan, host, &queue_uri, "").unwrap();
    soap::seek_track(&lan, host, 2).unwrap();
    soap::play(&lan, host).unwrap();
    sim
}

#[test]
fn range_moves_preserve_playback_and_can_append_on_s1_and_s2() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let lan = sim.lan();
        let host = sim.player("Alpha").unwrap().ip;
        sim.advance(Duration::from_secs(12));
        let position = soap::get_position_info(&lan, host).unwrap();
        let before = browse_snapshot(&lan, host, QUEUE).unwrap();
        let log_at = sim.soap_log().len();
        reorder_tracks(&lan, host, 2, 2, 6, before.update_id).unwrap();
        let after = browse_snapshot(&lan, host, QUEUE).unwrap();
        let uris: Vec<_> = after
            .objects
            .iter()
            .map(|o| o.res.as_ref().unwrap().uri.clone())
            .collect();
        let expected =
            ["a", "d", "e", "b", "c"].map(|n| format!("x-file-cifs://example.invalid/{n}.flac"));
        assert_eq!(uris, expected);
        let now = soap::get_position_info(&lan, host).unwrap();
        assert_eq!(now.uri, position.uri);
        assert_eq!(now.position_secs, position.position_secs);
        assert_eq!(now.track, 4);
        let log = sim.soap_log();
        let writes: Vec<_> = log[log_at..]
            .iter()
            .filter(|r| !r.action.starts_with("Get") && r.action != "Browse")
            .collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].action, "ReorderTracksInQueue");
        reorder_tracks(&lan, host, 4, 2, 2, after.update_id).unwrap();
        assert_eq!(soap::get_position_info(&lan, host).unwrap().track, 2);
        sim.shutdown();
    }
}

#[test]
fn stale_revision_and_group_member_calls_are_refused_without_reordering() {
    let sim = sim(true);
    let lan = sim.lan();
    let host = sim.player("Alpha").unwrap().ip;
    let before = browse_snapshot(&lan, host, QUEUE).unwrap();
    assert!(matches!(
        reorder_tracks(&lan, host, 1, 1, 6, before.update_id + 1),
        Err(ProtoError::SoapFault { .. })
    ));
    let beta = sim.player("Beta").unwrap().ip;
    let join = format!("x-rincon:{}", sim.player("Alpha").unwrap().uuid);
    soap::set_av_transport_uri(&lan, beta, &join, "").unwrap();
    assert!(matches!(
        reorder_tracks(&lan, beta, 1, 1, 6, before.update_id),
        Err(ProtoError::SoapFault { code: 800, .. })
    ));
    assert_eq!(browse_snapshot(&lan, host, QUEUE).unwrap(), before);
    sim.shutdown();
}
