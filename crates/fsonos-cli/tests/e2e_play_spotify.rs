//! Spotify albums and playlists play, against the virtual households: the
//! CLI takes an open.spotify.com album link and MCP a playlist URI, and the
//! sim confirms each replaced the queue with its own tracks and plays from
//! the first. An artist says it can't play yet, and what plays plays on.
//! A library search that names a saved album plays the whole album, and
//! one that names a playlist in the owner's list plays the playlist.

mod e2e;

use e2e::Scenario;
use fsonos_core::store::{SqliteStore, Store};
use fsonos_proto::control::{get_position_info, get_transport_info};
use fsonos_sim::SimHousehold;
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::{LibraryItem, Origin};
use fsonos_types::TransportState;
use serde_json::json;

const ALBUM: &str = "0SimAlbumForPlayback01";
const PLAYLIST: &str = "0SimPlaylistPlayback01";

#[test]
fn spotify_albums_and_playlists_play_from_their_first_track() {
    let mut s = Scenario::start("play-spotify");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    // The sim's queue names a container's tracks `<container id><n>`.
    let now = |s: &Scenario| {
        let state = get_transport_info(&s.lan(), kitchen).map(|t| t.state).ok();
        let position = get_position_info(&s.lan(), kitchen).ok();
        let track = position.as_ref().map(|p| p.track);
        (state, track, position.map_or_default(|p| p.uri))
    };

    let link = format!("https://open.spotify.com/album/{ALBUM}?si=share");
    let run = s.cli("play-album", &["play", "Kitchen", &link]);
    s.check(
        "play-album",
        "cli",
        "an album link exits 0",
        run.ok(),
        &run.stderr,
    );
    let (state, track, uri) = now(&s);
    s.check(
        "play-album",
        "sim",
        "the album's tracks replaced the queue and play from the first",
        state == Some(TransportState::Playing)
            && track == Some(1)
            && uri.contains(&format!("{ALBUM}1")),
        format!("{state:?}, track {track:?}, {uri}"),
    );

    let run = s.cli("next", &["next", "Kitchen"]);
    s.check("next", "cli", "exits 0", run.ok(), &run.stderr);
    let (_, track, uri) = now(&s);
    s.check(
        "next",
        "sim",
        "the queue moves on through the album",
        track == Some(2) && uri.contains(&format!("{ALBUM}2")),
        format!("track {track:?}, {uri}"),
    );

    let mut mcp = s.mcp();
    mcp.initialize();
    let played = mcp.request(
        "tools/call",
        &json!({
            "name": "play",
            "arguments": { "zone": "Kitchen", "source_uri": format!("spotify:playlist:{PLAYLIST}") },
        }),
    );
    s.check(
        "play-playlist",
        "mcp",
        "a playlist URI plays",
        played["result"].is_object() && played["result"]["isError"] != true,
        &played,
    );
    let (state, track, uri) = now(&s);
    s.check(
        "play-playlist",
        "sim",
        "the playlist replaced the album and plays from its first track",
        state == Some(TransportState::Playing)
            && track == Some(1)
            && uri.contains(&format!("{PLAYLIST}1")),
        format!("{state:?}, track {track:?}, {uri}"),
    );

    // An audiobook is a Spotify kind the house still cannot play; the bare
    // kinds have their own test below.
    let audiobook = format!("spotify:audiobook:{ALBUM}");
    let run = s.cli("play-audiobook", &["play", "Kitchen", &audiobook]);
    let (_, _, uri) = now(&s);
    s.check(
        "play-audiobook",
        "cli",
        "an audiobook is refused before any speaker changes, and the playlist plays on",
        !run.ok()
            && run.stderr.contains("error[INVALID_ARGUMENT]")
            && uri.contains(&format!("{PLAYLIST}1")),
        &run.stderr,
    );

    s.finish();
}

const SAVED: &str = "0SimHarborLightsAlbum1";
const SUNDAY: &str = "0SimSundayPlaylist0001";

/// Two tracks of an album the owner saved, as a library sync leaves them.
fn saved_album() -> Vec<LibraryItem> {
    ["Low Tide", "Every Window"]
        .iter()
        .zip(1u32..)
        .map(|(title, n)| LibraryItem {
            source_uri: format!("spotify:track:0SimHarborLightsTrack{n}"),
            title: (*title).to_owned(),
            artists: vec!["The Sim Lanterns".into()],
            album: Some("Harbor Lights".into()),
            album_uri: Some(format!("spotify:album:{SAVED}")),
            album_artists: vec!["The Sim Lanterns".into()],
            track_number: Some(n),
            duration_secs: Some(200),
            origin: Origin::SavedAlbum,
            ..LibraryItem::default()
        })
        .collect()
}

