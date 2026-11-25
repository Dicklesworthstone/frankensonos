//! Native direct-source handoffs compare playback identity, not XML spelling.

use super::*;
use fsonos_proto::didl::{equivalent_metadata, xml_escape};

const SOURCE: &str = "http://example.invalid/native-recording.mp3";

fn native_document() -> String {
    metadata(SOURCE)
        .replace("Queue item", "Selection &amp; recording")
        .replace(
            "</item>",
            "<desc id=\"cdudn\" nameSpace=\"urn:example:account\">account-one</desc>\
             <v:extension xmlns:v=\"urn:example:service\" quality=\"original\">retained</v:extension></item>",
        )
}

fn reserialized(original: &str) -> String {
    original
        .replace("xmlns:dc=", "xmlns:label=")
        .replace("dc:title", "label:title")
        .replace("xmlns:upnp=", "xmlns:kind=")
        .replace("upnp:class", "kind:class")
        .replace(
            "id=\"file\" parentID=\"-1\" restricted=\"true\"",
            "restricted='true' parentID='-1' id='file'",
        )
        .replace("Selection &amp; recording", "Selection &#38; recording")
}

struct MetadataReplies<'a> {
    during: During<'a>,
    original: String,
    alternate: String,
    change_after_clip: bool,
    reads_per_spelling: usize,
    reads: AtomicUsize,
    rewrites: AtomicUsize,
}

impl Transport for MetadataReplies<'_> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.during.lan.http_get(url)
    }

    fn soap_post(
        &self,
        address: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.during.soap_post(address, path, action, body)?;
        let observe = !self.change_after_clip
            || (self.during.advanced.load(Ordering::Acquire)
                && address == host(self.during.sim, "Beta"));
        let original = xml_escape(&self.original);
        if observe && action.ends_with("#GetMediaInfo\"") && reply.contains(&original) {
            let read = self.reads.fetch_add(1, Ordering::AcqRel);
            if !(read / self.reads_per_spelling).is_multiple_of(2) {
                self.rewrites.fetch_add(1, Ordering::AcqRel);
                return Ok(reply.replace(&original, &xml_escape(&self.alternate)));
            }
        }
        Ok(reply)
    }
}

fn announce_native(t: &MetadataReplies<'_>) -> AnnounceReport {
    let h = houses(t.during.sim, t.during.lan);
    let target = resolve_room(&h, "Alpha").unwrap();
    Announcer::with_timing(Duration::from_millis(1), Duration::from_millis(20))
        .announce(
            t,
            &[target],
            &Clip {
                url: CLIP.into(),
                title: "Native metadata continuity".into(),
                duration: Duration::from_millis(1),
            },
            25,
            &|_| {
                if !t.change_after_clip {
                    assert_eq!(
                        t.reads.load(Ordering::Acquire),
                        2,
                        "the origin must finish its two reads before the full snapshot"
                    );
                }
                25
            },
        )
        .unwrap()
}

fn start_native(sim: &SimHandle, lan: &SimLan, metadata: &str) {
    let alpha = host(sim, "Alpha");
    soap::set_av_transport_uri(lan, alpha, SOURCE, metadata).unwrap();
    soap::seek_position(lan, alpha, 40).unwrap();
    soap::play(lan, alpha).unwrap();
}

#[test]
fn native_metadata_reserialization_keeps_partial_group_music_continuous() {
    for s2 in [false, true] {
        // Single reads exercise capture's internal confirmation. Pairs keep
        // each snapshot internally identical but change Origin's retained
        // second read into the full snapshot's different fourth read, so
        // announcement admission must also accept equivalent native XML.
        for reads_per_spelling in [1, 2] {
            let (sim, lan) = setup(s2, TransportState::Playing);
            let original = native_document();
            start_native(&sim, &lan, &original);
            let from = sim.soap_log().len();
            let t = MetadataReplies {
                during: During::new(&sim, &lan, Change::None),
                alternate: reserialized(&original),
                original,
                change_after_clip: false,
                reads_per_spelling,
                reads: AtomicUsize::new(0),
                rewrites: AtomicUsize::new(0),
            };
            restored(&announce_native(&t));
            assert!(t.reads.load(Ordering::Acquire) >= 4);
            assert!(t.rewrites.load(Ordering::Acquire) > 0);
            assert_eq!(t.during.handbacks.load(Ordering::Acquire), 1);
            let h = houses(&sim, &lan);
            assert_eq!(
                resolve_room(&h, "Beta").unwrap().coordinator.id,
                id(&sim, "Alpha")
            );
            let alpha = host(&sim, "Alpha");
            let media = soap::get_media_info(&lan, alpha).unwrap();
            assert_eq!(media.uri, SOURCE);
            assert!(equivalent_metadata(&media.uri_metadata, &t.original));
            assert_eq!(
                soap::get_position_info(&lan, alpha).unwrap().position_secs,
                Some(51)
            );
            assert_eq!(
                soap::get_transport_info(&lan, alpha).unwrap().state,
                TransportState::Playing
            );
            assert_eq!(soap::get_volume(&lan, alpha).unwrap(), 45);
            assert!(soap::get_mute(&lan, alpha).unwrap());
            assert_eq!(soap::get_volume(&lan, host(&sim, "Beta")).unwrap(), 60);
            assert!(sim.soap_log()[from..].iter().all(|entry| {
                !matches!(
                    entry.action.as_str(),
                    "Seek" | "AddURIToQueue" | "RemoveAllTracksFromQueue"
                ) && !(entry.action == "SetAVTransportURI"
                    && entry
                        .args
                        .iter()
                        .any(|(name, value)| name == "CurrentURI" && value == SOURCE))
            }));
        }
    }
}

#[test]
fn changed_native_metadata_still_refuses_the_survivors_handback() {
    for s2 in [false, true] {
        for (old, new) in [
            ("account-one", "account-two"),
            ("retained", "replaced"),
            ("native-recording.mp3", "different-recording.mp3"),
        ] {
            let (sim, lan) = setup(s2, TransportState::Playing);
            let original = native_document();
            start_native(&sim, &lan, &original);
            let from = sim.soap_log().len();
            let t = MetadataReplies {
                during: During::new(&sim, &lan, Change::None),
                alternate: reserialized(&original).replace(old, new),
                original,
                change_after_clip: true,
                reads_per_spelling: 1,
                reads: AtomicUsize::new(0),
                rewrites: AtomicUsize::new(0),
            };
            let report = announce_native(&t);
            assert!(report.households[0].outcome.is_ok(), "{report:?}");
            assert!(
                !report.households[0].restore_failed.is_empty(),
                "{report:?}"
            );
            assert!(t.rewrites.load(Ordering::Acquire) > 0, "{old}");
            assert_eq!(t.during.handbacks.load(Ordering::Acquire), 0);
            let beta = host(&sim, "Beta");
            assert_eq!(soap::get_media_info(&lan, beta).unwrap().uri, SOURCE);
            assert_eq!(
                soap::get_position_info(&lan, beta).unwrap().position_secs,
                Some(51)
            );
            assert_eq!(
                soap::get_transport_info(&lan, beta).unwrap().state,
                TransportState::Playing
            );
            assert!(sim.soap_log()[from..].iter().all(|entry| {
                !matches!(
                    entry.action.as_str(),
                    "Seek" | "AddURIToQueue" | "RemoveAllTracksFromQueue"
                )
            }));
        }
    }
}
