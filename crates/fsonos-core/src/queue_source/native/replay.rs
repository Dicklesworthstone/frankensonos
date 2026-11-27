//! Re-enqueue native queue occurrences within their observed household.
//! Keep service-owned metadata and repair only missing Spotify identities from
//! the current renderer or the household's own favorites. Cross-household
//! transfers must render for their destination instead of using this module.

use crate::topology::read_at;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    DidlDesc, DidlKind, NativeDidlObject, SpotifyRenderParams, native_account_descriptor,
    parse_didl, spotify_track_didl, spotify_uri_from_renderer_uri, with_native_replay_identity,
};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

fn invalid(detail: impl std::fmt::Display) -> ProtoError {
    ProtoError::Malformed(format!("native queue replay refused: {detail}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
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
    pub(crate) fn read(uri: &str, metadata: &str) -> Result<Option<Self>, ProtoError> {
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

    pub(crate) fn repair(&self, uri: &str, metadata: &str) -> Result<Option<String>, ProtoError> {
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

/// Current renderer evidence used to repair incomplete native queue metadata.
/// Membership is deliberately left to the caller: a move may intentionally
/// detach rooms while the original coordinator, boot and selection stay owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selected {
    coordinator: PlayerId,
    track: u32,
    uri: String,
    pub(crate) boot_seq: Option<u32>,
    identity: Option<Identity>,
}

fn boot<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<Option<u32>, ProtoError> {
    let topology = read_at(t, host, coordinator)?;
    if !crate::topology::coordinator_at(&topology, host, coordinator)? {
        return Err(invalid(
            "the queue player no longer owns its control address",
        ));
    }
    Ok(topology
        .boot_seqs()
        .find_map(|(id, boot)| (id == coordinator).then_some(boot)))
}

impl Selected {
    /// Observe the current queue track for a partial occurrence selection.
    /// The selected items need not include this occurrence, and native replay
    /// needs no renderer flags: only the matching service/account identity.
    pub(crate) fn current<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Option<Self>, ProtoError> {
        let source = format!("x-rincon-queue:{}#0", coordinator.0);
        if soap::get_media_info(t, host)?.uri != source {
            return Ok(None);
        }
        let (position, _) = soap::get_position_info_with_metadata(t, host)?;
        if position.track == 0
            || !spotify_uri_from_renderer_uri(&position.uri)
                .is_some_and(|uri| uri.starts_with("spotify:track:"))
        {
            return Ok(None);
        }
        Self::observe(t, host, coordinator, position.track, &position.uri, true)
            .map(|current| current.identity.is_some().then_some(current))
    }

    pub(crate) fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        track: u32,
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
        let selected = track
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| items.get(index))
            .and_then(|item| item.object.res.as_ref())
            .ok_or_else(|| invalid("the selected queue occurrence is missing"))?;
        Self::observe(t, host, coordinator, track, &selected.uri, needs_evidence).map(Some)
    }

    fn observe<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        track: u32,
        expected_uri: &str,
        read_identity: bool,
    ) -> Result<Self, ProtoError> {
        let boot_seq = boot(t, host, coordinator)?;
        let source = format!("x-rincon-queue:{}#0", coordinator.0);
        if soap::get_media_info(t, host)?.uri != source {
            return Err(invalid("the source changed before reading replay metadata"));
        }
        let (position, metadata) = soap::get_position_info_with_metadata(t, host)?;
        let same_track = position.uri == expected_uri
            || spotify_uri_from_renderer_uri(expected_uri).is_some_and(|expected| {
                spotify_uri_from_renderer_uri(&position.uri).as_ref() == Some(&expected)
            });
        if position.track != track
            || !same_track
            || (spotify_uri_from_renderer_uri(expected_uri).is_some()
                && account(&position.uri)? != account(expected_uri)?)
            || soap::get_media_info(t, host)?.uri != source
            || boot(t, host, coordinator)? != boot_seq
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
            coordinator: coordinator.clone(),
            track,
            uri: position.uri,
            boot_seq,
            identity,
        })
    }

    pub(crate) fn repair(&self, uri: &str, metadata: &str) -> Result<Option<String>, ProtoError> {
        self.identity
            .as_ref()
            .map_or(Ok(None), |identity| identity.repair(uri, metadata))
    }

    pub(crate) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
    ) -> Result<(), ProtoError> {
        let current = Self::observe(
            t,
            host,
            &self.coordinator,
            self.track,
            &self.uri,
            self.identity.is_some(),
        )?;
        if current.boot_seq != self.boot_seq || current.identity != self.identity {
            return Err(invalid("the selected track's replay identity changed"));
        }
        Ok(())
    }
}

