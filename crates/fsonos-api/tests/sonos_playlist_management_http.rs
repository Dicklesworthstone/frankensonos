//! Native playlist lifecycle through the real HTTP listener and S1/S2 speakers.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::{Identity, Surface, WebPolicy};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::HouseholdId;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const CONTENTS: &str = "/sonos/playlists/contents?zone=Room&playlist=SQ%3A7";
const RENAME: &str = "/sonos/playlists/rename";
const REMOVE: &str = "/sonos/playlists/remove";
const REMOVE_TRACKS: &str = "/sonos/playlists/remove-tracks";
const MOVE_TRACKS: &str = "/sonos/playlists/move-tracks";

#[test]
fn http_saved_occurrence_edits_confirm_new_revisions_and_reject_stale_or_unbound_requests() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let api = Api::start(&sim, Client::LoopbackHttp);
        let before = api.get(CONTENTS);
        let total = before["total"].as_u64().unwrap();
        let queue = api.get("/zones/Room/queue");
        let request = json!({
            "zone":"Room", "playlist":"SQ:7", "expected_revision":before["revision"],
            "position":1, "count":1, "insert_before":total + 1,
        });
        let (status, moved) = api.call(MOVE_TRACKS, Some(&request));
        assert_eq!(status, 200, "{moved}");
        assert_eq!(moved["operation"], "move_tracks");
        assert_eq!(moved["previous_total"], total);
        assert_eq!(moved["total"], total);
        assert_eq!(moved["insert_before"], total + 1);
        assert_eq!(moved["changed"], true);
        let after = api.get(CONTENTS);
        assert_eq!(after["revision"], moved["revision"]);
        let names = |page: &Value| {
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["title"].clone())
                .collect::<Vec<_>>()
        };
        let mut wanted = names(&before);
        wanted.rotate_left(1);
        assert_eq!(names(&after), wanted);
        let stale = json!({
            "zone":"Room", "playlist":"SQ:7", "expected_revision":before["revision"], "position":1, "count":1,
        });
        let (status, error) = api.call(REMOVE_TRACKS, Some(&stale));
        assert_eq!(status, 409, "{error}");
        assert_eq!(error["code"], "QUEUE_CHANGED");
        let mut remove = stale;
        remove["expected_revision"] = after["revision"].clone();
        let (status, removed) = api.call(REMOVE_TRACKS, Some(&remove));
        assert_eq!(status, 200, "{removed}");
        assert_eq!(removed["operation"], "remove_tracks");
        assert_eq!(removed["total"], total - 1);
        assert!(removed.get("insert_before").is_none());
        assert_eq!(api.get(CONTENTS)["revision"], removed["revision"]);
        assert_eq!(api.get("/zones/Room/queue"), queue);
        let start = sim.soap_log().len();
        for body in [
            json!({"zone":"Room", "playlist":"SQ:7", "position":1, "count":1}),
            json!({"zone":"Room", "playlist":"SQ:7", "expected_revision":removed["revision"], "position":0, "count":1}),
            json!({"zone":"Room", "playlist":"SQ:7", "expected_revision":removed["revision"], "position":1, "count":1, "force":true}),
        ] {
            let (status, error) = api.call(REMOVE_TRACKS, Some(&body));
            assert_eq!(status, 422, "{error}");
        }
        assert_eq!(sim.soap_log().len(), start);
        let spec = api.get("/openapi.json");
        for path in [REMOVE_TRACKS, MOVE_TRACKS] {
            let schema = &spec["paths"][path]["post"]["requestBody"];
            assert_eq!(schema["required"], true, "{schema}");
        }
        let denied = Api::start(&sim, Client::Unknown);
        let (status, error) = denied.call(REMOVE_TRACKS, Some(&remove));
        assert_eq!(status, 403, "{error}");
    }
}

