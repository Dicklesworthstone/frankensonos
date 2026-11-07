//! Identity outages cannot erase observations of a retained private read.

use super::*;

#[test]
fn identity_http_failures_leave_retained_pages_and_public_ownership_unchanged() {
    for status in [503, 429] {
        for adopted in [false, true] {
            let server = Server::new();
            liked_library(&server, 5);
            runtime().block_on(async {
                let cx = Cx::current().unwrap();
                let file = server.data.join("identity-outage-progress.db");
                let mut store = SqliteStore::open(&file).unwrap();
                if adopted {
                    store
                        .adopt_spotify_library_owner("previous-public-owner")
                        .unwrap();
                }
                let owner = store.spotify_library_owner().unwrap();
                apply_library_read(&mut store, &[held(0)]).unwrap();
                let public_library = store.library().unwrap();
                let first = attempt(&server, &cx, &mut store, 4).await;
                assert!(matches!(
                    first.result,
                    Err(SpotifyError::LibraryPending { .. })
                ));
                assert!(first.progress_observed);
                let retained = first.progress.unwrap();
                let encoded = store
                    .spotify_library_progress(OWNER, owner.as_ref())
                    .unwrap()
                    .unwrap();
                let albums_read = server.calls("/v1/me/albums").len();
                let tracks_read = server.calls("/v1/me/tracks").len();
                store.close().unwrap();

                let mut store = SqliteStore::open(&file).unwrap();
                server
                    .state
                    .lock()
                    .unwrap()
                    .refused
                    .insert("/v1/me".into(), status);
                let mut session = server.session(&cx, "");
                let failed =
                    sync_library_attempt_with_budget(&mut session, &cx, &mut store, 4).await;
                assert!(matches!(
                    failed.result,
                    Err(SpotifyError::Api { status: actual, .. }) if actual == status
                ));
                assert!(
                    !failed.progress_observed,
                    "identity did not observe saved pages"
                );
                assert!(failed.progress.is_none());
                assert_eq!(
                    store
                        .spotify_library_progress(OWNER, owner.as_ref())
                        .unwrap(),
                    Some(encoded)
                );
                assert_eq!(store.spotify_library_owner().unwrap(), owner);
                assert_eq!(store.library().unwrap(), public_library);
                assert_eq!(server.calls("/v1/me/albums").len(), albums_read);
                assert_eq!(server.calls("/v1/me/tracks").len(), tracks_read);
                if status == 429 {
                    assert!(session.retry_after_secs() > 0);
                }
                store.close().unwrap();

                server.state.lock().unwrap().refused.remove("/v1/me");
                let mut store = SqliteStore::open(&file).unwrap();
                let resumed = attempt(&server, &cx, &mut store, 4).await;
                assert!(matches!(
                    resumed.result,
                    Err(SpotifyError::LibraryPending { .. })
                ));
                assert!(resumed.progress_observed);
                let progress = resumed.progress.unwrap();
                assert!(progress.pages > retained.pages);
                assert!(progress.tracks > retained.tracks);
                assert_eq!(store.spotify_library_owner().unwrap(), owner);
                assert_eq!(store.library().unwrap(), public_library);
                let complete = finish(&server, &cx, &mut store).await;
                assert_eq!(complete.library.tracks, 5);
                assert_eq!(
                    store.spotify_library_owner().unwrap().unwrap().owner_id,
                    OWNER
                );
                store.close().unwrap();
            });
        }
    }
}
