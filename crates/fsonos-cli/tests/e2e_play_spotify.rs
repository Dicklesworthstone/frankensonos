//! Spotify albums and playlists play, against the virtual households: the
//! CLI takes an open.spotify.com album link and MCP a playlist URI, and the
//! sim confirms each replaced the queue with its own tracks and plays from
//! the first. An artist says it can't play yet, and what plays plays on.

mod e2e;

use e2e::Scenario;
use fsonos_proto::control::{get_position_info, get_transport_info};
use fsonos_sim::SimHousehold;
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
        (state, track, position.map(|p| p.uri).unwrap_or_default())
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

    let artist = format!("spotify:artist:{ALBUM}");
    let run = s.cli("play-artist", &["play", "Kitchen", &artist]);
    let (_, _, uri) = now(&s);
    s.check(
        "play-artist",
        "cli",
        "an artist says it can't play yet, and the playlist plays on",
        !run.ok()
            && run.stderr.contains("error[NOT_IMPLEMENTED]")
            && uri.contains(&format!("{PLAYLIST}1")),
        &run.stderr,
    );
    s.finish();
}