#[test]
fn http_native_occurrence_batches_retain_order_and_distinguish_queue_and_playlist_proofs() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let api = Api::start(&sim, Client::LoopbackHttp);
        for mode in ["append", "next", "play"] {
            let page = api.get(CONTENTS);
            let queue = api.get("/zones/Room/queue");
            let body = json!({
                "zone":"Room", "playlist":"SQ:7", "positions":[3,2,3],
                "expected_revision":page["revision"], "mode":mode,
                "expected_queue_revision":queue["revision"],
            });
            let (status, result) = api.call("/sonos/playlists/tracks", Some(&body));
            assert_eq!(status, 200, "{mode}: {result}");
            assert_eq!(result["positions"], json!([3, 2, 3]));
            assert_eq!(result["mode"], mode);
            assert_eq!(
                result["titles"],
                json!([
                    page["items"][2]["title"],
                    page["items"][1]["title"],
                    page["items"][2]["title"]
                ])
            );
            assert_eq!(result["changed"], true);
            assert!(result.get("metadata").is_none());
            if mode == "play" {
                assert!(result.get("first").is_none());
                let after = api.get("/zones/Room/queue");
                assert_eq!(after["total"], 3);
                assert_eq!(after["playing"], 1);
            } else {
                assert_eq!(result["count"], 3);
            }
            // A source token never substitutes for a destination queue token.
            let mut stale = body;
            stale["expected_queue_revision"] = page["revision"].clone();
            let start = sim.soap_log().len();
            let (status, result) = api.call("/sonos/playlists/tracks", Some(&stale));
            assert_eq!(status, 409, "{result}");
            assert_eq!(result["code"], "QUEUE_CHANGED");
            assert!(
                sim.soap_log()[start..]
                    .iter()
                    .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
            );
        }
    }
}

#[test]
fn http_explicit_spotify_additions_preserve_playback_and_require_the_saved_revision() {
    const PATH: &str = "/sonos/playlists/add-tracks";
    for s2 in [false, true] {
        let sim = sim(s2);
        let api = Api::start(&sim, Client::LoopbackHttp);
        let before = api.get(CONTENTS);
        let queue = api.get("/zones/Room/queue");
        let body = json!({
            "zone": "Room", "playlist": "SQ:7", "expected_revision": before["revision"],
            "tracks": [
                {"source_uri": "https://open.spotify.com/track/0000000000000000000001", "title": "First"},
                {"source_uri": "spotify:episode:0000000000000000000002", "title": "Episode"},
                {"source_uri": "spotify:track:0000000000000000000001", "title": "First"}
            ]
        });
        let start = sim.soap_log().len();
        let (status, added) = api.call(PATH, Some(&body));
        assert_eq!(status, 200, "{added}");
        assert_eq!(added["playlist"], before["playlist"]);
        assert_eq!(added["previous_total"], before["total"]);
        assert_eq!(added["added"], 3);
        assert_eq!(
            added["source_uris"],
            json!([
                "spotify:track:0000000000000000000001",
                "spotify:episode:0000000000000000000002",
                "spotify:track:0000000000000000000001"
            ])
        );
        assert_eq!(added["titles"], json!(["First", "Episode", "First"]));
        assert_eq!(added["logged"], true);
        let after = api.get(CONTENTS);
        assert_eq!(after["revision"], added["revision"]);
        assert_eq!(after["total"], before["total"].as_u64().unwrap() + 3);
        assert_eq!(api.get("/zones/Room/queue"), queue);
        let (status, error) = api.call(PATH, Some(&body));
        assert_eq!(status, 409, "{error}");
        assert_eq!(error["code"], "QUEUE_CHANGED");
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|call| call.action != "Browse" && !call.action.starts_with("Get"))
            .map(|call| call.action.clone())
            .collect();
        assert_eq!(writes, ["AddURIToSavedQueue"; 3]);
        let doc = api.get("/openapi.json");
        let operation = &doc["paths"][PATH]["post"];
        assert_eq!(operation["operationId"], "add_tracks_to_sonos_playlist");
        let input = schema(
            &doc,
            &operation["requestBody"]["content"]["application/json"]["schema"],
        );
        for field in ["zone", "playlist", "expected_revision", "tracks"] {
            assert!(
                input["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(field)),
                "{input}"
            );
        }
        let denied = Api::start(&sim, Client::Unknown);
        let (status, error) = denied.call(PATH, Some(&body));
        assert_eq!(status, 403, "{error}");
    }
}

