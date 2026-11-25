//! Source ownership follows native DIDL values across renderer serialization.
//! Every request still uses the S1/S2 simulator's real localhost sockets.

use fsonos_core::HouseholdState;
use fsonos_core::control::{self, SourceDestination};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{SpotifyContainer, equivalent_metadata, xml_escape};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

const SOURCE: &str = "http://example.invalid/current.flac";
const ALBUM: &str = "spotify:album:0000000000000000000123";
const ORIGINAL: &str = concat!(
    "<DIDL-Lite xmlns='urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/' ",
    "xmlns:dc='http://purl.org/dc/elements/1.1/' ",
    "xmlns:upnp='urn:schemas-upnp-org:metadata-1-0/upnp/'>",
    "<item id='recording' parentID='-1' restricted='true'>",
    "<dc:title>Current &amp; complete</dc:title>",
    "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
    "<res protocolInfo='http-get:*:audio/flac:*' duration='0:10:00'>",
    "http://example.invalid/current.flac</res>",
    "<desc id='cdudn' nameSpace='urn:example:account'>account-one</desc>",
    "</item></DIDL-Lite>",
);
const REWRITTEN: &str = concat!(
    "<d:DIDL-Lite xmlns:d=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" ",
    "xmlns:title=\"http://purl.org/dc/elements/1.1/\" ",
    "xmlns:u=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\n",
    " <d:item restricted=\"true\" parentID=\"-1\" id=\"recording\">\n",
    "  <title:title>Current &#38; complete</title:title>\n",
    "  <u:class>object.item.audioItem.musicTrack</u:class>\n",
    "  <d:res duration=\"0:10:00\" protocolInfo=\"http-get:*:audio/flac:*\">",
    "http://example.invalid/current.flac</d:res>\n",
    "  <d:desc nameSpace=\"urn:example:account\" id=\"cdudn\">account-one</d:desc>\n",
    " </d:item>\n</d:DIDL-Lite>",
);

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(s2: bool) -> Self {
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        } else {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let coordinator = PlayerId(player.uuid.clone());
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        soap::add_uri_to_queue(&lan, host, "http://example.invalid/old.flac", "", false).unwrap();
        soap::set_av_transport_uri(&lan, host, SOURCE, ORIGINAL).unwrap();
        soap::play(&lan, host).unwrap();
        Self {
            sim,
            lan,
            households: vec![household],
            coordinator,
            host,
        }
    }

    fn replace(
        &self,
        transport: &impl Transport,
        autoplay: bool,
    ) -> Result<(), fsonos_core::CoreError> {
        if autoplay {
            control::play_spotify_container(
                transport,
                &self.households,
                &self.coordinator,
                SpotifyContainer::Album,
                ALBUM,
                "Selected album",
            )
        } else {
            control::load_spotify_container(
                transport,
                &self.households,
                &self.coordinator,
                SpotifyContainer::Album,
                ALBUM,
                "Selected album",
            )
        }
    }
}

struct RewrittenReplies<'a> {
    inner: &'a SimLan,
    metadata: &'a str,
    reads: AtomicUsize,
}

impl<'a> RewrittenReplies<'a> {
    fn new(inner: &'a SimLan, metadata: &'a str) -> Self {
        Self {
            inner,
            metadata,
            reads: AtomicUsize::new(0),
        }
    }
}

impl Transport for RewrittenReplies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#GetMediaInfo\"")
            && !self.reads.fetch_add(1, Ordering::Relaxed).is_multiple_of(2)
        {
            Ok(reply.replace(&xml_escape(ORIGINAL), &xml_escape(self.metadata)))
        } else {
            Ok(reply)
        }
    }
}

#[test]
fn retained_and_serialized_source_proofs_accept_equivalent_native_metadata() {
    assert!(equivalent_metadata(ORIGINAL, REWRITTEN));
    for s2 in [false, true] {
        let f = Fixture::new(s2);
        let t = RewrittenReplies::new(&f.lan, REWRITTEN);
        let retained = SourceDestination::read(&t, f.host, &f.coordinator).unwrap();
        let serialized = serde_json::to_string(&retained).unwrap();
        let restored: SourceDestination = serde_json::from_str(&serialized).unwrap();
        restored.confirm(&t, f.host, &f.coordinator).unwrap();
        let original = SourceDestination::read(&t, f.host, &f.coordinator).unwrap();
        let rewritten = SourceDestination::read(&t, f.host, &f.coordinator).unwrap();
        assert_eq!(original, rewritten);
        assert_eq!(soap::get_media_info(&f.lan, f.host).unwrap().uri, SOURCE);
    }
}

#[test]
fn album_queue_replacement_survives_metadata_reserialization() {
    for s2 in [false, true] {
        for autoplay in [false, true] {
            let f = Fixture::new(s2);
            let t = RewrittenReplies::new(&f.lan, REWRITTEN);
            f.replace(&t, autoplay).unwrap();
            assert!(t.reads.load(Ordering::Relaxed) >= 2);
            let queue = content::browse_all(&f.lan, f.host, QUEUE).unwrap();
            assert!(!queue.is_empty());
            assert!(queue.iter().all(|item| {
                item.res
                    .as_ref()
                    .is_none_or(|res| res.uri != "http://example.invalid/old.flac")
            }));
            assert_eq!(
                soap::get_transport_info(&f.lan, f.host).unwrap().state == TransportState::Playing,
                autoplay,
            );
        }
    }
}

#[test]
fn changed_metadata_values_retire_source_proofs_before_queue_writes() {
    for changed in [
        REWRITTEN.replace("account-one", "account-two"),
        REWRITTEN.replace("current.flac", "different.flac"),
        REWRITTEN.replace("Current &#38; complete", "Different title"),
    ] {
        let f = Fixture::new(true);
        let t = RewrittenReplies::new(&f.lan, &changed);
        let from = f.sim.soap_log().len();
        assert!(f.replace(&t, true).is_err());
        assert!(
            f.sim.soap_log()[from..]
                .iter()
                .all(|entry| { entry.action.starts_with("Get") || entry.action == "Browse" })
        );
        assert_eq!(soap::get_media_info(&f.lan, f.host).unwrap().uri, SOURCE);
        assert_eq!(
            soap::get_transport_info(&f.lan, f.host).unwrap().state,
            TransportState::Playing,
        );
        let queue = content::browse_all(&f.lan, f.host, QUEUE).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue[0].res.as_ref().unwrap().uri,
            "http://example.invalid/old.flac"
        );
    }
}
