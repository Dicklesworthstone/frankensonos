//! Native metadata may be reserialized by a renderer across snapshot reads.
//! All requests cross real simulator sockets; only the returned DIDL spelling
//! and explicitly injected source replies differ from the ordinary transport.

use super::*;
use fsonos_proto::didl::{equivalent_metadata, xml_escape};
use std::cell::Cell;

const SOURCE: &str = "http://example.invalid/recording.flac";
const ORIGINAL: &str = concat!(
    "<DIDL-Lite xmlns='urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/' ",
    "xmlns:dc='http://purl.org/dc/elements/1.1/' xmlns:v='urn:example:service' ",
    "xmlns:upnp='urn:schemas-upnp-org:metadata-1-0/upnp/'>",
    "<item id='source' parentID='-1' restricted='true'>",
    "<dc:title>Recording &amp; encore</dc:title>",
    "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
    "<res duration='0:06:00' protocolInfo='http-get:*:audio/flac:*'>",
    "http://example.invalid/recording.flac</res>",
    "<desc id='cdudn' nameSpace='urn:example:account'>account-one</desc>",
    "<v:extension v:quality='original'>kept &amp; exact</v:extension>",
    "</item></DIDL-Lite>",
);
const NORMALIZED: &str = concat!(
    "<d:DIDL-Lite xmlns:d=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" ",
    "xmlns:title=\"http://purl.org/dc/elements/1.1/\" xmlns:service=\"urn:example:service\" ",
    "xmlns:kind=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\n",
    "  <d:item restricted=\"true\" parentID=\"-1\" id=\"source\">\n",
    "    <title:title>Recording &#38; encore</title:title>\n",
    "    <kind:class>object.item.audioItem.musicTrack</kind:class>\n",
    "    <d:res protocolInfo=\"http-get:*:audio/flac:*\" duration=\"0:06:00\">",
    "http://example.invalid/recording.flac</d:res>\n",
    "    <d:desc nameSpace=\"urn:example:account\" id=\"cdudn\">account-one</d:desc>\n",
    "    <service:extension service:quality=\"original\">kept &#38; exact</service:extension>\n",
    "  </d:item>\n</d:DIDL-Lite>",
);

#[derive(Clone, Copy, Debug)]
enum SourceReply {
    Accepted,
    Lost,
    TransientRefusal,
}

struct MetadataReplies<'a> {
    inner: &'a SimLan,
    alternate: &'a str,
    source_reply: SourceReply,
    reads: Cell<usize>,
    installs: Cell<usize>,
    calls: RefCell<Vec<String>>,
}

impl<'a> MetadataReplies<'a> {
    fn new(inner: &'a SimLan, alternate: &'a str, source_reply: SourceReply) -> Self {
        Self {
            inner,
            alternate,
            source_reply,
            reads: Cell::new(0),
            installs: Cell::new(0),
            calls: RefCell::default(),
        }
    }
}

impl Transport for MetadataReplies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(name.into());
        if name == "SetAVTransportURI" {
            let count = self.installs.get() + 1;
            self.installs.set(count);
            if matches!(self.source_reply, SourceReply::TransientRefusal) && count == 1 {
                return Err(ProtoError::SoapFault {
                    code: 402,
                    reason: "injected teardown refusal".into(),
                });
            }
        }
        let mut reply = self.inner.soap_post(host, path, action, body)?;
        if name == "GetMediaInfo" {
            let read = self.reads.get() + 1;
            self.reads.set(read);
            if read.is_multiple_of(2) {
                reply = reply.replace(&xml_escape(ORIGINAL), &xml_escape(self.alternate));
            }
        }
        if name == "SetAVTransportURI" && matches!(self.source_reply, SourceReply::Lost) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost source acknowledgement".into(),
            });
        }
        Ok(reply)
    }
}

fn changes() -> Vec<String> {
    vec![
        NORMALIZED.replace("recording.flac", "other.flac"),
        NORMALIZED.replace("account-one", "account-two"),
        NORMALIZED.replace("kept &#38; exact", "changed &#38; exact"),
    ]
}