#[test]
fn http_explicit_saved_additions_reject_bad_selections_without_speaker_io() {
    const PATH: &str = "/sonos/playlists/add-tracks";
    let sim = sim(true);
    let api = Api::start(&sim, Client::LoopbackHttp);
    let original = json!({
        "zone": "Room", "playlist": "SQ:7", "expected_revision": "observed",
        "tracks": [{"source_uri": "spotify:track:0000000000000000000001"}]
    });
    let start = sim.soap_log().len();
    for field in ["zone", "playlist", "expected_revision", "tracks"] {
        let mut body = original.clone();
        body.as_object_mut().unwrap().remove(field);
        let (status, failure) = api.call(PATH, Some(&body));
        assert_eq!(status, 422, "{failure}");
    }
    for tracks in [
        json!([]),
        json!([{"source_uri": "spotify:album:0000000000000000000001"}]),
        json!([{"source_uri": "spotify:track:0000000000000000000001", "metadata": "supplied"}]),
    ] {
        let mut body = original.clone();
        body["tracks"] = tracks;
        let (status, failure) = api.call(PATH, Some(&body));
        assert_eq!(status, 422, "{failure}");
    }
    assert_eq!(sim.soap_log().len(), start);
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().unwrap())
        .blocking_threads(0, 4)
        .build()
        .unwrap()
}

struct Api {
    addr: SocketAddr,
    server: Arc<TcpServer>,
    worker: Option<JoinHandle<()>>,
}

impl Api {
    fn start(sim: &SimHandle, client: Client) -> Self {
        let player = sim.player("Room").unwrap();
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&sim.lan(), player.ip).unwrap());
        household.id = Some(HouseholdId(player.household.clone()));
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(vec![household.clone()])),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "http");
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
        let app = Arc::new(fsonos_api::app(
            &Arc::new(surface),
            &Identity::fixed(client),
            &web,
        ));
        let server = Arc::new(TcpServer::new(
            ServerConfig::new("127.0.0.1:0").with_allowed_hosts(web.hosts().to_vec()),
        ));
        let (tx, rx) = mpsc::channel();
        let running = Arc::clone(&server);
        let worker = thread::spawn(move || {
            runtime().block_on(async move {
                let cx = Cx::current().unwrap();
                let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                let _ = running.serve_on_app(&cx, listener, app).await;
            });
        });
        Self {
            addr: rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            server,
            worker: Some(worker),
        }
    }

    fn call(&self, path: &str, body: Option<&Value>) -> (u16, Value) {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let http = HttpClient::default_for_runtime(&cx);
            let url = format!("http://{}{path}", self.addr);
            let request = match body {
                Some(body) => http
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(body.to_string()),
                None => http.get(url),
            };
            let response = request.send(&cx).await.unwrap();
            (
                response.status,
                serde_json::from_slice(&response.body).unwrap(),
            )
        })
    }

    fn get(&self, path: &str) -> Value {
        let (status, body) = self.call(path, None);
        assert_eq!(status, 200, "{path}: {body}");
        body
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        drop(std::net::TcpStream::connect(self.addr));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn sim(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [SimPlayerSpec::new("Room", model)];
    let builder = SimHousehold::builder();
    if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    }
    .spawn()
    .unwrap()
}

