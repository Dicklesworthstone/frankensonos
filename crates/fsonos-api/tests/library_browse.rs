//! Cached library browsing through the real HTTP app, with no speaker or Spotify I/O.

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::surface::dj_sync::library::BrowsePage;
use fsonos_core::store::{LibraryEntry, MemStore, Store};
use std::sync::{Arc, mpsc};
use std::time::Duration;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .blocking_threads(0, 4)
        .build()
        .expect("runtime")
}

fn app() -> fastapi::App {
    struct NoLan;
    impl fsonos_proto::Transport for NoLan {
        fn soap_post(
            &self,
            _: std::net::IpAddr,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<String, fsonos_proto::ProtoError> {
            panic!("library browsing must not contact a speaker");
        }
    }
    let mut store = MemStore::default();
    let entries = (0..51).map(|n| {
        let mut entry = LibraryEntry::default();
        entry.track.source_uri = format!("spotify:track:sim{n:03}");
        entry.track.title = format!("Track {n:03}");
        entry.album_uri = Some("spotify:album:simAlbum".into());
        entry.track.album = Some("A cached album".into());
        entry
    }).collect::<Vec<_>>();
    store.upsert_library(&entries).unwrap();
    store.save_playlists(&[("spotify:playlist:simList".into(), "A list".into())]).unwrap();
    let surface = fsonos_api::Surface::new(
        Box::new(NoLan),
        Box::new(|_| panic!("library browsing must not discover speakers")),
        fsonos_core::policy::Policy::default(),
        Box::new(fsonos_core::clock::SystemClock),
    ).with_action_log(Box::new(store), "http-test");
    fsonos_api::app(
        &Arc::new(surface),
        &fsonos_api::Identity::fixed(fsonos_core::policy::Client::LoopbackHttp),
        &fsonos_api::WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]),
    )
}

#[test]
fn browse_routes_page_filter_validate_and_describe_the_cache_over_loopback() {
    let server = Arc::new(TcpServer::new(ServerConfig::new("127.0.0.1:0")));
    let (tx, rx) = mpsc::channel();
    let worker = {
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            runtime().block_on(async move {
                let cx = Cx::current().unwrap();
                let listener = asupersync::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                let _ = server.serve_on_app(&cx, listener, Arc::new(app())).await;
            });
        })
    };
    let addr = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let paths = [
        "/library/browse",
        "/library/browse?offset=50",
        "/library/browse?kind=albums",
        "/library/browse?kind=playlists",
        "/library/browse?q=Track+050",
        "/library/browse?limit=51",
        "/library/browse?limit=1&limit=2",
        "/library/browse?offset=-1",
        "/library/browse?kind=albums&album_uri=spotify%3Aalbum%3AsimAlbum",
        "/openapi.json",
    ];
    let replies = runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let client = Client::default_for_runtime(&cx);
        let mut replies = Vec::new();
        for path in paths {
            let response = client.get(format!("http://{addr}{path}")).send(&cx).await.unwrap();
            replies.push((response.status, response.body));
        }
        replies
    });
    server.shutdown();
    drop(std::net::TcpStream::connect(addr));
    worker.join().unwrap();

    for (i, (status, _)) in replies.iter().enumerate() {
        assert_eq!(*status, if (5..=8).contains(&i) { 400 } else { 200 }, "{}", paths[i]);
    }
    let page = |i: usize| serde_json::from_slice::<BrowsePage>(&replies[i].1).unwrap();
    assert_eq!(page(0).items.len(), 50);
    assert_eq!(page(0).next_offset, Some(50));
    assert_eq!(page(1).items.len(), 1);
    assert_eq!(page(0).revision, page(1).revision);
    assert_eq!(page(2).items[0].cached_tracks, Some(51));
    assert_eq!(page(3).items[0].kind, "playlist");
    assert_eq!(page(4).items[0].title, "Track 050");
    let spec: serde_json::Value = serde_json::from_slice(&replies[9].1).unwrap();
    assert_eq!(spec["paths"]["/library/browse"]["get"]["operationId"], "browse_library");
}
