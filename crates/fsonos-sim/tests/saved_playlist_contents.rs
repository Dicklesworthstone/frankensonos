//! Native saved queues are household catalogs, independent of live playback.

use fsonos_proto::ProtoError;
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::control::{add_uri_to_queue, remove_all_tracks_from_queue, save_queue};
use fsonos_proto::didl::{DidlKind, item_metadata};
use fsonos_sim::SimHousehold;

#[test]
fn native_playlist_children_keep_order_duplicates_metadata_and_household_identity() {
    let sim = SimHousehold::standard().spawn().unwrap();
    for (room, generation) in [("Kitchen", 1), ("Living Room", 2)] {
        let transport = sim.transport(room).unwrap();
        let host = transport.ip();
        let catalog = content::browse_snapshot(&transport, host, content::SONOS_PLAYLISTS).unwrap();
        let listed = catalog
            .objects
            .iter()
            .find(|item| item.id == "SQ:7")
            .unwrap();
        let metadata =
            content::browse(&transport, host, "SQ:7", BrowseFlag::Metadata, 0, 1).unwrap();
        assert_eq!(metadata.objects, [listed.clone()]);
        assert_eq!((metadata.number_returned, metadata.total_matches), (1, 1));
        assert_eq!(metadata.update_id, catalog.update_id);

        let all = content::browse_snapshot(&transport, host, "SQ:7").unwrap();
        assert_eq!(all.update_id, catalog.update_id);
        assert_eq!(all.objects.len(), 3);
        assert_eq!(
            all.objects[0].res, all.objects[2].res,
            "duplicate occurrences remain"
        );
        for (index, item) in all.objects.iter().enumerate() {
            assert_eq!(item.id, format!("SQ:7/{}", index + 1));
            assert_eq!(item.parent_id, "SQ:7");
            assert_eq!(item.kind, DidlKind::Item);
            assert_eq!(item.creator.as_deref(), Some("Sim Saved Artist"));
            assert_eq!(item.album, Some(format!("S{generation} Saved Album")));
            assert_eq!(item.res.as_ref().unwrap().duration_secs(), Some(180));
            assert_eq!(
                item.desc.as_ref().unwrap().value,
                sim.render_params(generation).unwrap().cdudn
            );
            assert!(
                item.album_art_uri
                    .as_deref()
                    .is_some_and(|uri| uri.starts_with("/getaa?"))
            );
        }
        let middle =
            content::browse(&transport, host, "SQ:7", BrowseFlag::DirectChildren, 1, 1).unwrap();
        assert_eq!(middle.objects, all.objects[1..2]);
        assert_eq!((middle.number_returned, middle.total_matches), (1, 3));
        assert_eq!(middle.update_id, all.update_id);
        let past_end = content::browse(
            &transport,
            host,
            "SQ:7",
            BrowseFlag::DirectChildren,
            u32::MAX,
            100,
        )
        .unwrap();
        assert!(past_end.objects.is_empty());
        assert_eq!((past_end.number_returned, past_end.total_matches), (0, 3));
        for missing in ["SQ:999", "SQ:x", "SQ:7/1", "SQ:+7"] {
            assert!(
                matches!(
                    content::browse(&transport, host, missing, BrowseFlag::DirectChildren, 0, 10),
                    Err(ProtoError::SoapFault { code: 701, .. })
                ),
                "{missing}"
            );
        }
        assert!(
            content::browse_snapshot(&transport, host, content::QUEUE)
                .unwrap()
                .objects
                .is_empty()
        );
    }
    assert!(sim.soap_log().iter().all(|entry| entry.action == "Browse"));
}

#[test]
fn a_newly_saved_queue_can_be_browsed_after_the_live_queue_is_cleared() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let transport = sim.transport("Kitchen").unwrap();
    let host = transport.ip();
    let before = content::browse_snapshot(&transport, host, content::SONOS_PLAYLISTS).unwrap();
    let entries = content::browse_snapshot(&transport, host, "SQ:7").unwrap();
    for item in &entries.objects {
        add_uri_to_queue(
            &transport,
            host,
            &item.res.as_ref().unwrap().uri,
            &item_metadata(item),
            false,
        )
        .unwrap();
    }
    let saved_id = save_queue(&transport, host, "Preserved & ordered").unwrap();
    let saved = content::browse_snapshot(&transport, host, &saved_id).unwrap();
    assert_ne!(saved.update_id, before.update_id);
    assert_eq!(saved.objects.len(), entries.objects.len());
    for (position, (expected, actual)) in entries.objects.iter().zip(&saved.objects).enumerate() {
        assert_eq!(actual.id, format!("{saved_id}/{}", position + 1));
        assert_eq!(actual.parent_id, saved_id);
        assert_eq!(actual.title, expected.title);
        assert_eq!(actual.creator, expected.creator);
        assert_eq!(actual.album, expected.album);
        assert_eq!(actual.desc, expected.desc);
        assert_eq!(actual.res, expected.res);
    }
    remove_all_tracks_from_queue(&transport, host).unwrap();
    let after = content::browse_snapshot(&transport, host, &saved_id).unwrap();
    assert_eq!(
        after, saved,
        "live queue edits cannot change saved contents"
    );
    let other_household = sim.transport("Living Room").unwrap();
    assert!(matches!(
        content::browse_snapshot(&other_household, other_household.ip(), &saved_id),
        Err(ProtoError::SoapFault { code: 701, .. })
    ));
}
