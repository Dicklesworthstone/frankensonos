//! One behavioral suite, run against every `Store`: the in-memory `MemStore`,
//! fsqlite in memory, and fsqlite on disk (including survival across close
//! and reopen). Keeps the two implementations from drifting apart.

use fsonos_core::store::{
    Action, ActionFilter, AlbumTrack, CachedAlbum, DjSession, Feedback, FeedbackKey, LibraryEntry,
    LibraryOrigin, MemStore, SqliteStore, Store, StoredScene, StoredSchedule,
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
        ..LibraryEntry::default()
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
    // The genre-agnostic fields too (migration 7).
    movement.genres = vec!["classical".into(), "late romantic era".into()];
    movement.release_year = Some(1987);
    movement.explicit = true;
    movement.candidate = Some(true);
    movement.taste_weight = Some(1300);
    s.upsert_library(std::slice::from_ref(&movement)).unwrap();
    movement.origin = LibraryOrigin::Both;
    s.upsert_library(std::slice::from_ref(&movement)).unwrap();
    let mut expected = movement.clone();
    expected.track.uri = None;
    assert_eq!(s.library().unwrap().last(), Some(&expected));
    assert_eq!(s.library().unwrap().len(), 4);
    // A track known only from the owner's other taste signals.
    let mut heard = entry("spotify:track:heard", 50, Some("Nina Marsh Quartet"), false);
    heard.origin = LibraryOrigin::Taste;
    heard.candidate = Some(true);
    heard.taste_weight = Some(1100);
    s.upsert_library(std::slice::from_ref(&heard)).unwrap();
    heard.track.uri = None;
    assert_eq!(s.library().unwrap().last(), Some(&heard));
    s.upsert_library(&[]).unwrap();
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
        ..Feedback::default()
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
        s.feedback(FeedbackKey::Work("unknown"), all_time.clone())
            .unwrap()
            .len(),
        0
    );

    // A song's feedback keys its artist and album.
    let song = Feedback {
        at: 250,
        work_key: Some("nina marsh quartet|blue hours".into()),
        artist_key: Some("nina marsh quartet".into()),
        album_key: Some("spotify:album:blue".into()),
        signal: 3,
        ..Feedback::default()
    };
    s.record_feedback(&song).unwrap();
    assert_eq!(
        s.feedback(FeedbackKey::Artist("nina marsh quartet"), all_time.clone())
            .unwrap(),
        std::slice::from_ref(&song)
    );
    assert_eq!(
        s.feedback(FeedbackKey::Album("spotify:album:blue"), all_time.clone())
            .unwrap(),
        std::slice::from_ref(&song)
    );
    assert!(
        s.feedback(
            FeedbackKey::Composer("nina marsh quartet"),
            all_time.clone()
        )
        .unwrap()
        .is_empty()
    );

    // Everything in a window, whatever it is about; same ordering and bounds.
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
    assert_eq!(
        s.feedback_between(all_time).unwrap(),
        [skip, like.clone(), song.clone(), late.clone(), tie.clone()]
    );
    assert_eq!(s.feedback_between(150..300).unwrap(), [like, song]);
    assert_eq!(s.feedback_between(300..301).unwrap(), [late, tie]);
    assert_eq!(s.feedback_between(301..400).unwrap(), [] as [Feedback; 0]);
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

fn act(at: i64, client: &str, before: Option<&str>, undo_of: Option<i64>) -> Action {
    Action {
        at,
        client: client.into(),
        surface: "mcp".into(),
        intent: format!("set_volume Kitchen at {at}"),
        decision: if before.is_some() {
            "allow"
        } else {
            "deny: capped"
        }
        .into(),
        result: "done".into(),
        before_state: before.map(str::to_string),
        undo_of,
    }
}

fn ids(actions: &[fsonos_core::store::LoggedAction]) -> Vec<i64> {
    actions.iter().map(|a| a.id).collect()
}

fn recent(s: &dyn Store, client: Option<&str>, since: Option<i64>, limit: usize) -> Vec<i64> {
    ids(&s
        .recent_actions(&ActionFilter {
            client: client.map(str::to_string),
            since,
            limit,
        })
        .unwrap())
}

fn undoable(s: &dyn Store, client: Option<&str>) -> Option<i64> {
    s.last_undoable_action(client).unwrap().map(|a| a.id)
}

