//! Native source repair is shared by queue selections and same-house moves.

use super::*;
use fsonos_core::playlists::{PreparedQueueSelection, queue_selection_revision};
use fsonos_proto::content::{self, BrowseFlag};

#[test]
fn queue_occurrence_copies_retain_native_identity_even_without_renderer_flags() {
    for generation in [1, 2] {
        for complete in [false, true] {
            for favorites in [false, true] {
                let f = Fixture::new(generation);
                f.queue(true);
                if !favorites {
                    f.sim.retain_favorites(generation, |_| false).unwrap();
                }
                let replies = Replies {
                    f: &f,
                    complete,
                    damage: if complete {
                        Some(Damage::EmptyCurrent)
                    } else if !favorites {
                        Some(Damage::WithoutFlags)
                    } else {
                        None
                    },
                    target_read: Cell::new(false),
                };
                let source_before = control::playback(&replies, &f.houses, &f.source_id).unwrap();
                let target = resolve_room(&f.houses, "Target")
                    .unwrap()
                    .coordinator
                    .id
                    .clone();
                let target_before = control::playback(&replies, &f.houses, &target).unwrap();
                let page =
                    content::browse(&replies, f.source, QUEUE, BrowseFlag::DirectChildren, 0, 3)
                        .unwrap();
                let revision = queue_selection_revision(&f.source_id, &page);
                let destination =
                    control::TrackBatchDestination::read(&replies, f.target, &target).unwrap();
                let start = f.sim.soap_log().len();
                let selection = PreparedQueueSelection::read(
                    &replies,
                    &f.houses,
                    &f.source_id,
                    &[3, 1, 3],
                    &revision,
                )
                .unwrap();
                assert_eq!(selection.positions(), [3, 1, 3]);
                assert_eq!(selection.tracks()[0], selection.tracks()[2]);
                for track in selection.tracks() {
                    assert!(track.metadata.contains("v:license='retained'"));
                    assert!(
                        track
                            .metadata
                            .contains("<![CDATA[native <payload> & bytes]]>")
                    );
                    assert!(track.metadata.contains(&f.parameters.cdudn));
                    assert!(track.id.starts_with(&f.parameters.item_id_prefix));
                }
                let queued = control::enqueue_native_tracks_bound(
                    &replies,
                    &f.houses,
                    &target,
                    selection.tracks(),
                    true,
                    &destination,
                )
                .unwrap();
                assert_eq!(queued.first, 2);
                assert!(!queued.playing_queue, "another source keeps playing");
                let copied = browse_snapshot(&f.lan, f.target, QUEUE).unwrap();
                let tracks = copied
                    .objects
                    .iter()
                    .skip(1)
                    .map(|item| {
                        spotify_uri_from_renderer_uri(&item.res.as_ref().unwrap().uri).unwrap()
                    })
                    .collect::<Vec<_>>();
                assert_eq!(tracks, [LAST, TRACK, LAST]);
                assert_eq!(
                    control::playback(&replies, &f.houses, &f.source_id).unwrap(),
                    source_before
                );
                assert_eq!(
                    control::playback(&replies, &f.houses, &target).unwrap(),
                    target_before
                );
                selection.confirm(&replies, &f.houses).unwrap();
                if !favorites {
                    f.assert_no_favorites(start);
                }
                let log = f.sim.soap_log();
                let writes = log[start..]
                    .iter()
                    .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
                    .collect::<Vec<_>>();
                assert_eq!(writes.len(), 3);
                assert!(writes.iter().all(|call| call.action == "AddURIToQueue"));
                for write in writes {
                    let metadata = &write
                        .args
                        .iter()
                        .find(|(name, _)| name == "EnqueuedURIMetaData")
                        .unwrap()
                        .1;
                    assert!(metadata.contains("v:license='retained'"));
                    assert!(metadata.contains("<![CDATA[native <payload> & bytes]]>"));
                }
                // A later source edit cannot authorize another copy with this selection.
                soap::add_uri_to_queue(
                    &f.lan,
                    f.source,
                    "https://example.invalid/later.mp3",
                    "",
                    false,
                )
                .unwrap();
                assert!(selection.confirm(&replies, &f.houses).is_err());
            }
        }
    }
}