#[test]
fn http_rename_and_remove_use_reviewed_identity_without_changing_playback() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let lan = sim.lan();
        soap::enqueue_uri(
            &lan,
            host,
            "x-file-cifs://example.invalid/playing.flac",
            "",
            0,
            false,
        )
        .unwrap();
        soap::set_av_transport_uri(&lan, host, &format!("x-rincon-queue:{}#0", player.uuid), "")
            .unwrap();
        soap::seek_track(&lan, host, 1).unwrap();
        soap::play(&lan, host).unwrap();
        let playback = || {
            (
                browse_snapshot(&lan, host, QUEUE).unwrap(),
                soap::get_position_info(&lan, host).unwrap(),
                soap::get_transport_info(&lan, host).unwrap(),
            )
        };
        let before_playback = playback();
        let api = Api::start(&sim, Client::LoopbackHttp);
        let catalog = api.get("/sonos/playlists?zone=Room");
        let before = api.get(CONTENTS);
        let title = "Bach, Dvořák & <friends>";
        let start = sim.soap_log().len();
        let (status, renamed) = api.call(RENAME, Some(&json!({
            "zone":"Room", "playlist":"SQ:7", "expected_revision":before["revision"], "title":title
        })));
        assert_eq!(status, 200, "{renamed}");
        assert_eq!(renamed["playlist"]["id"], "SQ:7");
        assert_eq!(renamed["playlist"]["title"], title);
        assert_eq!(renamed["operation"], "rename");
        assert_eq!(renamed["changed"], true);
        assert_eq!(renamed["logged"], true);
        assert_eq!(playback(), before_playback);
        let (status, stale) = api.call(
            REMOVE,
            Some(&json!({
                "zone":"Room", "playlist":"SQ:7", "expected_revision":before["revision"]
            })),
        );
        assert_eq!(status, 409, "{stale}");
        assert_eq!(stale["code"], "PLAYLIST_CHANGED");
        assert_eq!(stale["retryable"], false);
        let reviewed = api.get(CONTENTS);
        assert_eq!(reviewed["items"], before["items"]);
        assert_ne!(reviewed["revision"], before["revision"]);
        let (status, removed) = api.call(
            REMOVE,
            Some(&json!({
                "zone":"Room", "playlist":"SQ:7", "expected_revision":reviewed["revision"]
            })),
        );
        assert_eq!(status, 200, "{removed}");
        assert_eq!(removed["playlist"], renamed["playlist"]);
        assert_eq!(removed["operation"], "remove");
        assert_eq!(removed["changed"], true);
        assert_eq!(removed["logged"], true);
        assert_eq!(playback(), before_playback);
        let remaining = api.get("/sonos/playlists?zone=Room");
        assert_eq!(
            remaining,
            json!(
                catalog
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|entry| entry["id"] != "SQ:7")
                    .collect::<Vec<_>>()
            )
        );
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .map(|entry| entry.action.clone())
            .collect();
        assert_eq!(writes, ["UpdateObject", "DestroyObject"]);
        drop(api);
        sim.shutdown();
    }
}

#[test]
fn malformed_management_requests_are_rejected_before_speaker_io() {
    let sim = sim(true);
    let api = Api::start(&sim, Client::LoopbackHttp);
    let start = sim.soap_log().len();
    for path in [RENAME, REMOVE] {
        let mut valid = json!({"zone":"Room", "playlist":"SQ:7", "expected_revision":"observed"});
        if path == RENAME {
            valid["title"] = json!("New title");
        }
        for field in ["zone", "playlist", "expected_revision"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            let (status, failure) = api.call(path, Some(&missing));
            assert_eq!(status, 422, "{path}/{field}: {failure}");
        }
        for (field, value) in [
            ("force", json!(true)),
            ("expected_revision", json!("")),
            ("playlist", json!("Morning")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            let (status, failure) = api.call(path, Some(&invalid));
            assert_eq!(status, 422, "{path}/{field}: {failure}");
        }
    }
    assert_eq!(sim.soap_log().len(), start);
    drop(api);
    sim.shutdown();
}

fn schema<'a>(doc: &'a Value, value: &'a Value) -> &'a Value {
    value["$ref"]
        .as_str()
        .and_then(|reference| reference.rsplit('/').next())
        .map_or(value, |name| &doc["components"]["schemas"][name])
}

#[test]
fn openapi_exposes_revision_bound_management_requests_and_confirmed_results() {
    let sim = sim(true);
    let api = Api::start(&sim, Client::LoopbackHttp);
    let doc = api.get("/openapi.json");
    for (path, operation) in [
        (RENAME, "rename_sonos_playlist"),
        (REMOVE, "remove_sonos_playlist"),
    ] {
        let op = &doc["paths"][path]["post"];
        assert_eq!(op["operationId"], operation);
        let input = schema(
            &doc,
            &op["requestBody"]["content"]["application/json"]["schema"],
        );
        for field in ["zone", "playlist", "expected_revision"] {
            assert!(
                input["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(field)),
                "{path}: {input}"
            );
        }
        if path == RENAME {
            assert!(
                input["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("title"))
            );
        }
        let output = schema(
            &doc,
            &op["responses"]["200"]["content"]["application/json"]["schema"],
        );
        for field in ["done", "zone", "playlist", "operation", "changed", "logged"] {
            assert!(
                output["properties"][field].is_object(),
                "{path}/{field}: {output}"
            );
        }
    }
    drop(api);
    sim.shutdown();
}
