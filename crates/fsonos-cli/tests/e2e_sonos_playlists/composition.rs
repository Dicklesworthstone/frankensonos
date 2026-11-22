//! The same composition command works directly and through the selected daemon.

use super::*;
use fsonos_api::surface::sonos_playlists::AppendedSonosPlaylistDto;

fn contents(s: &mut Scenario, route: &str, room: &str, playlist: &str) -> SonosPlaylistContentsDto {
    let read = call(
        s,
        route,
        "composition-contents",
        &["playlist-contents", room, playlist],
    );
    assert!(read.ok(), "{read:?}");
    serde_json::from_str(&read.stdout).unwrap()
}

fn resources(page: &SonosPlaylistContentsDto) -> Vec<String> {
    page.items
        .iter()
        .map(|item| item.uri.clone().unwrap())
        .collect()
}

pub(super) fn exercise(s: &mut Scenario, route: &str, room: &str) {
    let host = s.ip(room);
    let queue = browse_all(&s.lan(), host, QUEUE).unwrap();
    let playing = soap::get_position_info(&s.lan(), host).unwrap();
    let transport = soap::get_transport_info(&s.lan(), host).unwrap();
    for source_playlist in [None, Some("SQ:7")] {
        let destination = contents(s, route, room, "SQ:8");
        let (source_revision, source_resources) = if let Some(playlist) = source_playlist {
            let source = contents(s, route, room, playlist);
            (source.revision.clone(), resources(&source))
        } else {
            let read = call(s, route, "composition-queue", &["queue", room]);
            assert!(read.ok(), "{read:?}");
            let observed: QueueDto = serde_json::from_str(&read.stdout).unwrap();
            (
                observed.revision.unwrap(),
                queue
                    .iter()
                    .map(|item| item.res.as_ref().unwrap().uri.clone())
                    .collect(),
            )
        };
        let mut expected = resources(&destination);
        expected.extend(
            [2, 1, 2]
                .into_iter()
                .map(|position| source_resources[position - 1].clone()),
        );
        let mut args = vec![
            "playlist-append-tracks",
            room,
            "SQ:8",
            "--expected-revision",
            &destination.revision,
            "--source-revision",
            &source_revision,
            "--positions",
            "2,1,2",
        ];
        if let Some(playlist) = source_playlist {
            args.extend(["--source-playlist", playlist]);
        }
        let appended = call(s, route, "composition-append", &args);
        assert!(appended.ok(), "{appended:?}");
        let receipt: AppendedSonosPlaylistDto = serde_json::from_str(&appended.stdout).unwrap();
        let after = contents(s, route, room, "SQ:8");
        s.check(
            "composition-append",
            "cli",
            "saved composition preserves selected order and repeats from either native source",
            receipt.playlist.id == "SQ:8"
                && receipt.positions == [2, 1, 2]
                && receipt.previous_total == destination.total
                && receipt.total == destination.total + 3
                && receipt.added == 3
                && receipt.revision == after.revision
                && receipt.logged
                && resources(&after) == expected,
            &appended.stdout,
        );

        // Replaying the original destination revision must not duplicate the append.
        let stale = call(s, route, "composition-stale-destination", &args);
        assert!(!stale.ok(), "{stale:?}");
        let mut changed_source = args.clone();
        let destination_index = changed_source
            .iter()
            .position(|arg| *arg == "--expected-revision")
            .unwrap()
            + 1;
        changed_source[destination_index] = &after.revision;
        let source_index = changed_source
            .iter()
            .position(|arg| *arg == "--source-revision")
            .unwrap()
            + 1;
        changed_source[source_index] = "obsolete-source";
        let stale = call(s, route, "composition-stale-source", &changed_source);
        assert!(!stale.ok(), "{stale:?}");
        assert_eq!(resources(&contents(s, route, room, "SQ:8")), expected);
        if let Some(playlist) = source_playlist {
            assert_eq!(
                resources(&contents(s, route, room, playlist)),
                source_resources
            );
        }
        s.check(
            "composition-playback",
            "cli",
            "composing saved tracks leaves the live queue and playback unchanged",
            browse_all(&s.lan(), host, QUEUE).unwrap() == queue
                && soap::get_position_info(&s.lan(), host).unwrap() == playing
                && soap::get_transport_info(&s.lan(), host).unwrap() == transport,
            &appended.stdout,
        );
    }
}