#[test]
fn a_search_naming_a_saved_album_or_a_playlist_plays_it() {
    let mut s = Scenario::start("play-saved-album");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let seeded = SqliteStore::open(&s.dir().join("data").join("fsonos.db"))
        .map_err(|e| e.to_string())
        .and_then(|mut store| {
            apply_library_read(&mut store, &saved_album()).map_err(|e| e.to_string())?;
            let playlists = [(
                format!("spotify:playlist:{SUNDAY}"),
                "Slow Sunday".to_owned(),
            )];
            store.save_playlists(&playlists).map_err(|e| e.to_string())
        });
    s.check(
        "library",
        "store",
        "a synced library with one saved album and one playlist",
        seeded.is_ok(),
        format!("{seeded:?}"),
    );

    let run = s.cli(
        "pick",
        &["play", "Kitchen", "--search", "harbor lights", "--pick"],
    );
    s.check(
        "pick",
        "cli",
        "the album is the first match, then its tracks",
        run.ok()
            && run
                .stdout
                .starts_with("1. Harbor Lights (The Sim Lanterns) [album]"),
        &run.stdout,
    );
    let run = s.cli("play", &["play", "Kitchen", "--search", "harbor lights"]);
    let state = get_transport_info(&s.lan(), kitchen).map(|t| t.state).ok();
    let position = get_position_info(&s.lan(), kitchen).ok();
    let (track, uri) = position.map_or((None, String::new()), |p| (Some(p.track), p.uri));
    s.check(
        "play",
        "sim",
        "the whole album replaced the queue and plays from its first track",
        run.ok()
            && state == Some(TransportState::Playing)
            && track == Some(1)
            && uri.contains(&format!("{SAVED}1")),
        format!("{}; {state:?}, track {track:?}, {uri}", run.stderr),
    );
    let run = s.cli("playlist", &["play", "Kitchen", "--search", "slow sunday"]);
    let position = get_position_info(&s.lan(), kitchen).ok();
    let (track, uri) = position.map_or((None, String::new()), |p| (Some(p.track), p.uri));
    s.check(
        "playlist",
        "sim",
        "the playlist replaced the album and plays from its first track",
        run.ok() && track == Some(1) && uri.contains(&format!("{SUNDAY}1")),
        format!("{}; track {track:?}, {uri}", run.stderr),
    );
    s.finish();
}

#[test]
fn an_artist_or_show_is_refused_and_an_episode_plays() {
    let mut s = Scenario::start("play-spotify-kinds");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let now = |s: &Scenario| {
        let state = get_transport_info(&s.lan(), kitchen).map(|t| t.state).ok();
        let position = get_position_info(&s.lan(), kitchen).ok();
        (state, position.map_or_default(|p| p.uri))
    };
    let run = s.cli(
        "play-playlist",
        &["play", "Kitchen", &format!("spotify:playlist:{PLAYLIST}")],
    );
    s.check(
        "play-playlist",
        "cli",
        "a playlist plays",
        run.ok(),
        &run.stderr,
    );

    // An artist and a show are typed refusals with protocol evidence
    // (docs/PROTOCOL.md, "Bare Spotify kinds"); nothing reaches the speaker
    // and the playlist plays on.
    for (tag, prefix, says) in [
        (
            "play-artist",
            "spotify:artist:",
            "an artist is a typed refusal naming the SMAPI shortcut id",
        ),
        (
            "play-show",
            "spotify:show:",
            "a show is a typed refusal naming the service container id",
        ),
    ] {
        let uri_in = format!("{prefix}{ALBUM}");
        let run = s.cli(tag, &["play", "Kitchen", &uri_in]);
        let (_, uri) = now(&s);
        s.check(
            tag,
            "cli",
            says,
            !run.ok()
                && run.stderr.contains("error[NOT_IMPLEMENTED]")
                && uri.contains(&format!("{PLAYLIST}1")),
            &run.stderr,
        );
    }

    // An episode does play from the id alone.
    let episode = format!("spotify:episode:{ALBUM}");
    let run = s.cli("play-episode", &["play", "Kitchen", &episode]);
    s.check(
        "play-episode",
        "cli",
        "an episode exits 0",
        run.ok(),
        &run.stderr,
    );
    let (state, uri) = now(&s);
    s.check(
        "play-episode",
        "sim",
        "the episode renders on the account id and plays",
        state == Some(TransportState::Playing) && uri.contains(&format!("episode%3a{ALBUM}")),
        format!("{state:?}, {uri}"),
    );

    let mut mcp = s.mcp();
    mcp.initialize();
    let refused = mcp.request(
        "tools/call",
        &json!({
            "name": "play",
            "arguments": { "zone": "Kitchen", "source_uri": format!("spotify:artist:{ALBUM}") },
        }),
    );
    s.check(
        "play-artist-mcp",
        "mcp",
        "MCP answers the same typed refusal as the CLI",
        refused["result"]["isError"] == true
            && refused["result"].to_string().contains("NOT_IMPLEMENTED"),
        &refused,
    );
    s.finish();
}