/// An already-selected direct renderer source may be reused within its own
/// household. Its account-bearing URI and metadata remain the authority.
pub(crate) struct Direct {
    coordinator: PlayerId,
    host: IpAddr,
    boot_seq: Option<u32>,
    uri: String,
    metadata: String,
}

impl Direct {
    pub(crate) fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
        uri: &str,
        metadata: &str,
    ) -> Result<Self, ProtoError> {
        let origin = Self {
            coordinator: coordinator.clone(),
            host,
            boot_seq: boot(t, host, coordinator)?,
            uri: uri.into(),
            metadata: metadata.into(),
        };
        origin.confirm(t)?;
        Ok(origin)
    }

    pub(crate) fn confirm<T: Transport + ?Sized>(&self, t: &T) -> Result<(), ProtoError> {
        let media = soap::get_media_info(t, self.host)?;
        if media.uri != self.uri
            || !(media.uri_metadata == self.metadata
                || fsonos_proto::didl::equivalent_metadata(&media.uri_metadata, &self.metadata))
            || boot(t, self.host, &self.coordinator)? != self.boot_seq
        {
            return Err(invalid(
                "the direct source, account metadata or player boot changed",
            ));
        }
        Ok(())
    }
}

pub(crate) struct Item {
    pub(crate) uri: String,
    pub(crate) metadata: String,
}

pub(crate) fn playable(uri: &str) -> bool {
    !uri.trim().is_empty()
        && uri != "NOT_IMPLEMENTED"
        && !uri.chars().any(char::is_control)
        && !["x-rincon:", "x-rincon-queue:", "x-rincon-cpcontainer:"]
            .iter()
            .any(|prefix| uri.starts_with(prefix))
        && spotify_uri_from_renderer_uri(uri).is_none_or(|source| {
            source.starts_with("spotify:track:") || source.starts_with("spotify:episode:")
        })
}

pub(crate) fn item<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    native: NativeDidlObject,
    parameters: &mut Option<SpotifyRenderParams>,
    current: Option<&Selected>,
) -> Result<Item, CoreError> {
    let NativeDidlObject {
        object,
        mut metadata,
    } = native;
    let uri = object
        .res
        .as_ref()
        .map(|res| res.uri.clone())
        .ok_or_else(|| invalid("a queue occurrence has no resource"))?;
    if object.kind != DidlKind::Item
        || !object.class.starts_with("object.item.audioItem")
        || !playable(&uri)
    {
        return Err(invalid("a queue occurrence is not an expanded playable audio item").into());
    }
    // Retain complete native replay metadata verbatim. Queue Browse commonly
    // omits its service identity/account; the selected track's own renderer
    // metadata can supply those without requiring a matching My Sonos favorite.
    // An incomplete observation still falls back to this household's favorites.
    if let Some(spotify) = spotify_uri_from_renderer_uri(&uri)
        && spotify.starts_with("spotify:track:")
    {
        if Identity::read(&uri, &metadata)?.is_some() {
            return Ok(Item { uri, metadata });
        }
        if let Some(current) = current
            && let Some(repaired) = current.repair(&uri, &metadata)?
        {
            return Ok(Item {
                uri,
                metadata: repaired,
            });
        }
        if parameters.is_none() {
            *parameters = Some(
                control::spotify_params(t, households, coordinator)?.ok_or_else(|| {
                    invalid("Spotify queue entries have no discoverable account metadata")
                })?,
            );
        }
        let replay_metadata = spotify_track_didl(
            &spotify,
            &object.title,
            parameters.as_ref().expect("parameters resolved above"),
        );
        let replay = parse_didl(&replay_metadata)?
            .into_iter()
            .next()
            .ok_or_else(|| invalid("Spotify replay metadata is missing"))?;
        let params = parameters.as_ref().expect("parameters resolved above");
        let same_account = uri.split_once('?').is_none_or(|(_, query)| {
            let number = |name: &str| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
                    .and_then(|value| value.parse::<u32>().ok())
            };
            number("sid") == Some(params.sid) && number("sn") == Some(params.sn)
        });
        if !same_account {
            return Err(invalid(
                "a Spotify queue occurrence belongs to an unresolved linked account",
            )
            .into());
        }
        // Queue IDs need the renderer's service prefix. Repair only that
        // identity and a missing account; native extensions remain intact.
        metadata = with_native_replay_identity(
            &metadata,
            &replay.id,
            &replay.parent_id,
            replay
                .desc
                .as_ref()
                .ok_or_else(|| invalid("Spotify replay account is missing"))?,
        )?;
    }
    Ok(Item { uri, metadata })
}
