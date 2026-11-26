//! A saved direct source retains its identity across renderer XML serialization.

use super::*;
use fsonos_proto::didl::{equivalent_metadata, xml_escape};

const SOURCE: &str = "http://example.invalid/sleep.flac";
const ORIGINAL: &str = concat!(
    "<DIDL-Lite xmlns='urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/' ",
    "xmlns:dc='http://purl.org/dc/elements/1.1/' ",
    "xmlns:upnp='urn:schemas-upnp-org:metadata-1-0/upnp/' ",
    "xmlns:v='urn:example:service'>",
    "<item id='recording' parentID='-1' restricted='true'>",
    "<dc:title>Night &amp; quiet</dc:title>",
    "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
    "<res protocolInfo='http-get:*:audio/flac:*' duration='0:10:00'>",
    "http://example.invalid/sleep.flac</res>",
    "<desc id='cdudn' nameSpace='urn:example:account'>account-one</desc>",
    "<v:flags>7</v:flags>",
    "</item></DIDL-Lite>",
);
const REWRITTEN: &str = concat!(
    "<d:DIDL-Lite xmlns:d=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" ",
    "xmlns:title=\"http://purl.org/dc/elements/1.1/\" ",
    "xmlns:u=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" ",
    "xmlns:vendor=\"urn:example:service\">\n",
    " <d:item restricted=\"true\" parentID=\"-1\" id=\"recording\">\n",
    "  <title:title>Night &#38; quiet</title:title>\n",
    "  <u:class>object.item.audioItem.musicTrack</u:class>\n",
    "  <d:res duration=\"0:10:00\" protocolInfo=\"http-get:*:audio/flac:*\">",
    "http://example.invalid/sleep.flac</d:res>\n",
    "  <d:desc nameSpace=\"urn:example:account\" id=\"cdudn\">account-one</d:desc>\n",
    "  <vendor:flags>7</vendor:flags>\n",
    " </d:item>\n</d:DIDL-Lite>",
);

struct MetadataReplies<'a> {
    lan: &'a SimLan,
    metadata: &'a str,
    rewritten: AtomicUsize,
}

impl<'a> MetadataReplies<'a> {
    fn new(lan: &'a SimLan, metadata: &'a str) -> Self {
        Self {
            lan,
            metadata,
            rewritten: AtomicUsize::new(0),
        }
    }
}

impl Transport for MetadataReplies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.lan.soap_post(host, path, header, body)?;
        let original = xml_escape(ORIGINAL);
        if header.ends_with("#GetMediaInfo\"") && reply.contains(&original) {
            self.rewritten.fetch_add(1, Ordering::Relaxed);
            Ok(reply.replace(&original, &xml_escape(self.metadata)))
        } else {
            Ok(reply)
        }
    }
}

fn interrupted(generation: u8, action: &'static str) -> (House, Journal, SleepFade) {
    let h = House::with_generation(generation);
    soap::set_av_transport_uri(&h.lan, h.ip("SleepA"), SOURCE, ORIGINAL).unwrap();
    soap::play(&h.lan, h.ip("SleepA")).unwrap();
    let prior = Journal::default();
    let fade = h.crash(&prior, action, 1, true);
    // The old timer and fader were dropped by crash(); reopen the durable
    // journal without relying on the interrupted worker's in-memory state.
    let bytes = serde_json::to_vec(&fade).unwrap();
    let restored: SleepFade = serde_json::from_slice(&bytes).unwrap();
    assert!(restored.valid());
    let journal = Journal {
        saved: Mutex::new(Some(restored.clone())),
        fail: false,
    };
    (h, journal, restored)
}

