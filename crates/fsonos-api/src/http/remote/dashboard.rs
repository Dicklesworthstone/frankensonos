//! The Listening Room dashboard is a separate, embedded client of the same API.
//! It inherits the remote's browser security headers and listener guards.

use fastapi::Response;
use fastapi::core::RouteEntry;

use super::{Ctx, Op, REMOTE, asset};

const PAGE: &str = include_str!("../../remote/dashboard.html");
const STYLE: &str = include_str!("../../remote/dashboard.css");
const LIBRARY: &str = include_str!("../../remote/dashboard-library.js");
const PLAYLIST: &str = include_str!("../../remote/playlist-contents.js");
const QUEUE_TRACKS: &str = include_str!("../../remote/queue-tracks.js");
const LIBRARY_PANEL: &str = include_str!("../../remote/dashboard-library-panel.js");
const LIBRARY_STYLE: &str = include_str!("../../remote/dashboard-library.css");
const EVENTS: &str = include_str!("../../remote/event-feed.js");
const LIVE: &str = include_str!("../../remote/dashboard-live.js");
const CORE: &str = include_str!("../../remote/dashboard-core.js");
const SEEK: &str = include_str!("../../remote/dashboard-seek.js");
const VISUALIZER: &str = include_str!("../../remote/dashboard-visualizer.js");
const AUDIO: &str = include_str!("../../remote/dashboard-audio.js");
const SESSION: &str = include_str!("../../remote/dashboard-session.js");
const DJ_DIRECTION: &str = include_str!("../../remote/dj-direction.js");
const DJ_CONFIGURATION: &str = include_str!("../../remote/dj-configuration.js");
const DJ_CONFIGURATION_PANEL: &str = include_str!("../../remote/dj-configuration-panel.js");
const DJ_CONFIGURATION_STYLE: &str = include_str!("../../remote/dj-configuration.css");
const SCENE_QUEUE: &str = include_str!("../../remote/scene-queue.js");
const QUEUE_STYLE: &str = include_str!("../../remote/dashboard-queue.css");
const QUEUE: &str = include_str!("../../remote/dashboard-queue.js");
const ACTIVITY: &str = include_str!("../../remote/dashboard-activity.js");
const ACTIVITY_PANEL: &str = include_str!("../../remote/dashboard-activity-panel.js");
const ACTIVITY_STYLE: &str = include_str!("../../remote/dashboard-activity.css");
const WORKSPACE: &str = include_str!("../../remote/dashboard-workspace.js");
const PLAY_MODE: &str = include_str!("../../remote/dashboard-play-mode.js");
const GROUPS: &str = include_str!("../../remote/dashboard-groups.js");
const TIMERS: &str = include_str!("../../remote/timers.js");
const SLEEP: &str = include_str!("../../remote/dashboard-sleep.js");
const VOLUME: &str = include_str!("../../remote/dashboard-volume.js");
const SCENES: &str = include_str!("../../remote/dashboard-scenes.js");
const SCHEDULES: &str = include_str!("../../remote/schedules.js");
const ROUTINES: &str = include_str!("../../remote/dashboard-routines.js");
const ANNOUNCEMENTS: &str = include_str!("../../remote/announcements.js");
const ANNOUNCEMENTS_PANEL: &str = include_str!("../../remote/dashboard-announcements.js");
const ANNOUNCEMENTS_STYLE: &str = include_str!("../../remote/dashboard-announcements.css");
const STEERING: &str = include_str!("../../remote/dj-steering.js");
const DIRECTION: &str = include_str!("../../remote/dashboard-direction.js");
const DIRECTION_STYLE: &str = include_str!("../../remote/dashboard-direction.css");
const DJ_PREFERENCES: &str = include_str!("../../remote/dj-preferences.js");
const PREFERENCES: &str = include_str!("../../remote/dashboard-preferences.js");
const PREFERENCES_STYLE: &str = include_str!("../../remote/dashboard-preferences.css");
const DJ_SESSION: &str = include_str!("../../remote/dj-session.js");
const REHEARSAL: &str = include_str!("../../remote/dashboard-rehearsal.js");
const REHEARSAL_STYLE: &str = include_str!("../../remote/dashboard-rehearsal.css");
const SOURCE_PLAYER: &str = include_str!("../../remote/source-player.js");
const SOURCE: &str = include_str!("../../remote/dashboard-source.js");
const SOURCE_STYLE: &str = include_str!("../../remote/dashboard-source.css");
const PLAYLIST_SAVE: &str = include_str!("../../remote/dashboard-playlist-save.js");
const PLAYLIST_SAVE_PANEL: &str = include_str!("../../remote/dashboard-playlist-save-panel.js");
const PLAYLIST_SAVE_STYLE: &str = include_str!("../../remote/dashboard-playlist-save.css");
const SPOTIFY_CONNECT: &str = include_str!("../../remote/spotify-connect.js");
const LIBRARY_SYNC: &str = include_str!("../../remote/library-sync.js");
const MUSIC: &str = include_str!("../../remote/dashboard-music.js");
const MUSIC_STYLE: &str = include_str!("../../remote/dashboard-music.css");
const SCRIPT: &str = include_str!("../../remote/dashboard.js");

