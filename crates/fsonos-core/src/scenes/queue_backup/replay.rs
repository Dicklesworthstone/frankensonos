//! Replay identities observed on this queue and its currently selected track.
//! No service prefix or linked account is inferred from a Spotify track ID.

use super::{QueueReference, invalid, queue};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    DidlDesc, DidlKind, NativeDidlObject, native_account_descriptor, parse_didl,
    spotify_uri_from_renderer_uri, with_native_replay_identity,
};
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Identity {
    sid: u32,
    sn: u32,
    prefix: String,
    descriptor: DidlDesc,
}

fn account(uri: &str) -> Result<Option<(u32, u32)>, ProtoError> {
    let Some((_, query)) = uri.split_once('?') else {
        return Ok(None);
    };
    let number = |name: &str| {
        let mut values = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .filter_map(|(key, value)| (key == name).then_some(value));
        let Some(value) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some() {
            return Err(invalid(
                "a Spotify resource has ambiguous account parameters",
            ));
        }
        value
            .parse::<u32>()
            .map(Some)
            .map_err(|_| invalid("a Spotify resource has invalid account parameters"))
    };
    Ok(number("sid")?.zip(number("sn")?))
}

impl Identity {
    /// A renderer/service item ID must name this exact track. Queue/catalog
    /// occurrence IDs do not reveal a service prefix and remain incomplete.
    /// Resource metadata, when present, must retain the same track/account.
    pub(super) fn read(uri: &str, metadata: &str) -> Result<Option<Self>, ProtoError> {
        let Some(spotify) = spotify_uri_from_renderer_uri(uri)
            .filter(|source| source.starts_with("spotify:track:"))
        else {
            return Ok(None);
        };
        if metadata.is_empty() {
            return Ok(None);
        }
        let descriptor = native_account_descriptor(metadata)?;
        let mut objects = parse_didl(metadata)?;
        if objects.len() != 1 {
            return Err(invalid("Spotify replay metadata needs exactly one item"));
        }
        let object = objects.remove(0);
        if object.kind != DidlKind::Item || !object.class.starts_with("object.item.audioItem") {
            return Err(invalid(
                "Spotify replay metadata does not describe an audio item",
            ));
        }
        let linked = account(uri)?;
        if let Some(resource) = &object.res
            && (spotify_uri_from_renderer_uri(&resource.uri).as_ref() != Some(&spotify)
                || account(&resource.uri)? != linked)
        {
            return Err(invalid(
                "Spotify replay metadata names a different resource or account",
            ));
        }
        let lower = object.id.to_ascii_lowercase();
        let Some(start) = lower.find("spotify%3a").or_else(|| lower.find("spotify:")) else {
            return Ok(None);
        };
        let item_uri = format!("x-sonos-spotify:{}", &object.id[start..]);
        if spotify_uri_from_renderer_uri(&item_uri).as_ref() != Some(&spotify) {
            return Err(invalid(
                "Spotify replay item identity names a different track",
            ));
        }
        let prefix = &object.id[..start];
        if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let Some(descriptor) = descriptor else {
            return Ok(None);
        };
        if descriptor.value.trim().is_empty() || descriptor.name_space.trim().is_empty() {
            return Err(invalid("Spotify replay account descriptor is incomplete"));
        }
        let Some((sid, sn)) = linked else {
            return Ok(None);
        };
        Ok(Some(Self {
            sid,
            sn,
            prefix: prefix.into(),
            descriptor,
        }))
    }

    fn repair(&self, uri: &str, metadata: &str) -> Result<Option<String>, ProtoError> {
        if account(uri)? != Some((self.sid, self.sn)) {
            return Ok(None);
        }
        let Some(spotify) = spotify_uri_from_renderer_uri(uri)
            .filter(|source| source.starts_with("spotify:track:"))
        else {
            return Ok(None);
        };
        let id = format!("{}{}", self.prefix, spotify.replace(':', "%3a"));
        with_native_replay_identity(metadata, &id, "-1", &self.descriptor).map(Some)
    }
}

pub(super) struct Current {
    identity: Option<Identity>,
    group: queue::Group,
    uri: String,
}

impl Current {
    /// Bind Spotify queue resources to the selected renderer track even when
    /// their native metadata is complete. Read its replay identity only when
    /// an incomplete occurrence needs it; absent TrackMetaData does not weaken
    /// the source, selection, account and physical-owner proof.
    pub(super) fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        reference: &QueueReference,
        items: &[NativeDidlObject],
    ) -> Result<Option<Self>, ProtoError> {
        let mut spotify_queue = false;
        let mut needs_evidence = false;
        for native in items {
            let Some(resource) = &native.object.res else {
                continue;
            };
            if spotify_uri_from_renderer_uri(&resource.uri)
                .is_some_and(|source| source.starts_with("spotify:track:"))
            {
                spotify_queue = true;
                needs_evidence |= Identity::read(&resource.uri, &native.metadata)?.is_none();
            }
        }
        if !spotify_queue {
            return Ok(None);
        }
        let selected = reference
            .track
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| items.get(index))
            .and_then(|item| item.object.res.as_ref())
            .ok_or_else(|| invalid("the selected queue occurrence is missing"))?;
        Self::observe(t, host, reference, &selected.uri, needs_evidence).map(Some)
    }

    fn observe<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        reference: &QueueReference,
        expected_uri: &str,
        read_identity: bool,
    ) -> Result<Self, ProtoError> {
        let group = queue::group(t, host, &reference.coordinator)?;
        let source = format!("x-rincon-queue:{}#0", reference.coordinator.0);
        if group.boot_seq != reference.boot_seq || soap::get_media_info(t, host)?.uri != source {
            return Err(invalid(
                "the queue owner changed before reading replay metadata",
            ));
        }
        let (position, metadata) = soap::get_position_info_with_metadata(t, host)?;
        if position.track != reference.track
            || !queue::same_track(&position.uri, expected_uri)
            || (spotify_uri_from_renderer_uri(expected_uri).is_some()
                && account(&position.uri)? != account(expected_uri)?)
            || soap::get_media_info(t, host)?.uri != source
        {
            return Err(invalid(
                "the selected queue track changed while reading replay metadata",
            ));
        }
        let identity = if read_identity {
            Identity::read(&position.uri, &metadata)?
        } else {
            None
        };
        Ok(Self {
            identity,
            group,
            uri: position.uri,
        })
    }

    pub(super) fn repair(&self, uri: &str, metadata: &str) -> Result<Option<String>, ProtoError> {
        let Some(identity) = &self.identity else {
            return Ok(None);
        };
        identity.repair(uri, metadata)
    }

    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        reference: &QueueReference,
    ) -> Result<(), ProtoError> {
        let current = Self::observe(t, host, reference, &self.uri, self.identity.is_some())?;
        if current.group != self.group || current.identity != self.identity {
            return Err(invalid(
                "the selected track's replay identity changed while saving",
            ));
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
