//! A saved queue also retains its complete physical group through capture.
//! Native replay identity and metadata repair are shared with playback moves.

use super::{QueueReference, invalid, queue};
#[cfg(test)]
use crate::queue_source::native::replay::Identity;
use crate::queue_source::native::replay::Selected;
use fsonos_proto::didl::NativeDidlObject;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

pub(super) struct Current {
    selected: Selected,
    group: queue::Group,
}

impl Current {
    pub(super) fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        reference: &QueueReference,
        items: &[NativeDidlObject],
    ) -> Result<Option<Self>, ProtoError> {
        let Some(selected) =
            Selected::read(t, host, &reference.coordinator, reference.track, items)?
        else {
            return Ok(None);
        };
        let group = queue::group(t, host, &reference.coordinator)?;
        if group.boot_seq != reference.boot_seq || selected.boot_seq != reference.boot_seq {
            return Err(invalid(
                "the queue owner changed while reading replay metadata",
            ));
        }
        Ok(Some(Self { selected, group }))
    }

    pub(super) fn selected(&self) -> &Selected {
        &self.selected
    }

    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        reference: &QueueReference,
    ) -> Result<(), ProtoError> {
        self.selected.confirm(t, host)?;
        let current = queue::group(t, host, &reference.coordinator)?;
        if current != self.group || current.boot_seq != reference.boot_seq {
            return Err(invalid("the selected track's group changed while saving"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::{SpotifyRenderParams, spotify_track_didl, spotify_track_uri};

    const TRACK: &str = "spotify:track:ObservedTrack";

    fn params() -> SpotifyRenderParams {
        SpotifyRenderParams {
            sid: 29,
            flags: 8232,
            sn: 4,
            cdudn: "observed-household-account".into(),
            item_id_prefix: "10032028".into(),
        }
    }

    #[test]
    fn replay_evidence_keeps_the_observed_prefix_account_and_native_extensions() {
        let params = params();
        let uri = spotify_track_uri(TRACK, &params);
        let metadata = spotify_track_didl(TRACK, "Observed", &params);
        let identity = Identity::read(&uri, &metadata).unwrap().unwrap();
        let queued = concat!(
            "<d:DIDL-Lite xmlns:d='urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/' ",
            "xmlns:upnp='urn:schemas-upnp-org:metadata-1-0/upnp/' ",
            "xmlns:v='urn:example:vendor' v:context='kept'>",
            "<d:item id='Q:0/7' parentID='Q:0' v:license='keep'>",
            "<upnp:class>object.item.audioItem.musicTrack</upnp:class>",
            "<v:context><![CDATA[native <payload> & bytes]]></v:context>",
            "<d:desc id='extra' nameSpace='urn:vendor'>keep</d:desc></d:item></d:DIDL-Lite>"
        );
        let replay = identity.repair(&uri, queued).unwrap().unwrap();
        assert!(replay.contains("id='10032028spotify%3atrack%3aObservedTrack'"));
        for native in [
            "v:context='kept'",
            "v:license='keep'",
            "<![CDATA[native <payload> & bytes]]>",
            "<d:desc id='extra' nameSpace='urn:vendor'>keep</d:desc>",
        ] {
            assert!(replay.contains(native), "{native}");
        }
        assert_eq!(
            Identity::read(&uri, &replay).unwrap(),
            Some(identity.clone())
        );
        assert!(
            identity
                .repair(&uri.replace("sn=4", "sn=5"), queued)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ambiguous_conflicting_and_wrong_track_evidence_is_not_repaired() {
        let params = params();
        let uri = spotify_track_uri(TRACK, &params);
        let metadata = spotify_track_didl(TRACK, "Observed", &params);
        for bad in [
            metadata.replace("ObservedTrack", "AnotherTrack"),
            metadata.replace("</item>", "<desc id='cdudn'>another-account</desc></item>"),
            metadata.replace("observed-household-account", ""),
            metadata.replace(
                "</item>",
                &format!(
                    "<res>{}</res></item>",
                    fsonos_proto::didl::xml_escape(&uri.replace("sn=4", "sn=5"))
                ),
            ),
        ] {
            assert!(Identity::read(&uri, &bad).is_err(), "{bad}");
        }
        for bad in [format!("{uri}&sn=4"), uri.replace("sid=29", "sid=unknown")] {
            assert!(Identity::read(&bad, &metadata).is_err());
        }
        let identity = Identity::read(&uri, &metadata).unwrap().unwrap();
        let foreign = metadata.replace("observed-household-account", "another-account");
        assert!(identity.repair(&uri, &foreign).is_err());
    }

    #[test]
    fn missing_service_evidence_remains_eligible_for_household_fallback() {
        let params = params();
        let uri = spotify_track_uri(TRACK, &params);
        let metadata = spotify_track_didl(TRACK, "Observed", &params);
        for incomplete in [
            String::new(),
            metadata.replace("10032028spotify%3atrack%3aObservedTrack", "Q:0/1"),
            metadata.replace("id=\"cdudn\"", "id=\"unrelated\""),
            metadata.replace(
                "10032028spotify%3atrack%3aObservedTrack",
                "spotify%3atrack%3aObservedTrack",
            ),
        ] {
            assert!(Identity::read(&uri, &incomplete).unwrap().is_none());
        }
    }
}