fn action_log(s: &mut dyn Store) {
    assert_eq!(undoable(s, None), None);
    let a1 = s
        .record_action(&act(100, "tag:agent", Some("[s1]"), None))
        .unwrap();
    let a2 = s
        .record_action(&act(110, "cli", Some("[s2]"), None))
        .unwrap();
    // A denial is logged but has nothing to undo.
    let denied = s.record_action(&act(120, "tag:agent", None, None)).unwrap();
    assert!(a1 < a2 && a2 < denied);

    assert_eq!(recent(s, None, None, 0), [denied, a2, a1]);
    assert_eq!(recent(s, Some("tag:agent"), None, 0), [denied, a1]);
    assert_eq!(recent(s, None, Some(110), 0), [denied, a2]);
    assert_eq!(recent(s, None, None, 1), [denied]);
    assert_eq!(recent(s, Some("nobody"), None, 0), Vec::<i64>::new());

    assert_eq!(undoable(s, None), Some(a2));
    assert_eq!(undoable(s, Some("tag:agent")), Some(a1));
    assert_eq!(undoable(s, Some("nobody")), None);

    // Undoing a2 makes a1 the next undoable; undos are never undoable.
    let u2 = s
        .record_action(&act(130, "cli", Some("[s3]"), Some(a2)))
        .unwrap();
    assert_eq!(undoable(s, None), Some(a1));
    let u1 = s
        .record_action(&act(140, "tag:agent", Some("[s4]"), Some(a1)))
        .unwrap();
    assert_eq!(undoable(s, None), None);

    let newest = s
        .recent_actions(&ActionFilter {
            limit: 1,
            ..ActionFilter::default()
        })
        .unwrap();
    assert_eq!(newest[0].id, u1);
    assert_eq!(
        newest[0].action,
        act(140, "tag:agent", Some("[s4]"), Some(a1))
    );

    // Retention: older than 115 goes (a1, a2), then all but the newest 2.
    assert_eq!(s.prune_actions(3, 115).unwrap(), 2);
    assert_eq!(recent(s, None, None, 0), [u1, u2, denied]);
    assert_eq!(s.prune_actions(2, 0).unwrap(), 1);
    assert_eq!(recent(s, None, None, 0), [u1, u2]);
    assert_eq!(s.prune_actions(2, 0).unwrap(), 0);
    // Ids keep ascending past pruned rows.
    let next = s
        .record_action(&act(150, "cli", Some("[s5]"), None))
        .unwrap();
    assert!(next > u1);
}

fn scene(name: &str, spec: &str, updated: i64) -> StoredScene {
    StoredScene {
        name: name.into(),
        spec: spec.into(),
        updated,
    }
}

fn scenes(s: &mut dyn Store) {
    assert_eq!(s.scenes().unwrap(), []);
    assert_eq!(s.scene("Evening").unwrap(), None);
    s.save_scene(&scene("Evening", "{\"v\":1}", 100)).unwrap();
    s.save_scene(&scene("Dinner", "{}", 50)).unwrap();
    // Saving again replaces; names are exact.
    s.save_scene(&scene("Evening", "{\"v\":2}", 200)).unwrap();
    assert_eq!(s.scene("evening").unwrap(), None);
    assert_eq!(
        s.scene("Evening").unwrap(),
        Some(scene("Evening", "{\"v\":2}", 200))
    );
    let names: Vec<String> = s.scenes().unwrap().into_iter().map(|x| x.name).collect();
    assert_eq!(names, ["Dinner", "Evening"]);
    assert!(s.delete_scene("Dinner").unwrap());
    assert!(!s.delete_scene("Dinner").unwrap());
    assert_eq!(s.scenes().unwrap(), [scene("Evening", "{\"v\":2}", 200)]);
}

fn plan(spec: &str, creator: &str, created: i64) -> StoredSchedule {
    StoredSchedule {
        id: 0,
        spec: spec.into(),
        action: "{\"kind\":\"volume\",\"room\":\"Den\",\"level\":20}".into(),
        creator: creator.into(),
        enabled: true,
        created,
        last_fired: None,
    }
}

