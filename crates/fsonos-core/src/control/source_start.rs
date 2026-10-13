//! Complete a direct source start when a reply, rather than its effect, is
//! lost. Each write is sent once. Read-back must establish the requested
//! source and coordinator before advancing; a Play reply also needs Playing.
//! Definite refusals keep their meaning for the caller's existing recovery.

use fsonos_proto::control::{self as soap, MediaInfo};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

struct Source<'a> {
    uri: &'a str,
    metadata: &'a str,
}

impl Source<'_> {
    fn matches(&self, media: &MediaInfo) -> bool {
        // Do not collapse renderer parameters to a track ID: the same track
        // with another account's metadata is not proof of this request.
        media.uri == self.uri
            && (self.metadata.trim().is_empty()
                || media.uri_metadata.trim() == self.metadata.trim())
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Source,
    Play,
}

pub(super) fn play<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    uri: &str,
    metadata: &str,
) -> Result<(), ProtoError> {
    if uri.trim().is_empty() {
        return Err(ProtoError::Malformed("cannot start an empty source URI".into()));
    }
    let source = Source { uri, metadata };
    for stage in [Stage::Source, Stage::Play] {
        let result = match stage {
            Stage::Source => soap::set_av_transport_uri(transport, host, uri, metadata),
            Stage::Play => soap::play(transport, host),
        };
        if let Err(error) = result {
            let ambiguous = matches!(
                &error,
                ProtoError::Network { .. } | ProtoError::Malformed(_)
            );
            if !ambiguous
                || !confirmed(transport, host, coordinator, &source, stage).unwrap_or(false)
            {
                // Keep the original failure, not the read-back error. Never
                // reset the source, repeat Play, or roll back another app.
                return Err(error);
            }
            tracing::warn!(cause = %error, "source start confirmed without replaying its write");
        }
    }
    Ok(())
}

/// A bounded set of reads, not a transaction with other controllers. Check
/// the source again after reading topology/transport so a switch during that
/// interval cannot authorize the next write or turn a failure into success.
fn confirmed<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    source: &Source<'_>,
    stage: Stage,
) -> Result<bool, ProtoError> {
    if !source.matches(&soap::get_media_info(transport, host)?) {
        return Ok(false);
    }
    if !get_zone_group_state(transport, host)?
        .groups
        .iter()
        .any(|group| group.coordinator == *coordinator)
    {
        return Ok(false);
    }
    if matches!(stage, Stage::Play)
        && soap::get_transport_info(transport, host)?.state != TransportState::Playing
    {
        return Ok(false);
    }
    Ok(source.matches(&soap::get_media_info(transport, host)?))
}
