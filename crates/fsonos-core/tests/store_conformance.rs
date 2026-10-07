//! One behavioral suite, run against every `Store`: the in-memory `MemStore`,
//! fsqlite in memory, and fsqlite on disk (including survival across close
//! and reopen). Keeps the two implementations from drifting apart.

use fsonos_core::store::{
    AlbumTrack, CachedAlbum, DjSession, Feedback, FeedbackKey, LibraryEntry, LibraryOrigin,
    MemStore, SqliteStore, Store,
};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Generation, Player, PlayerId, Track, ZoneGroup};

fn pid(s: &str) -> PlayerId {
    PlayerId(s.into())
}

fn player(id: &str, room: &str, ip: &str, generation: Generation) -> Player {
    Player {
        id: pid(id),
        room_name: room.into(),
        ip: ip.parse().unwrap(),
        model: "Sonos One".into(),
        generation,
    }
}

fn entry(uri: &str, added: i64, artist: Option<&str>, classical: bool) -> LibraryEntry {
    LibraryEntry {
        track: Track {
            title: format!("Title {uri}"),
            artist: artist.map(str::to_string),
            album: None,
            source_uri: uri.into(),
            uri: Some("x-sonos-spotify:not-cached".into()),
            duration_secs: Some(431),
        },
        is_classical: classical,
        added,
        album_uri: None,
        album_artists: None,
        origin: LibraryOrigin::default(),
        disc_number: None,
        track_number: None,
        work_key: None,
    }
}

fn params(flags: u32) -> SpotifyRenderParams {
    SpotifyRenderParams {
        sid: 12,
        flags,
        sn: 1,
        cdudn: "SA_RINCON3079_X_#Svc3079-0-Token".into(),
        item_id_prefix: "10032020".into(),
    }
}

fn uris(plays: &[fsonos_core::store::PlayRecord]) -> Vec<&str> {
    plays.iter().map(|p| p.source_uri.as_str()).collect()
}

fn play_history(s: &mut dyn Store) {
    for (zone, uri, at) in [
        ("Den", "spotify:track:1", 100),
        ("Lounge", "spotify:track:2", 110),
        ("Den", "spotify:track:3", 120),
        ("Den", "spotify:track:1", 130),
        ("Den", "spotify:track:5", 140),
    ] {
        s.record_play(zone, uri, at).unwrap();
    }
    let all = s.recent_plays(None, 10).unwrap();
    assert_eq!(
        uris(&all),
        [
            "spotify:track:1",
            "spotify:track:2",
            "spotify:track:3",
            "spotify:track:1",
            "spotify:track:5"
        ]
    );
    assert_eq!((all[3].zone.as_str(), all[3].played_at), ("Den", 130));
    assert_eq!(
        uris(&s.recent_plays(None, 2).unwrap()),
        ["spotify:track:1", "spotify:track:5"]
    );
    assert_eq!(s.recent_plays(None, 0).unwrap().len(), 0);
    assert_eq!(
        uris(&s.recent_plays(Some("Den"), 2).unwrap()),
        ["spotify:track:1", "spotify:track:5"]
    );
    assert_eq!(
        uris(&s.recent_plays(Some("Lounge"), 5).unwrap()),
        ["spotify:track:2"]
    );
    assert_eq!(s.recent_plays(Some("Kitchen"), 5).unwrap().len(), 0);
    assert_eq!(s.recent_play_count("spotify:track:1", 10).unwrap(), 2);
    assert_eq!(s.recent_play_count("spotify:track:1", 2).unwrap(), 1);
    assert_eq!(s.recent_play_count("spotify:track:1", 1).unwrap(), 0);
    assert_eq!(s.recent_play_count("spotify:track:2", 3).unwrap(), 0);
}

