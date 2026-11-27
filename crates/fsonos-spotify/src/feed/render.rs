//! Recover one definitively refused movement from this household's observed
//! track accounts. The queue proof owns every send, including the one retry.

use fsonos_proto::content::browse_all;
use fsonos_proto::didl::{
    SpotifyRenderParams, spotify_queue_uri, spotify_track_didl, spotify_track_templates,
};
use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

use super::{FeedError, Movement, enqueue};
use fsonos_core::CoreError;

/// Add one movement, retaining an accepted replacement template for the rest
/// of this work/batch. A definite AddURIToQueue/800 permits one new favorites
/// read and at most one retry, only after the owner and unchanged queue have
/// been confirmed. An uncertain write is never replayed.
pub(super) fn add_movement<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    params: &mut SpotifyRenderParams,
    movement: &Movement,
    desired: u32,
    proof: &mut enqueue::Batch,
    admit: &dyn Fn() -> Result<(), FeedError>,
) -> Result<(u32, Option<u32>), FeedError> {
    let metadata = spotify_track_didl(&movement.uri, &movement.title, params);
    let mut refused = false;
    let result = proof.confirmed(transport, host, &movement.uri, desired, admit, || {
        let result = send(transport, host, movement, &metadata, desired);
        refused = matches!(
            &result,
            Err(FeedError::Proto(ProtoError::SoapFault { code: 800, .. }))
        );
        result
    });
    if !refused || !is_refusal(&result) {
        return result;
    }
    if !proof.retains_refused_queue() {
        return classify_refusal(result, refused);
    }
    let favorites = browse_all(transport, host, "FV:2")?;
    let templates = spotify_track_templates(&favorites);
    // Queue URIs carry neither flags nor account numbers. Compare the actual
    // DIDL so an equivalent payload cannot consume the one recovery attempt.
    let replacement = templates
        .iter()
        .find(|candidate| spotify_track_didl(&movement.uri, &movement.title, candidate) != metadata)
        .or_else(|| templates.first());
    let Some(replacement) = replacement else {
        return classify_refusal(result, refused);
    };
    let metadata = spotify_track_didl(&movement.uri, &movement.title, replacement);
    // A prerequisite for the retry can itself return 800. Only the actual
    // AddURIToQueue result can identify this attempt as a renderer refusal.
    refused = false;
    let result = proof.confirmed(transport, host, &movement.uri, desired, admit, || {
        let result = send(transport, host, movement, &metadata, desired);
        refused = is_refusal(&result);
        result
    });
    if result.is_ok() {
        *params = replacement.clone();
    }
    classify_refusal(result, refused)
}

fn is_refusal(result: &Result<(u32, Option<u32>), FeedError>) -> bool {
    matches!(
        result,
        Err(FeedError::Proto(ProtoError::SoapFault { code: 800, .. }))
    )
}

/// A send's refusal must survive the queue/owner read-back unchanged before
/// it can become a credential diagnosis. Read failures never qualify, even
/// when a speaker uses the same UPnP code for them.
fn classify_refusal(
    result: Result<(u32, Option<u32>), FeedError>,
    refused: bool,
) -> Result<(u32, Option<u32>), FeedError> {
    if refused && is_refusal(&result) {
        Err(CoreError::RenderParamsStale.into())
    } else {
        result
    }
}

fn send<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    movement: &Movement,
    metadata: &str,
    desired: u32,
) -> Result<(u32, Option<u32>), FeedError> {
    let response = soap::call(
        transport,
        host,
        &AV_TRANSPORT,
        "AddURIToQueue",
        &soap::args_xml(&[
            ("InstanceID", "0"),
            ("EnqueuedURI", &spotify_queue_uri(&movement.uri)),
            ("EnqueuedURIMetaData", metadata),
            ("DesiredFirstTrackNumberEnqueued", &desired.to_string()),
            ("EnqueueAsNext", "0"),
        ]),
    )?;
    let position = response.require_u32("FirstTrackNumberEnqueued")?;
    let count = response.require_u32("NumTracksAdded")?;
    let len = response
        .get("NewQueueLength")
        .map(|n| {
            n.trim().parse::<u32>().map_err(|_| {
                ProtoError::Malformed("non-numeric NewQueueLength in enqueue reply".into())
            })
        })
        .transpose()?;
    if position == 0 || count != 1 || len.is_some_and(|n| n < position) {
        return Err(ProtoError::Malformed(
            "inconsistent counters in single-movement enqueue reply".into(),
        )
        .into());
    }
    Ok((position, len))
}