#[test]
fn stable_snapshot_capture_accepts_native_metadata_reserialization() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        soap::set_av_transport_uri(&rig.lan, rig.host, SOURCE, ORIGINAL).unwrap();
        soap::play(&rig.lan, rig.host).unwrap();
        soap::seek_position(&rig.lan, rig.host, 23).unwrap();
        let t = MetadataReplies::new(&rig.lan, NORMALIZED, SourceReply::Accepted);
        let snap = snapshot::capture(&t, &rig.houses, &rig.snapshot.coordinator, 2).unwrap();
        let snapshot::SnapshotSource::Uri {
            uri,
            metadata,
            position_secs,
        } = snap.source
        else {
            panic!("expected direct source");
        };
        assert_eq!(uri, SOURCE);
        assert_eq!(position_secs, Some(23));
        assert!(equivalent_metadata(&metadata, ORIGINAL));
        assert_eq!(t.reads.get(), 2, "stable playback needs only one capture");
        assert_eq!(t.installs.get(), 0);
    }
}

#[test]
fn changing_native_resources_accounts_or_extensions_cannot_form_a_snapshot() {
    let rig = Rig::new(true);
    soap::set_av_transport_uri(&rig.lan, rig.host, SOURCE, ORIGINAL).unwrap();
    soap::play(&rig.lan, rig.host).unwrap();
    for changed in changes() {
        let t = MetadataReplies::new(&rig.lan, &changed, SourceReply::Accepted);
        assert!(snapshot::capture(&t, &rig.houses, &rig.snapshot.coordinator, 2).is_err());
        assert_eq!(t.reads.get(), 6, "all three mixed captures were rejected");
        assert_eq!(t.installs.get(), 0);
    }
}

#[test]
fn native_metadata_reserialization_preserves_restore_and_bounded_recovery() {
    for s2 in [false, true] {
        for reply in [
            SourceReply::Accepted,
            SourceReply::Lost,
            SourceReply::TransientRefusal,
        ] {
            let mut rig = Rig::new(s2);
            // A nonempty interruption source also exercises the retry anchor:
            // its metadata gets a different spelling after the first refusal.
            soap::set_av_transport_uri(&rig.lan, rig.host, OTHER, ORIGINAL).unwrap();
            rig.snapshot.source = snapshot::SnapshotSource::Uri {
                uri: SOURCE.into(),
                metadata: ORIGINAL.into(),
                position_secs: Some(10),
            };
            let t = MetadataReplies::new(&rig.lan, NORMALIZED, reply);
            let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
            assert!(report.skipped.is_empty(), "{reply:?}: {report:?}");
            assert_eq!(
                t.installs.get(),
                if matches!(reply, SourceReply::TransientRefusal) {
                    2
                } else {
                    1
                },
                "{reply:?}"
            );
            let calls = t.calls.borrow();
            assert_eq!(calls.iter().filter(|name| *name == "Seek").count(), 1);
            assert_eq!(calls.iter().filter(|name| *name == "Play").count(), 1);
            assert_eq!(
                soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
                SOURCE
            );
            assert_eq!(
                soap::get_position_info(&rig.lan, rig.host)
                    .unwrap()
                    .position_secs,
                Some(10)
            );
            assert_eq!(
                soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
                TransportState::Playing
            );
            rig.levels_restored();
        }
    }
}

#[test]
fn changed_native_metadata_never_authorizes_seek_resume_or_a_refused_retry() {
    for reply in [
        SourceReply::Accepted,
        SourceReply::Lost,
        SourceReply::TransientRefusal,
    ] {
        for changed in changes() {
            let mut rig = Rig::new(true);
            soap::set_av_transport_uri(&rig.lan, rig.host, OTHER, ORIGINAL).unwrap();
            rig.snapshot.source = snapshot::SnapshotSource::Uri {
                uri: SOURCE.into(),
                metadata: ORIGINAL.into(),
                position_secs: Some(10),
            };
            let t = MetadataReplies::new(&rig.lan, &changed, reply);
            let report = snapshot::restore(&t, &rig.houses, &rig.snapshot).unwrap();
            for aspect in [Aspect::Source, Aspect::Position, Aspect::Transport] {
                assert!(skipped(&report, aspect), "{reply:?}: {report:?}");
            }
            assert_eq!(t.installs.get(), 1, "no replay or stale retry: {reply:?}");
            assert!(
                t.calls
                    .borrow()
                    .iter()
                    .all(|name| name != "Seek" && name != "Play")
            );
            rig.levels_restored();
        }
    }
}
