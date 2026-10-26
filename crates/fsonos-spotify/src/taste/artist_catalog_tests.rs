//! Artist discovery uses the production Session and a real loopback server.

use super::*;
use crate::cache::{sync_library_detailed, works_from_store};
use crate::client::Artist;
use crate::taste::ARTIST_PM;
use fsonos_core::store::LibraryOrigin;

const ARTIST: &str = "CatalogArtistA";
const NAME: &str = "Lantern Coast";
const ALBUM: &str = "CatalogAlbumA";

fn artist(id: &str, name: &str) -> Value {
    json!({ "id": id, "name": name, "genres": ["dream pop"] })
}

fn album(id: &str, artist_id: &str, artist_name: &str) -> Value {
    json!({
        "id": id, "uri": format!("spotify:album:{id}"),
        "name": "Lights Along the Shore", "album_type": "album",
        "artists": [{ "id": artist_id, "name": artist_name }],
        "release_date": "1994-05-17", "total_tracks": 6
    })
}

fn song(number: usize, artist_id: &str, artist_name: &str) -> Value {
    json!({
        "id": format!("CatalogTrack{number}"),
        "uri": format!("spotify:track:CatalogTrack{number}"),
        "name": format!("Song {number}"),
        "artists": [{ "id": artist_id, "name": artist_name }],
        "duration_ms": 210_000, "disc_number": 1, "track_number": number,
        "is_playable": true
    })
}

fn followed(server: &Server, artists: Vec<Value>) {
    server.set(
        "/v1/me/following",
        json!({ "artists": { "items": artists, "next": null } }),
    );
}

fn catalog(server: &Server, artist_id: &str, artist_name: &str, album_id: &str) {
    server.set(
        &format!("/v1/artists/{artist_id}/albums"),
        page(vec![album(album_id, artist_id, artist_name)], 0, None),
    );
    server.set(
        &format!("/v1/albums/{album_id}/tracks"),
        page(
            (1..=6).map(|n| song(n, artist_id, artist_name)).collect(),
            0,
            None,
        ),
    );
}

#[test]
fn unavailable_top_tracks_use_catalog_for_the_pass_with_full_metadata_and_taste() {
    for status in [403, 404] {
        let server = Server::new();
        let other = "CatalogArtistB";
        followed(
            &server,
            vec![artist(ARTIST, NAME), artist(other, "Dawn Harbor")],
        );
        // The same artist is followed and top: its catalog is read once.
        server.set(
            "/v1/me/top/artists",
            page(vec![artist(ARTIST, NAME)], 0, None),
        );
        server.set("/v1/me/top/tracks", page(Vec::new(), 0, None));
        catalog(&server, ARTIST, NAME, ALBUM);
        catalog(&server, other, "Dawn Harbor", "CatalogAlbumB");
        server
            .state
            .lock()
            .unwrap()
            .refused
            .insert(format!("/v1/artists/{ARTIST}/top-tracks"), status);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, "user-follow-read user-top-read");
            let (taste, read) =
                read_taste(&mut session, &cx, &mut MemStore::default(), &[], 0).await;
            assert!(!read.incomplete(), "{read:?}");
            assert_eq!(read.artist_tracks, 6);
            assert!(read.artist_tracks_unread.is_none());
            assert!(taste.artists_unread.is_empty());
            let mut items = taste.tracks.clone();
            taste.weigh(&mut items);
            let first = &items[0];
            assert_eq!(first.source_uri, "spotify:track:CatalogTrack1");
            assert_eq!(first.artists, [NAME]);
            assert_eq!(first.artist_id.as_deref(), Some(ARTIST));
            assert_eq!(first.album.as_deref(), Some("Lights Along the Shore"));
            assert_eq!(
                first.album_uri.as_deref(),
                Some("spotify:album:CatalogAlbumA")
            );
            assert_eq!(first.album_artists, [NAME]);
            assert_eq!((first.disc_number, first.track_number), (Some(1), Some(1)));
            assert_eq!(first.release_year, Some(1994));
            assert_eq!(first.duration_secs, Some(210));
            assert_eq!(first.origin, Origin::Taste);
            assert_eq!(first.taste_pm, Some(ARTIST_PM * ARTIST_PM / 1000));
        });
        let artist_calls = server.calls("/v1/artists/");
        assert_eq!(artist_calls.len(), 3, "{artist_calls:?}");
        assert_eq!(server.calls("/v1/albums/").len(), 2);
        assert!(
            artist_calls
                .iter()
                .filter(|call| path(call).ends_with("/albums"))
                .all(|call| call.contains("include_groups=album,single")
                    && call.contains("limit=10"))
        );
    }
}

