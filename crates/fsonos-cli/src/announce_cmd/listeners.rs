//! Direct announcements serve each player from its own observed LAN route.

use super::{Announcements, ErrorCode, Failure, MediaFiles, MediaStore};
use fsonos_core::HouseholdState;
use fsonos_proto::ProtoError;
use fsonos_proto::net::EventSink;
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

pub(super) struct Listeners {
    // All listeners outlive delivery and restoration, including for parallel
    // households. The route map alone must never keep advertising dead sockets.
    _sinks: Vec<EventSink>,
    routes: Arc<HashMap<PlayerId, String>>,
}

impl Listeners {
    pub(super) fn start(
        households: &[HouseholdState],
        media: &MediaFiles,
        local_address: impl Fn(IpAddr) -> Result<IpAddr, ProtoError>,
    ) -> Result<Self, Failure> {
        let mut sinks = Vec::new();
        let mut addresses: HashMap<IpAddr, Result<String, String>> = HashMap::new();
        let mut routes = HashMap::new();
        let mut unavailable = None;
        for player in households.iter().flat_map(|household| &household.players) {
            let route = local_address(player.ip)
                .map_err(|error| error.to_string())
                .and_then(|local| {
                    addresses
                        .entry(local)
                        .or_insert_with(|| {
                            EventSink::start_serving(
                                SocketAddr::new(local, 0),
                                Some(Arc::clone(media)),
                            )
                            .map(|sink| {
                                let base = sink.callback_url("").trim_end_matches('/').to_owned();
                                sinks.push(sink);
                                base
                            })
                            .map_err(|error| error.to_string())
                        })
                        .clone()
                });
            match route {
                Ok(base) => {
                    routes.insert(player.id.clone(), base);
                }
                Err(error) => {
                    tracing::warn!(player = %player.id.0, %error, "announcement media route unavailable");
                    unavailable = Some(error);
                }
            }
        }
        if routes.is_empty() {
            return Err(Failure::new(
                ErrorCode::NotReady,
                unavailable.map_or_else(
                    || "no player to serve the clip toward".to_owned(),
                    |error| format!("no announcement media listener could start: {error}"),
                ),
            ));
        }
        Ok(Self {
            _sinks: sinks,
            routes: Arc::new(routes),
        })
    }

    pub(super) fn announcements(&self, media: MediaStore) -> Announcements {
        let ready = Arc::clone(&self.routes);
        let routes = Arc::clone(&self.routes);
        Announcements::new(media, Box::new(move || ready.values().next().cloned()))
            .with_media_route(Box::new(move |player| routes.get(player).cloned()))
    }
}

#[cfg(test)]
mod tests;