fn inventory_cache(s: &mut dyn Store) {
    let a = player(
        "RINCON_A",
        "Owner\u{2019}s Study",
        "192.0.2.10",
        Generation::S1,
    );
    let b = player("RINCON_B", "Den", "192.0.2.11", Generation::S1);
    let c = player("RINCON_C", "Lounge", "192.0.2.20", Generation::S2);
    s.save_players("HH_S2", std::slice::from_ref(&c), 50)
        .unwrap();
    s.save_players("HH_S1", &[b.clone(), a.clone()], 100)
        .unwrap();

    // A later sighting updates one player and leaves the rest alone.
    let mut a2 = a.clone();
    a2.ip = "192.0.2.99".parse().unwrap();
    s.save_players("HH_S1", &[a2.clone()], 200).unwrap();

    let cached = s.cached_players().unwrap();
    let rows: Vec<_> = cached
        .iter()
        .map(|c| (c.household.as_str(), c.player.id.0.as_str(), c.last_seen))
        .collect();
    assert_eq!(
        rows,
        [
            ("HH_S1", "RINCON_A", 200),
            ("HH_S1", "RINCON_B", 100),
            ("HH_S2", "RINCON_C", 50)
        ]
    );
    assert_eq!(cached[0].player, a2);
    assert_eq!(cached[1].player, b);
    assert_eq!(cached[2].player, c);

    // Re-homing a player moves it rather than duplicating it.
    s.save_players("HH_S2", std::slice::from_ref(&b), 300)
        .unwrap();
    let homes: Vec<_> = s
        .cached_players()
        .unwrap()
        .into_iter()
        .map(|c| (c.household, c.player.id.0))
        .collect();
    assert_eq!(
        homes,
        [
            ("HH_S1".to_string(), "RINCON_A".to_string()),
            ("HH_S2".to_string(), "RINCON_B".to_string()),
            ("HH_S2".to_string(), "RINCON_C".to_string()),
        ]
    );

    let groups = vec![
        ZoneGroup {
            coordinator: pid("RINCON_B"),
            members: vec![pid("RINCON_B"), pid("RINCON_A")],
        },
        ZoneGroup {
            coordinator: pid("RINCON_D"),
            members: vec![pid("RINCON_D")],
        },
    ];
    s.save_groups("HH_S1", &groups, 10).unwrap();
    s.save_groups("HH_S2", &groups[1..], 10).unwrap();
    assert_eq!(s.cached_groups("HH_S1").unwrap(), groups);
    // Saving replaces the household's structure; other households are untouched.
    s.save_groups("HH_S1", &groups[..1], 20).unwrap();
    assert_eq!(s.cached_groups("HH_S1").unwrap(), groups[..1]);
    assert_eq!(s.cached_groups("HH_S2").unwrap(), groups[1..]);
    s.save_groups("HH_S1", &[], 30).unwrap();
    assert_eq!(s.cached_groups("HH_S1").unwrap().len(), 0);
    assert_eq!(s.cached_groups("HH_NONE").unwrap().len(), 0);
}

fn library_cache(s: &mut dyn Store) {
    s.upsert_library(&[
        entry("spotify:track:b", 20, Some("Bach; Glenn Gould"), true),
        entry("spotify:track:a", 20, None, false),
        entry("spotify:track:c", 10, Some("Mahler"), true),
    ])
    .unwrap();
    s.upsert_library(&[]).unwrap();
    // Replacing an entry keeps one row per source_uri.
    s.upsert_library(&[entry("spotify:track:a", 30, Some("Arvo P\u{e4}rt"), true)])
        .unwrap();
    let lib = s.library().unwrap();
    let order: Vec<_> = lib.iter().map(|e| e.track.source_uri.as_str()).collect();
    assert_eq!(
        order,
        ["spotify:track:c", "spotify:track:b", "spotify:track:a"]
    );
    let a = &lib[2];
    assert_eq!(a.track.artist.as_deref(), Some("Arvo P\u{e4}rt"));
    assert_eq!(a.track.album, None);
    assert_eq!(a.track.duration_secs, Some(431));
    assert_eq!(
        a.track.uri, None,
        "the renderer URI is household-specific: not cached"
    );
    assert!(a.is_classical);
    assert_eq!(a.added, 30);
    assert_eq!(lib[1].track.artist.as_deref(), Some("Bach; Glenn Gould"));
    assert_eq!(lib[1].origin, LibraryOrigin::SavedAlbum);

    // Album/work fields round-trip, and an update can change the origin.
    let mut movement = entry("spotify:track:m2", 40, Some("Mahler"), true);
    movement.album_uri = Some("spotify:album:sym5".into());
    movement.album_artists = Some("Mahler; Wiener Philharmoniker".into());
    movement.origin = LibraryOrigin::LikedTrack;
    movement.disc_number = Some(1);
    movement.track_number = Some(2);
    movement.work_key = Some("mahler|symphony no 5".into());
    s.upsert_library(std::slice::from_ref(&movement)).unwrap();
    movement.origin = LibraryOrigin::Both;
    s.upsert_library(std::slice::from_ref(&movement)).unwrap();
    let mut expected = movement.clone();
    expected.track.uri = None;
    assert_eq!(s.library().unwrap().last(), Some(&expected));
    assert_eq!(s.library().unwrap().len(), 4);
}