fn document(content_type: &str, text: &str) -> Response {
    asset(content_type, text).header(
        "permissions-policy",
        "camera=(), geolocation=(), microphone=(self)",
    )
}

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/dashboard",
                "web_dashboard",
                REMOTE,
                "Listening Room: live household dashboard and local visualization",
            ),
            |_, _, _| document("text/html; charset=utf-8", PAGE),
        )
        .response_schema::<String>(200, "The listening dashboard (text/html)"),
        cx.route(
            &Op::get(
                "/dashboard.css",
                "web_dashboard_style",
                REMOTE,
                "The listening dashboard's style sheet",
            ),
            |_, _, _| {
                document(
                    "text/css; charset=utf-8",
                    &format!(
                        "{STYLE}\n{LIBRARY_STYLE}\n{QUEUE_STYLE}\n{ACTIVITY_STYLE}\n{ANNOUNCEMENTS_STYLE}\n{DIRECTION_STYLE}\n{PREFERENCES_STYLE}\n{DJ_CONFIGURATION_STYLE}\n{REHEARSAL_STYLE}\n{SOURCE_STYLE}\n{MUSIC_STYLE}\n{PLAYLIST_SAVE_STYLE}"
                    ),
                )
            },
        )
        .response_schema::<String>(200, "The dashboard style sheet (text/css)"),
        cx.route(
            &Op::get(
                "/dashboard.js",
                "web_dashboard_script",
                REMOTE,
                "The listening dashboard's self-contained script",
            ),
            |_, _, _| {
                document(
                    "text/javascript; charset=utf-8",
                    &format!(
                        "{EVENTS}\n{CORE}\n{SEEK}\n{LIVE}\n{VISUALIZER}\n{AUDIO}\n{SCENE_QUEUE}\n{DJ_DIRECTION}\n{DJ_CONFIGURATION}\n{DJ_CONFIGURATION_PANEL}\n{QUEUE_TRACKS}\n{PLAYLIST}\n{LIBRARY}\n{SESSION}\n{QUEUE}\n{LIBRARY_PANEL}\n{ACTIVITY}\n{ACTIVITY_PANEL}\n{WORKSPACE}\n{PLAY_MODE}\n{GROUPS}\n{TIMERS}\n{SLEEP}\n{VOLUME}\n{SCENES}\n{SCHEDULES}\n{ROUTINES}\n{ANNOUNCEMENTS}\n{ANNOUNCEMENTS_PANEL}\n{STEERING}\n{DIRECTION}\n{DJ_PREFERENCES}\n{PREFERENCES}\n{DJ_SESSION}\n{REHEARSAL}\n{SOURCE_PLAYER}\n{SOURCE}\n{PLAYLIST_SAVE}\n{PLAYLIST_SAVE_PANEL}\n{SPOTIFY_CONNECT}\n{LIBRARY_SYNC}\n{MUSIC}\n{SCRIPT}"
                    ),
                )
            },
        )
        .response_schema::<String>(200, "The dashboard script (text/javascript)"),
    ]
}
