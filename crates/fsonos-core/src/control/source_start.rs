//! Complete a direct source start when a reply, rather than its effect, is
//! lost. Each write is sent once. Read-back must establish the requested
//! source and coordinator before advancing; a Play reply also needs Playing.
//! Definite refusals keep their meaning for the caller's existing recovery.

use fsonos_proto::control::{self as soap, MediaInfo};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::time::{Duration, Instant};

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
        let detail = "cannot start an empty source URI";
        return Err(ProtoError::Malformed(detail.into()));
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

/// Recheck source identity across topology/transport reads. A transitioning
/// Play may settle, but a changed source, pause, stop or read failure cannot.
fn confirmed<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    source: &Source<'_>,
    stage: Stage,
) -> Result<bool, ProtoError> {
    let unchanged = || {
        if !source.matches(&soap::get_media_info(transport, host)?) {
            return Ok(false);
        }
        let topology = get_zone_group_state(transport, host)?;
        Ok(topology
            .groups
            .iter()
            .any(|group| group.coordinator == *coordinator))
    };
    match stage {
        Stage::Source => {
            Ok(unchanged()? && source.matches(&soap::get_media_info(transport, host)?))
        }
        Stage::Play => wait_for_playing(transport, host, unchanged),
    }
}

/// Shared by direct-source and queue starts. Wait only for TRANSITIONING,
/// never retry a write or a failed read. The proof callback retains the
/// caller's original identity/revision, rather than adopting a changed queue.
/// Five seconds bound further observations and deliberate waits; an in-flight
/// network request remains bounded by the transport's own timeout.
pub(super) fn wait_for_playing<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    unchanged: impl FnMut() -> Result<bool, ProtoError>,
) -> Result<bool, ProtoError> {
    wait_with_budget(transport, host, Duration::from_secs(5), unchanged)
}

fn wait_with_budget<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    budget: Duration,
    mut unchanged: impl FnMut() -> Result<bool, ProtoError>,
) -> Result<bool, ProtoError> {
    let started = Instant::now();
    for attempt in 0..51 {
        if (attempt > 0 && started.elapsed() >= budget) || !unchanged()? {
            return Ok(false);
        }
        match soap::get_transport_info(transport, host)?.state {
            TransportState::Playing => return unchanged(),
            TransportState::Transitioning => {}
            _ => return Ok(false),
        }
        let left = budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100).min(left));
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::net::Ipv4Addr;

    struct Status {
        state: &'static str,
        calls: Cell<u32>,
    }

    impl Status {
        fn new(state: &'static str) -> Self {
            Self {
                state,
                calls: Cell::new(0),
            }
        }
    }

    impl Transport for Status {
        fn soap_post(
            &self,
            _: IpAddr,
            _: &str,
            action: &str,
            _: &str,
        ) -> Result<String, ProtoError> {
            assert!(action.ends_with("#GetTransportInfo\""));
            self.calls.set(self.calls.get() + 1);
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                 <s:Body><u:GetTransportInfoResponse xmlns:u=\"urn:x\">\
                 <CurrentTransportState>{}</CurrentTransportState>\
                 </u:GetTransportInfoResponse></s:Body></s:Envelope>",
                self.state
            ))
        }
    }

    #[test]
    fn exhausted_transition_budget_does_not_start_another_read() {
        let t = Status::new("TRANSITIONING");
        let host = Ipv4Addr::LOCALHOST.into();
        assert!(!wait_with_budget(&t, host, Duration::ZERO, || Ok(true)).unwrap());
        assert_eq!(t.calls.get(), 1);
    }

    #[test]
    fn paused_stopped_and_unknown_states_do_not_wait_for_an_owner_to_resume() {
        for state in ["PAUSED_PLAYBACK", "STOPPED", "UNKNOWN"] {
            let t = Status::new(state);
            let host = Ipv4Addr::LOCALHOST.into();
            assert!(!wait_for_playing(&t, host, || Ok(true)).unwrap());
            assert_eq!(t.calls.get(), 1);
        }
    }

    #[test]
    fn a_changed_identity_prevents_even_the_transport_read() {
        let t = Status::new("PLAYING");
        let host = Ipv4Addr::LOCALHOST.into();
        assert!(!wait_for_playing(&t, host, || Ok(false)).unwrap());
        assert_eq!(t.calls.get(), 0);
    }

    #[test]
    fn read_errors_propagate_without_retrying() {
        let t = Status::new("PLAYING");
        let host = Ipv4Addr::LOCALHOST.into();
        let result = wait_for_playing(&t, host, || {
            Err(ProtoError::Malformed("proof unavailable".into()))
        });
        assert!(result.is_err());
        assert_eq!(t.calls.get(), 0);
    }

    #[test]
    fn playing_requires_identity_to_match_on_both_sides_of_the_read() {
        for stable in [false, true] {
            let t = Status::new("PLAYING");
            let host = Ipv4Addr::LOCALHOST.into();
            let mut proofs = 0;
            let result = wait_for_playing(&t, host, || {
                proofs += 1;
                Ok(stable || proofs == 1)
            });
            assert_eq!(result.unwrap(), stable);
            assert_eq!(proofs, 2);
            assert_eq!(t.calls.get(), 1);
        }
    }
}