fn render_params_and_auth(s: &mut dyn Store) {
    assert_eq!(s.render_params("HH_S1").unwrap(), None);
    s.save_render_params("HH_S1", &params(8224), 1_000).unwrap();
    s.save_render_params("HH_S2", &params(8232), 1_001).unwrap();
    s.save_render_params("HH_S1", &params(8300), 2_000).unwrap();
    assert_eq!(
        s.render_params("HH_S1").unwrap(),
        Some((params(8300), 2_000))
    );
    assert_eq!(
        s.render_params("HH_S2").unwrap(),
        Some((params(8232), 1_001))
    );

    assert_eq!(s.auth("spotify").unwrap(), None);
    s.save_auth("spotify", "refresh-1", 3_600).unwrap();
    s.save_auth("spotify", "refresh-2", 7_200).unwrap();
    let auth = s.auth("spotify").unwrap().unwrap();
    assert_eq!(
        (auth.refresh_token.as_str(), auth.expires),
        ("refresh-2", 7_200)
    );
    assert_eq!(s.auth("other").unwrap(), None);
}

fn session(coordinator: &str, mood: Option<&str>, expires: i64) -> DjSession {
    DjSession {
        coordinator: coordinator.into(),
        mood: mood.map(str::to_string),
        constraints: mood.map(|_| r#"{"avoid":["opera"]}"#.to_string()),
        expires,
    }
}

fn dj_sessions(s: &mut dyn Store) {
    assert_eq!(s.dj_session("RINCON_B").unwrap(), None);
    s.save_dj_session(&session("RINCON_B", Some("calm"), 500))
        .unwrap();
    s.save_dj_session(&session("RINCON_A", None, 400)).unwrap();
    // Saving again replaces the coordinator's steering.
    s.save_dj_session(&session("RINCON_B", Some("bright"), 900))
        .unwrap();
    assert_eq!(
        s.dj_session("RINCON_B").unwrap(),
        Some(session("RINCON_B", Some("bright"), 900))
    );
    assert_eq!(
        s.dj_sessions().unwrap(),
        [
            session("RINCON_A", None, 400),
            session("RINCON_B", Some("bright"), 900)
        ]
    );
    s.delete_dj_session("RINCON_A").unwrap();
    s.delete_dj_session("RINCON_NONE").unwrap();
    assert_eq!(s.dj_sessions().unwrap().len(), 1);
}

fn fb(
    at: i64,
    work: Option<&str>,
    composer: &str,
    performer: Option<&str>,
    signal: i64,
) -> Feedback {
    Feedback {
        at,
        work_key: work.map(str::to_string),
        composer_key: Some(composer.into()),
        performer: performer.map(str::to_string),
        signal,
    }
}

fn feedback(s: &mut dyn Store) {
    let skip = fb(
        100,
        Some("mahler|symphony no 5"),
        "mahler",
        Some("Abbado"),
        -1,
    );
    let like = fb(200, Some("bach|goldberg"), "bach", Some("Gould"), 2);
    let late = fb(300, Some("mahler|symphony no 5"), "mahler", None, 1);
    let tie = fb(300, None, "mahler", Some("Abbado"), -2);
    for f in [&late, &skip, &like, &tie] {
        s.record_feedback(f).unwrap();
    }
    let all_time = 0..i64::MAX;
    assert_eq!(
        s.feedback(FeedbackKey::Work("mahler|symphony no 5"), all_time.clone())
            .unwrap(),
        [skip.clone(), late.clone()]
    );
    // Oldest first; equal times keep recording order.
    assert_eq!(
        s.feedback(FeedbackKey::Composer("mahler"), all_time.clone())
            .unwrap(),
        [skip.clone(), late.clone(), tie.clone()]
    );
    assert_eq!(
        s.feedback(FeedbackKey::Performer("Abbado"), all_time.clone())
            .unwrap(),
        [skip.clone(), tie.clone()]
    );
    // The window's end is exclusive.
    assert_eq!(
        s.feedback(FeedbackKey::Composer("mahler"), 100..300)
            .unwrap(),
        [skip]
    );
    assert_eq!(
        s.feedback(FeedbackKey::Composer("mahler"), 300..301)
            .unwrap(),
        [late, tie]
    );
    assert_eq!(
        s.feedback(FeedbackKey::Composer("bach"), 201..i64::MAX)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        s.feedback(FeedbackKey::Work("unknown"), all_time)
            .unwrap()
            .len(),
        0
    );
}

fn track(disc: u32, number: u32, title: &str) -> AlbumTrack {
    AlbumTrack {
        disc_number: disc,
        track_number: number,
        source_uri: format!("spotify:track:{disc}-{number}"),
        title: title.into(),
        duration_secs: (number != 3).then_some(600 + number),
    }
}

fn album_tracks(s: &mut dyn Store) {
    let album = "spotify:album:sym5";
    assert_eq!(s.album_tracks(album).unwrap(), None);
    s.save_album_tracks(
        album,
        &[
            track(2, 1, "IV. Adagietto"),
            track(1, 2, "II. Stürmisch bewegt"),
            track(1, 1, "I. Trauermarsch"),
            track(1, 2, "II. Stürmisch bewegt, mit größter Vehemenz"),
            track(1, 3, "III. Scherzo"),
        ],
        1_000,
    )
    .unwrap();
    s.save_album_tracks("spotify:album:other", &[track(1, 1, "Aria")], 1_001)
        .unwrap();
    assert_eq!(
        s.album_tracks(album).unwrap(),
        Some(CachedAlbum {
            tracks: vec![
                track(1, 1, "I. Trauermarsch"),
                track(1, 2, "II. Stürmisch bewegt, mit größter Vehemenz"),
                track(1, 3, "III. Scherzo"),
                track(2, 1, "IV. Adagietto"),
            ],
            fetched_at: 1_000,
        })
    );
    // A refetch replaces the whole list.
    s.save_album_tracks(album, &[track(1, 1, "I.")], 2_000)
        .unwrap();
    let refetched = s.album_tracks(album).unwrap().unwrap();
    assert_eq!((refetched.tracks.len(), refetched.fetched_at), (1, 2_000));
    // An empty list forgets the album; others are untouched.
    s.save_album_tracks(album, &[], 3_000).unwrap();
    assert_eq!(s.album_tracks(album).unwrap(), None);
    assert_eq!(
        s.album_tracks("spotify:album:other")
            .unwrap()
            .unwrap()
            .tracks
            .len(),
        1
    );
}

fn suite(s: &mut dyn Store) {
    play_history(s);
    inventory_cache(s);
    library_cache(s);
    render_params_and_auth(s);
    dj_sessions(s);
    feedback(s);
    album_tracks(s);
}

#[test]
fn mem_store_conforms() {
    suite(&mut MemStore::default());
}

#[test]
fn sqlite_in_memory_conforms() {
    let mut s = SqliteStore::open_in_memory().unwrap();
    suite(&mut s);
    assert_eq!(s.schema_versions().unwrap(), [1, 2, 3]);
    s.close().unwrap();
}

#[test]
fn sqlite_file_survives_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fsonos.db");

    let mut s = SqliteStore::open(&path).unwrap();
    suite(&mut s);
    s.close().unwrap();

    // Reopening re-runs no migrations and sees every committed write.
    let s = SqliteStore::open(&path).unwrap();
    assert_eq!(s.schema_versions().unwrap(), [1, 2, 3]);
    assert_eq!(s.recent_plays(None, 10).unwrap().len(), 5);
    assert_eq!(s.cached_players().unwrap().len(), 3);
    assert_eq!(s.cached_groups("HH_S2").unwrap().len(), 1);
    assert_eq!(s.library().unwrap().len(), 4);
    assert_eq!(
        s.render_params("HH_S1").unwrap(),
        Some((params(8300), 2_000))
    );
    assert_eq!(
        s.auth("spotify").unwrap().unwrap().refresh_token,
        "refresh-2"
    );
    assert_eq!(
        s.dj_session("RINCON_B").unwrap(),
        Some(session("RINCON_B", Some("bright"), 900))
    );
    assert_eq!(
        s.feedback(FeedbackKey::Composer("mahler"), 0..i64::MAX)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        s.album_tracks("spotify:album:other")
            .unwrap()
            .unwrap()
            .fetched_at,
        1_001
    );
    assert_eq!(
        s.library().unwrap().last().unwrap().work_key.as_deref(),
        Some("mahler|symphony no 5")
    );

    // Dropping without close is also safe: the WAL carries committed writes.
    let mut s = s;
    s.record_play("Den", "spotify:track:9", 999).unwrap();
    drop(s);
    let s = SqliteStore::open(&path).unwrap();
    assert_eq!(
        uris(&s.recent_plays(Some("Den"), 1).unwrap()),
        ["spotify:track:9"]
    );
    s.close().unwrap();
}
