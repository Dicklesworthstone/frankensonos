"""A stand-in daemon for the E2E harness (rows 20 to 25).

    stub-daemon.py PORT

* GET /events is refused with the POLICY_DENIED body the real daemon sent a LAN caller before `events` was added to
  its allow list (row 20: the app must stay connected and say why).
* Two rooms behave like speakers, so the app can be tested against the timing of real hardware (row 22):
  - "Lag Room" answers a resume the way a Sonos does, measured on a real one: the first read after the command still
    says `paused`, the next says `transitioning`, then `playing`. A pause takes effect at once.
  - "Stuck Room" accepts a resume and never starts playing, so the app must not claim it plays forever.
* The Spotify routes answer with the exact shapes of fsonos-api's spotify.rs (rows 23 to 25): status, a sync that
  runs for two reads, two albums, their tracks and liked tracks. POST /play and /dj/* are recorded and GET
  /_debug/posts returns what was posted, so a test can check what the app sent.
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

DENIED = {
    "detail": "unknown may not use events: unknown may only use: list_zones, play, pause",
    "code": "POLICY_DENIED",
    "hint": "The house policy forbids this; ask the owner to change it.",
    "suggestions": [],
    "retryable": False,
}
LOCK = threading.Lock()
# reads since the last resume; None when no resume is pending
PENDING = {"Lag Room": None, "Stuck Room": None}
TRANSPORT = {"Lag Room": "paused", "Stuck Room": "paused"}
POSTS = []
# Pause, resume and play-favorite bodies, kept apart from POSTS so row 25 still sees only what it sent.
COMMANDS = []
FAVORITES = [
    {"id": "FV:2/1", "title": "Jazz Mix", "kind": "station", "description": None, "art_uri": None},
    {"id": "FV:2/2", "title": "Jazz Classics", "kind": "station", "description": None, "art_uri": None},
    {"id": "FV:2/3", "title": "Morning News", "kind": "station", "description": None, "art_uri": None},
]
# Sync progress: None before a sync, else the number of status reads since it started.
SYNC = {"reads": None}
SIGNED = {"in": True}
# What the real daemon sends for a track: a relative /art URL for a speaker's art, an https URL passed through.
ART = {
    "Lag Room": "/speaker-art?player=RINCON_TEST&u=%2Fgetaa%3Fs%3D1%26u%3Dx",
    "Stuck Room": "https://cdn.example.invalid/x.jpg",
}

ALBUMS = [
    {"id": "a1", "title": "Bach: Goldberg Variations, BWV 988", "artist": "Glenn Gould", "year": 1981, "tracks": 3,
     "uri": "spotify:album:a1", "art_url": "https://cdn.example.invalid/bach.jpg"},
    {"id": "a2", "title": "Kind of Blue", "artist": "Miles Davis", "year": 1959, "tracks": 2,
     "uri": "spotify:album:a2", "art_url": None},
]
ALBUM_TRACKS = {
    "a1": [
        {"id": "t1", "title": "Aria", "artists": ["Glenn Gould"], "uri": "spotify:track:t1", "duration_secs": 183, "disc": 1, "number": 1},
        {"id": "t2", "title": "Variation 1", "artists": ["Glenn Gould"], "uri": "spotify:track:t2", "duration_secs": 62, "disc": 1, "number": 2},
        {"id": "t3", "title": "Variation 2", "artists": ["Glenn Gould"], "uri": "spotify:track:t3", "duration_secs": 51, "disc": 1, "number": 3},
    ],
    "a2": [
        {"id": "t4", "title": "So What", "artists": ["Miles Davis"], "uri": "spotify:track:t4", "duration_secs": 562, "disc": 1, "number": 1},
        {"id": "t5", "title": "Blue in Green", "artists": ["Miles Davis", "Bill Evans"], "uri": "spotify:track:t5", "duration_secs": 338, "disc": 1, "number": 2},
    ],
}
LIKED = [
    {"id": "t1", "title": "Aria", "artists": ["Glenn Gould"], "uri": "spotify:track:t1", "duration_secs": 183, "disc": 1,
     "number": 1, "album": "Bach: Goldberg Variations, BWV 988", "art_url": "https://cdn.example.invalid/bach.jpg"},
    {"id": "t5", "title": "Blue in Green", "artists": ["Miles Davis", "Bill Evans"], "uri": "spotify:track:t5",
     "duration_secs": 338, "disc": 1, "number": 2, "album": "Kind of Blue", "art_url": None},
]


def transport(room, count_read):
    with LOCK:
        pending = PENDING[room]
        if room == "Lag Room" and pending is not None:
            if count_read:
                PENDING[room] = pending + 1
            sequence = ["paused", "transitioning", "playing"]
            state = sequence[min(pending, 2)]
            if state == "playing":
                TRANSPORT[room] = "playing"
            return state
        return TRANSPORT[room]


def zone(room):
    return {"coordinator_room": room, "members": [room], "transport_state": transport(room, False), "household": "S2"}


def spotify_status():
    with LOCK:
        reads = SYNC["reads"]
        if reads is not None:
            SYNC["reads"] = reads + 1
    done = reads is not None and reads >= 2
    running = reads is not None and not done
    library = {"albums": 2, "tracks": 2, "synced_at": 1791500000} if done else {"albums": 0, "tracks": 0, "synced_at": None}
    signed_in = SIGNED["in"]
    return {
        "configured": True,
        "signed_in": signed_in,
        "client_id": "stub-client",
        "app_redirect_uri": "frankensonos://spotify-callback",
        "reauthorize": False,
        "library": library,
        "sync": {"running": running, "done": 5 if done else (1 if running else 0), "total": 5 if reads is not None else 0, "error": None},
    }


def page(items, query):
    needle = (query.get("q") or [""])[0].lower()
    offset = int((query.get("offset") or ["0"])[0])
    limit = int((query.get("limit") or ["50"])[0])
    hits = [i for i in items if not needle or needle in json.dumps(i).lower()]
    return {"total": len(hits), "items": hits[offset:offset + limit]}


class Handler(BaseHTTPRequestHandler):
    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        parsed = urlparse(self.path)
        path, query = parsed.path, parse_qs(parsed.query)
        if path == "/zones":
            self.reply(200, [zone(room) for room in PENDING])
        elif path == "/rooms":
            self.reply(200, [{"name": room, "household": "S2", "zone": room} for room in PENDING])
        elif path.startswith("/zones/") and path.endswith("/state"):
            room = unquote(path[len("/zones/"):-len("/state")])
            if room.split("@")[0] not in PENDING:
                self.reply(404, {"detail": "unknown room", "code": "UNKNOWN_ROOM"})
                return
            room = room.split("@")[0]
            track = {"title": "Aria", "creator": "Glenn Gould", "album": "Goldberg", "uri": "x-sonos-spotify:stub",
                     "duration_secs": 183, "position_secs": 0, "art_url": ART[room]}
            self.reply(200, {"zone": zone(room), "transport_state": transport(room, True), "volume": 20, "track": track})
        elif path == "/spotify/status":
            self.reply(200, spotify_status())
        elif path == "/spotify/albums":
            self.reply(200, page(ALBUMS, query))
        elif path.startswith("/spotify/albums/") and path.endswith("/tracks"):
            album = path[len("/spotify/albums/"):-len("/tracks")]
            self.reply(200, ALBUM_TRACKS.get(album, []))
        elif path == "/spotify/tracks":
            self.reply(200, page(LIKED, query))
        elif path == "/favorites":
            self.reply(200, FAVORITES)
        elif path == "/_debug/posts":
            with LOCK:
                self.reply(200, list(POSTS))
        elif path == "/_debug/commands":
            with LOCK:
                self.reply(200, list(COMMANDS))
        elif path == "/health":
            self.reply(200, {"status": "ok", "version": "stub"})
        elif path == "/events":
            self.reply(403, DENIED)
        else:
            self.reply(404, {"detail": "not found", "code": "NOT_FOUND"})

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        path = self.path.split("?")[0]
        if path == "/dj/sync":
            with LOCK:
                if SYNC["reads"] is None:
                    SYNC["reads"] = 0
            self.reply(202, spotify_status())
            return
        if path == "/_debug/signout":
            with LOCK:
                SIGNED["in"] = False
                SYNC["reads"] = None
            self.reply(200, {"signed_in": False})
            return
        if path == "/auth/spotify/exchange":
            with LOCK:
                POSTS.append({"path": path, "body": body})
                SIGNED["in"] = True
            self.reply(200, {"signed_in": True})
            return
        if path in ("/play", "/dj/start", "/dj/skip", "/dj/stop"):
            with LOCK:
                POSTS.append({"path": path, "body": body})
            self.reply(200, {"done": "ok " + path, "changed": True})
            return
        room = str(body.get("zone", "")).split("@")[0]
        if path in ("/pause", "/resume", "/play/favorite"):
            with LOCK:
                COMMANDS.append({"path": path, "body": body})
        if path == "/play/favorite" and room in PENDING:
            self.reply(200, {"done": "playing favorite in " + room, "changed": True})
            return
        if room not in PENDING or path not in ("/resume", "/pause"):
            self.reply(404, {"detail": "not found", "code": "NOT_FOUND"})
            return
        with LOCK:
            if path == "/resume":
                PENDING[room] = 0 if room == "Lag Room" else None
            else:
                PENDING[room] = None
                TRANSPORT[room] = "paused"
        self.reply(200, {"done": path[1:] + "d " + room, "changed": True})

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