#[test]
fn catalog_sampling_deduplicates_and_skips_unplayable_local_and_other_artists_tracks() {
    let server = Server::new();
    followed(&server, vec![artist(ARTIST, NAME)]);
    catalog(&server, ARTIST, NAME, ALBUM);
    let mut unavailable = song(1, ARTIST, NAME);
    unavailable["is_playable"] = false.into();
    let mut local = song(2, ARTIST, NAME);
    local["is_local"] = true.into();
    let mut explicit = song(3, ARTIST, NAME);
    explicit["explicit"] = true.into();
    server.set(
        &format!("/v1/albums/{ALBUM}/tracks"),
        page(
            vec![
                unavailable,
                local,
                song(9, "SomeoneElse", "Another Artist"),
                explicit.clone(),
                explicit,
                song(4, ARTIST, NAME),
                song(5, ARTIST, NAME),
                song(6, ARTIST, NAME),
            ],
            0,
            Some("https://example.invalid/do-not-follow".into()),
        ),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, "user-follow-read");
        let (taste, read) = read_taste(&mut session, &cx, &mut MemStore::default(), &[], 0).await;
        assert_eq!(read.artist_tracks, 3);
        assert!(
            taste.tracks[0].explicit,
            "preferences decide explicit music later"
        );
        assert_eq!(
            taste
                .tracks
                .iter()
                .map(|track| track.track_number)
                .collect::<Vec<_>>(),
            [Some(3), Some(4), Some(5)]
        );
    });
    assert_eq!(server.calls("/v1/albums/").len(), 1);
}

#[test]
fn catalog_reads_at_most_three_unique_credited_albums_without_following_pages() {
    let server = Server::new();
    followed(&server, vec![artist(ARTIST, NAME)]);
    let mut wrong_uri = album("WrongUri", ARTIST, NAME);
    wrong_uri["uri"] = "spotify:album:SomeoneElsesAlbum".into();
    let mut compilation = album("Compilation", ARTIST, NAME);
    compilation["album_type"] = "compilation".into();
    let mut albums = vec![
        album("Uncredited", "OtherArtist", "Other"),
        wrong_uri,
        compilation,
    ];
    for number in 1..=4 {
        let id = format!("BoundedAlbum{number}");
        albums.push(album(&id, ARTIST, NAME));
        if number == 1 {
            albums.push(album(&id, ARTIST, NAME));
        }
        let mut track = song(number, ARTIST, NAME);
        track["is_playable"] = false.into();
        server.set(
            &format!("/v1/albums/{id}/tracks"),
            page(vec![track], 0, None),
        );
    }
    server.set(
        &format!("/v1/artists/{ARTIST}/albums"),
        page(
            albums,
            0,
            Some(format!("{API}/artists/{ARTIST}/albums?offset=10")),
        ),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, "user-follow-read");
        let (taste, read) = read_taste(&mut session, &cx, &mut MemStore::default(), &[], 0).await;
        assert!(taste.tracks.is_empty());
        assert_eq!(taste.artists_unread, [NAME.to_lowercase()]);
        assert!(read.artist_tracks_unread.is_some());
    });
    assert_eq!(server.calls("/v1/artists/").len(), 2);
    assert_eq!(server.calls("/v1/albums/").len(), 3);
    assert!(server.calls("/v1/albums/BoundedAlbum4").is_empty());
}

#[test]
fn top_tracks_stay_preferred_when_available_and_fill_three_playable_distinct_slots() {
    let server = Server::new();
    let mut unavailable = track(0);
    unavailable["is_playable"] = false.into();
    server.set(
        &format!("/v1/artists/{ARTIST}/top-tracks"),
        json!({ "tracks": [unavailable, track(1), track(1), track(2), track(3), track(4)] }),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, "user-follow-read");
        let mut read = TasteRead::default();
        let artist: Artist = serde_json::from_value(artist(ARTIST, NAME)).unwrap();
        let mut reader = Reader {
            session: &mut session,
            cx: &cx,
            read: &mut read,
        };
        let (tracks, unread) = reader.artist_tracks(&[&artist]).await;
        assert_eq!(tracks.len(), 3);
        assert!(unread.is_empty());
        assert_eq!(tracks[2].source_uri, "spotify:track:TestTrack3");
        assert!(read.artist_tracks_unread.is_none());
    });
    assert_eq!(server.calls("/v1/").len(), 1);
}

