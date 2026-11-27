//! Learn a missing renderer template from the requested group's native track.
//! Favorites keep their established preference order. The fallback reads only
//! this group's coordinator and retains its source, account and physical group.

use super::spotify_source;
use crate::queue_source::native::replay::Identity;
use crate::{CoreError, topology};
use fsonos_proto::Transport;
use fsonos_proto::content;
use fsonos_proto::control as soap;
use fsonos_proto::didl::{SpotifyContainer, SpotifyRenderParams, spotify_track_templates};
use fsonos_proto::topology::{ZoneGroupInfo, ZoneGroupState};
use fsonos_types::PlayerId;
use std::net::IpAddr;

pub(super) fn templates<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    player: &PlayerId,
) -> Result<Vec<SpotifyRenderParams>, CoreError> {
    let favorites = content::browse_all(transport, host, "FV:2")?;
    let mut templates = spotify_track_templates(&favorites);
    if templates.is_empty()
        && let Some(observed) = current(transport, host, player)?
    {
        templates.push(observed);
    }
    Ok(templates)
}

pub(super) fn container<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    player: &PlayerId,
    kind: SpotifyContainer,
    source: &str,
    title: &str,
    rejected: Option<&(String, String)>,
) -> Result<Option<(String, String)>, CoreError> {
    let favorites = content::browse_all(transport, host, "FV:2")?;
    let learned = match rejected {
        Some(old) => spotify_source::after_rejection(&favorites, kind, source, title, old),
        None => spotify_source::container(&favorites, kind, source, title),
    };
    if learned.is_some() {
        return Ok(learned);
    }
    Ok(current(transport, host, player)?
        .map(|params| spotify_source::from_track(&params, kind, source, title)))
}

fn changed() -> CoreError {
    CoreError::TargetChanged(
        "the group's native Spotify source changed while learning its renderer settings".into(),
    )
}

fn group_at<'a>(
    topology: &'a ZoneGroupState,
    host: IpAddr,
    player: &PlayerId,
) -> Result<&'a ZoneGroupInfo, CoreError> {
    topology::validate(topology)?;
    topology
        .groups
        .iter()
        .find(|group| {
            group
                .members
                .iter()
                .flat_map(|member| std::iter::once(member).chain(&member.satellites))
                .any(|member| {
                    member.uuid == *player && member.ip() == Some(host) && !member.is_zone_bridge
                })
        })
        .ok_or_else(changed)
}

fn current_source(
    uri: &str,
    coordinator: &PlayerId,
    track: u32,
    metadata: &str,
    params: &SpotifyRenderParams,
) -> Result<bool, CoreError> {
    if uri == format!("x-rincon-queue:{}#0", coordinator.0) {
        return Ok(track > 0);
    }
    Ok(Identity::render_params(uri, metadata)?.as_ref() == Some(params))
}

fn current<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    player: &PlayerId,
) -> Result<Option<SpotifyRenderParams>, CoreError> {
    let initial = topology::read_at(transport, host, player)?;
    let group = group_at(&initial, host, player)?;
    let owner = group
        .members
        .iter()
        .find(|member| member.uuid == group.coordinator && !member.is_zone_bridge)
        .ok_or_else(changed)?;
    let source_host = owner.ip().ok_or_else(changed)?;
    if source_host != host {
        let source_topology = topology::read_at(transport, source_host, &group.coordinator)?;
        if group_at(&source_topology, source_host, &group.coordinator)? != group {
            return Err(changed());
        }
    }
    let before_source = soap::get_media_info(transport, source_host)?;
    let (before, metadata) = soap::get_position_info_with_metadata(transport, source_host)?;
    let Some(params) = Identity::render_params(&before.uri, &metadata)? else {
        return Ok(None);
    };
    // PositionInfo may retain an old track after selecting a stream or
    // another group's queue. The current source must own this observation.
    if !current_source(
        &before_source.uri,
        &group.coordinator,
        before.track,
        &metadata,
        &params,
    )? {
        return Ok(None);
    }
    let after_source = soap::get_media_info(transport, source_host)?;
    let (after, metadata) = soap::get_position_info_with_metadata(transport, source_host)?;
    if after_source.uri != before_source.uri
        || after.track != before.track
        || after.uri != before.uri
        || Identity::render_params(&after.uri, &metadata)?.as_ref() != Some(&params)
    {
        return Err(changed());
    }
    let final_topology = topology::read_at(transport, source_host, &group.coordinator)?;
    if group_at(&final_topology, source_host, &group.coordinator)? != group {
        return Err(changed());
    }
    Ok(Some(params))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl, spotify_track_uri};

    fn params() -> SpotifyRenderParams {
        SpotifyRenderParams {
            sid: 29,
            flags: 8232,
            sn: 7,
            cdudn: "observed-test-account".into(),
            item_id_prefix: "10032028".into(),
        }
    }

    #[test]
    fn current_track_templates_keep_one_observed_account_prefix_and_flags() {
        let params = params();
        let source = "spotify:track:ObservedTrack";
        let uri = spotify_track_uri(source, &params);
        let metadata = spotify_track_didl(source, "Observed", &params);
        assert_eq!(
            Identity::render_params(&uri, &metadata).unwrap(),
            Some(params)
        );
        assert_eq!(
            Identity::render_params(&spotify_queue_uri(source), &metadata).unwrap(),
            None
        );
        assert_eq!(Identity::render_params(&uri, "").unwrap(), None);
    }

    #[test]
    fn incomplete_and_conflicting_current_templates_are_not_guessed() {
        let params = params();
        let source = "spotify:track:ObservedTrack";
        let uri = spotify_track_uri(source, &params);
        let metadata = spotify_track_didl(source, "Observed", &params);
        assert_eq!(
            Identity::render_params(&uri.replace("&flags=8232", ""), &metadata).unwrap(),
            None
        );
        for bad in [
            format!("{uri}&flags=8232"),
            uri.replace("flags=8232", "flags=+8232"),
            uri.replace("flags=8232", "flags=unknown"),
            uri.replace("flags=8232", "flags=4294967296"),
        ] {
            assert!(Identity::render_params(&bad, &metadata).is_err(), "{bad}");
        }
        for bad in [
            metadata.replace("ObservedTrack", "DifferentTrack"),
            metadata.replace(
                "</item>",
                "<desc id='cdudn'>different-account</desc></item>",
            ),
        ] {
            assert!(Identity::render_params(&uri, &bad).is_err());
        }
    }

    #[test]
    fn current_source_must_own_the_native_track_observation() {
        let params = params();
        let coordinator = PlayerId("RINCON_OBSERVED".into());
        let source = "spotify:track:ObservedTrack";
        let uri = spotify_track_uri(source, &params);
        let metadata = spotify_track_didl(source, "Observed", &params);
        assert!(current_source(&uri, &coordinator, 0, &metadata, &params).unwrap());
        assert!(
            current_source(
                "x-rincon-queue:RINCON_OBSERVED#0",
                &coordinator,
                1,
                &metadata,
                &params,
            )
            .unwrap()
        );
        for (source, track) in [
            ("x-rincon-queue:RINCON_OBSERVED#0", 0),
            ("x-rincon-queue:RINCON_OTHER#0", 1),
            ("http://example.invalid/other.flac", 1),
            ("x-rincon:RINCON_OTHER", 1),
        ] {
            assert!(!current_source(source, &coordinator, track, &metadata, &params).unwrap());
        }
        assert!(
            current_source(
                &spotify_track_uri("spotify:track:DifferentTrack", &params),
                &coordinator,
                1,
                &metadata,
                &params,
            )
            .is_err()
        );
    }
}