#[test]
fn restarted_cleanup_restores_owned_levels_after_metadata_reserialization() {
    assert_ne!(ORIGINAL, REWRITTEN);
    assert!(equivalent_metadata(ORIGINAL, REWRITTEN));
    for generation in [1, 2] {
        let (h, journal, mut fade) = interrupted(generation, "#Pause");
        assert_eq!(h.levels(), (0, 0));
        assert_eq!(h.state(), TransportState::Paused);
        let t = MetadataReplies::new(&h.lan, REWRITTEN);
        assert_eq!(
            fade.recovery(&t, &h.households, now(), true).unwrap(),
            SleepRecovery::Restore
        );
        let before = h.sim.soap_log().len();
        fade.restore(&t, &h.households, &Fader::default(), &journal)
            .unwrap();
        assert_eq!(h.levels(), (30, 20));
        assert_eq!(h.state(), TransportState::Paused);
        assert_eq!(
            soap::get_media_info(&h.lan, h.ip("SleepA")).unwrap().uri,
            SOURCE
        );
        assert!(journal.saved.lock().unwrap().is_none());
        assert!(t.rewritten.load(Ordering::Relaxed) >= 4);
        let log = h.sim.soap_log();
        let writes: Vec<_> = log[before..]
            .iter()
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .map(|entry| entry.action.as_str())
            .collect();
        assert_eq!(writes, ["SetVolume", "SetVolume"]);
    }
}

#[test]
fn restarted_fade_resumes_with_its_original_levels_after_metadata_reserialization() {
    for generation in [1, 2] {
        let (h, journal, fade) = interrupted(generation, "#SetVolume");
        assert_eq!(h.levels(), (0, 20));
        let t = MetadataReplies::new(&h.lan, REWRITTEN);
        assert_eq!(
            fade.recovery(&t, &h.households, now(), true).unwrap(),
            SleepRecovery::Resume
        );
        let timers = SleepTimers::with_fade(Duration::ZERO);
        assert!(timers.adopt_resuming(fade.timer.clone(), fade));
        let ticket = timers
            .due_runs(now() + chrono::TimeDelta::seconds(1), 1)
            .remove(0);
        assert_eq!(
            timers
                .run_due_with_checkpoint(&t, &h.households, &Fader::default(), &ticket, &journal)
                .unwrap(),
            SleepOutcome::Paused
        );
        assert_eq!(h.levels(), (30, 20));
        assert_eq!(h.state(), TransportState::Paused);
        assert!(journal.saved.lock().unwrap().is_none());
        assert!(t.rewritten.load(Ordering::Relaxed) >= 4);
        assert_eq!(
            h.sim
                .soap_log()
                .iter()
                .filter(|entry| entry.action == "Pause")
                .count(),
            1
        );
    }
}

#[test]
fn changed_or_missing_native_metadata_cannot_authorize_restarted_cleanup() {
    let changed = [
        REWRITTEN.replace(SOURCE, "http://example.invalid/different.flac"),
        REWRITTEN.replace("account-one", "account-two"),
        REWRITTEN.replace(
            "<vendor:flags>7</vendor:flags>",
            "<vendor:flags>8</vendor:flags>",
        ),
        "<DIDL-Lite>".into(),
        String::new(),
    ];
    for generation in [1, 2] {
        let (h, journal, mut fade) = interrupted(generation, "#Pause");
        for metadata in &changed {
            let t = MetadataReplies::new(&h.lan, metadata);
            assert_eq!(
                fade.recovery(&t, &h.households, now(), true).unwrap(),
                SleepRecovery::Changed
            );
            let before = h.sim.soap_log().len();
            fade.restore(&t, &h.households, &Fader::default(), &journal)
                .unwrap();
            assert_eq!(h.levels(), (0, 0));
            assert_eq!(h.state(), TransportState::Paused);
            assert!(t.rewritten.load(Ordering::Relaxed) >= 4);
            assert!(
                h.sim.soap_log()[before..]
                    .iter()
                    .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse")
            );
            // Each candidate represents a separate restart of the same saved
            // journal; retirement from one refusal cannot short-circuit another.
            *journal.saved.lock().unwrap() = Some(fade.clone());
        }
    }
}

#[test]
fn equivalent_metadata_does_not_authorize_recovery_of_a_newer_source() {
    for generation in [1, 2] {
        let (h, journal, mut fade) = interrupted(generation, "#Pause");
        soap::set_av_transport_uri(
            &h.lan,
            h.ip("SleepA"),
            "http://example.invalid/new-stream.flac",
            ORIGINAL,
        )
        .unwrap();
        let t = MetadataReplies::new(&h.lan, REWRITTEN);
        assert_eq!(
            fade.recovery(&t, &h.households, now(), true).unwrap(),
            SleepRecovery::Changed
        );
        let before = h.sim.soap_log().len();
        fade.restore(&t, &h.households, &Fader::default(), &journal)
            .unwrap();
        assert_eq!(h.levels(), (0, 0));
        assert!(
            h.sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse")
        );
    }
}