fn schedules(s: &mut dyn Store) {
    assert_eq!(s.schedules().unwrap(), []);
    let a = s.add_schedule(&plan("daily 07:30", "cli", 100)).unwrap();
    let b = s
        .add_schedule(&plan("weekends 09:00", "tag:assistant", 200))
        .unwrap();
    assert!(b > a);
    let all = s.schedules().unwrap();
    assert_eq!(all.iter().map(|x| x.id).collect::<Vec<_>>(), [a, b]);
    assert_eq!(
        all[1],
        StoredSchedule {
            id: b,
            ..plan("weekends 09:00", "tag:assistant", 200)
        }
    );
    // A run is claimed once; an older or repeated one is refused.
    assert!(s.mark_schedule_fired(a, 1_000).unwrap());
    assert!(!s.mark_schedule_fired(a, 1_000).unwrap());
    assert!(!s.mark_schedule_fired(a, 900).unwrap());
    assert!(s.mark_schedule_fired(a, 1_100).unwrap());
    assert!(!s.mark_schedule_fired(9_999, 1).unwrap());
    assert!(s.set_schedule_enabled(b, false).unwrap());
    assert!(!s.set_schedule_enabled(9_999, false).unwrap());
    let all = s.schedules().unwrap();
    assert_eq!((all[0].last_fired, all[1].enabled), (Some(1_100), false));
    assert!(s.delete_schedule(a).unwrap());
    assert!(!s.delete_schedule(a).unwrap());
    assert_eq!(s.schedules().unwrap().len(), 1);
}

fn artist_genres(s: &mut dyn Store) {
    assert_eq!(s.artist_genres("FakeArtist01").unwrap(), None);
    let tags = vec!["cool jazz".to_string(), "hard bop".to_string()];
    s.save_artist_genres("FakeArtist01", &tags, 1_000).unwrap();
    // No tags is an answer too, so the artist isn't read again.
    s.save_artist_genres("FakeArtist02", &[], 1_001).unwrap();
    assert_eq!(
        s.artist_genres("FakeArtist01").unwrap(),
        Some((tags.clone(), 1_000))
    );
    assert_eq!(
        s.artist_genres("FakeArtist02").unwrap(),
        Some((Vec::new(), 1_001))
    );
    // A re-read replaces the tags and the time.
    s.save_artist_genres("FakeArtist01", &tags[..1], 2_000)
        .unwrap();
    assert_eq!(
        s.artist_genres("FakeArtist01").unwrap(),
        Some((tags[..1].to_vec(), 2_000))
    );
}

fn album_misses(s: &mut dyn Store) {
    assert!(s.album_misses().unwrap().is_empty());
    let miss = |uri: &str, at: i64| (uri.to_owned(), at);
    s.save_album_misses(&[
        miss("spotify:album:gone", 5_000),
        miss("spotify:album:empty", 6_000),
        miss("spotify:album:gone", 7_000),
    ])
    .unwrap();
    assert_eq!(
        s.album_misses().unwrap(),
        [
            miss("spotify:album:empty", 6_000),
            miss("spotify:album:gone", 7_000)
        ],
        "by URI, a repeat keeping the last time"
    );
    // A save replaces the lot.
    s.save_album_misses(&[miss("spotify:album:later", 9_000)])
        .unwrap();
    assert_eq!(
        s.album_misses().unwrap(),
        [miss("spotify:album:later", 9_000)]
    );
    s.save_album_misses(&[]).unwrap();
    assert!(s.album_misses().unwrap().is_empty());
}

fn suite(s: &mut dyn Store) {
    play_history(s);
    inventory_cache(s);
    library_cache(s);
    render_params_and_auth(s);
    dj_sessions(s);
    feedback(s);
    album_tracks(s);
    artist_genres(s);
    album_misses(s);
    action_log(s);
    scenes(s);
    schedules(s);
}

#[test]
fn mem_store_conforms() {
    suite(&mut MemStore::default());
}

#[test]
fn sqlite_in_memory_conforms() {
    let mut s = SqliteStore::open_in_memory().unwrap();
    suite(&mut s);
    assert_eq!(s.schema_versions().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
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
    assert_eq!(s.schema_versions().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    assert_eq!(s.recent_plays(None, 10).unwrap().len(), 5);
    assert_eq!(s.cached_players().unwrap().len(), 3);
    assert_eq!(s.cached_groups("HH_S2").unwrap().len(), 1);
    assert_eq!(s.library().unwrap().len(), 5);
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
    let library = s.library().unwrap();
    let movement = library
        .iter()
        .find(|e| e.track.source_uri == "spotify:track:m2")
        .unwrap();
    assert_eq!(movement.work_key.as_deref(), Some("mahler|symphony no 5"));
    assert_eq!(recent(&s, None, None, 0).len(), 3);
    assert!(s.last_undoable_action(None).unwrap().is_some());

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