#[test]
fn one_missing_artist_does_not_starve_later_followed_artists() {
    let server = Server::new();
    followed(
        &server,
        vec![
            artist("MissingArtist", "Missing Artist"),
            artist(ARTIST, NAME),
        ],
    );
    catalog(&server, ARTIST, NAME, ALBUM);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, "user-follow-read");
        let (taste, read) = read_taste(&mut session, &cx, &mut MemStore::default(), &[], 0).await;
        assert_eq!(read.artist_tracks, 3);
        assert_eq!(taste.artists_unread, ["missing artist"]);
        assert!(read.artist_tracks_unread.is_some());
        assert!(
            taste
                .tracks
                .iter()
                .all(|track| track.artist_id.as_deref() == Some(ARTIST))
        );
    });
    assert!(
        server
            .calls("/v1/artists/")
            .iter()
            .any(|call| call.contains(&format!("/{ARTIST}/albums")))
    );
}

#[test]
fn failed_catalog_refreshes_retain_previous_artist_music_and_rate_limits_stop_requests() {
    for status in [403, 404, 429, 503] {
        let server = Server::new();
        followed(&server, vec![artist(ARTIST, NAME)]);
        catalog(&server, ARTIST, NAME, ALBUM);
        let lead = "CatalogCollaborator";
        let lead_name = "Harbor Guest";
        server.set(&format!("/v1/artists/{lead}"), artist(lead, lead_name));
        let mut tracks: Vec<Value> = (1..=6).map(|n| song(n, ARTIST, NAME)).collect();
        tracks[0]["artists"] = json!([
            { "id": lead, "name": lead_name },
            { "id": ARTIST, "name": NAME }
        ]);
        server.set(&format!("/v1/albums/{ALBUM}/tracks"), page(tracks, 0, None));
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = MemStore::default();
            let mut session = server.session(&cx, "user-follow-read");
            sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            let before = store.library().unwrap();
            assert_eq!(before.len(), 3);
            for entry in &before {
                let credits = crate::library::split_artists(entry.track.artist.as_deref().unwrap());
                if entry.track.source_uri == "spotify:track:CatalogTrack1" {
                    assert_eq!(credits, [lead_name, NAME], "preserve the lead and guest");
                } else {
                    assert_eq!(credits, [NAME], "single-artist music also stays");
                }
            }
            assert!(
                before
                    .iter()
                    .all(|entry| entry.origin == LibraryOrigin::Taste)
            );
            let calls = server.calls("/v1/").len();
            server
                .state
                .lock()
                .unwrap()
                .refused
                .insert(format!("/v1/albums/{ALBUM}/tracks"), status);
            let mut session = server.session(&cx, "user-follow-read");
            let result = sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            assert_eq!(store.library().unwrap(), before, "status {status}");
            assert_eq!(result.library.retired, 0);
            assert!(!result.notes.is_empty());
            if status == 429 {
                assert!(result.playlists.retry_after_secs > 0);
                let last = server.calls("/v1/").last().unwrap().clone();
                assert!(path(&last).ends_with("/tracks"));
                let after = server.calls("/v1/").len();
                assert_eq!(after - calls, 6, "no request after the rate limit");
            }
        });
    }
}

#[test]
fn a_catalog_movement_enters_the_normal_whole_work_completion_path() {
    let server = Server::new();
    let name = "Ludwig van Beethoven";
    let mut composer = artist(ARTIST, name);
    composer["genres"] = json!(["classical"]);
    followed(&server, vec![composer]);
    let mut recording = album(ALBUM, ARTIST, name);
    recording["name"] = "Beethoven: Symphony No. 5".into();
    recording["total_tracks"] = 4.into();
    server.set(
        &format!("/v1/artists/{ARTIST}/albums"),
        page(vec![recording], 0, None),
    );
    let movements = [
        "I. Allegro con brio",
        "II. Andante con moto",
        "III. Allegro",
        "IV. Allegro",
    ];
    let tracks = movements
        .iter()
        .enumerate()
        .map(|(index, movement)| {
            let mut track = song(index + 1, ARTIST, name);
            track["name"] = format!("Symphony No. 5 in C Minor, Op. 67: {movement}").into();
            track
        })
        .collect::<Vec<_>>();
    server.set(
        &format!("/v1/albums/{ALBUM}/tracks"),
        json!({ "items": tracks, "offset": 0, "total": 4, "next": null }),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, "user-follow-read");
        let mut store = MemStore::default();
        let result = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(
            result.library.tracks, 3,
            "the catalog contribution is bounded"
        );
        let works = works_from_store(&store).unwrap();
        assert_eq!(works.works().len(), 1);
        let work = &works.works()[0];
        assert_eq!(
            work.movements.len(),
            4,
            "all movements of the recording play together"
        );
        assert_eq!(
            work.movements
                .iter()
                .map(|track| track.track_number)
                .collect::<Vec<_>>(),
            [Some(1), Some(2), Some(3), Some(4)]
        );
        assert!(work.movements[3].expanded);
    });
}
