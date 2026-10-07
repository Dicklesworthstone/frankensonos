//! The speakers a surface acts on, shared by the HTTP API and the MCP tools.
//!
//! A [`Surface`] holds the LAN transport, how to find the households (a
//! survey, cached for a short while), the house policy, and the clock. Each
//! call names its [`Client`], so one surface serves callers with different
//! identities (local agents, tailnet principals, unknown peers).

use fsonos_core::clock::Clock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_types::TransportState;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::execute::{OutcomeDto, execute_guarded};
use crate::failure::{ErrorCode, Failure};
use crate::guard::Guard;
use crate::plan::{Command, resolve};
use crate::zones::{ZoneDto, zone_for_target, zone_views};

/// Finds the households (a LAN survey, say).
pub type Survey = Box<dyn Fn(&dyn Transport) -> Result<Vec<HouseholdState>, Failure> + Send + Sync>;

/// How long a survey answer is reused before the next call surveys again.
pub const REFRESH: Duration = Duration::from_secs(30);

/// See the module docs.
pub struct Surface {
    transport: Box<dyn Transport + Send + Sync>,
    survey: Survey,
    cache: Mutex<Option<(Instant, Vec<HouseholdState>)>>,
    policy: Policy,
    clock: Box<dyn Clock>,
}

impl Surface {
    #[must_use]
    pub fn new(
        transport: Box<dyn Transport + Send + Sync>,
        survey: Survey,
        policy: Policy,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            transport,
            survey,
            cache: Mutex::new(None),
            policy,
            clock,
        }
    }

    fn guard<'a>(&'a self, client: &'a Client) -> Guard<'a> {
        Guard {
            policy: &self.policy,
            client,
            clock: &*self.clock,
        }
    }

    /// The households, from the last survey while it is fresh and found
    /// rooms.
    pub fn households(&self) -> Result<Vec<HouseholdState>, Failure> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "household cache poisoned"))?;
        if let Some((at, households)) = cache.as_ref()
            && at.elapsed() < REFRESH
            && households.iter().any(|h| !h.rooms.is_empty())
        {
            return Ok(households.clone());
        }
        let households = (self.survey)(&*self.transport)?;
        *cache = Some((Instant::now(), households.clone()));
        Ok(households)
    }

    /// A control call: authorize `tool` for `client`, plan against the
    /// households, and carry it out under the policy.
    pub fn control(
        &self,
        client: &Client,
        tool: &str,
        plan: impl FnOnce(&[HouseholdState]) -> Result<Command, Failure>,
    ) -> Result<OutcomeDto, Failure> {
        let guard = self.guard(client);
        guard.authorize(tool, false)?;
        let households = self.households()?;
        let command = plan(&households)?;
        execute_guarded(&*self.transport, &households, &guard, command)
    }

    /// Every zone with its live transport state (`unknown` when a
    /// coordinator does not answer).
    pub fn zones(&self, client: &Client) -> Result<Vec<ZoneDto>, Failure> {
        self.guard(client).authorize("list_zones", true)?;
        let households = self.households()?;
        if households.iter().all(|h| h.rooms.is_empty()) {
            return Err(Failure::new(ErrorCode::NotReady, "no Sonos rooms answered"));
        }
        Ok(zone_views(&households, |c| {
            self.transport_state(&households, c)
        }))
    }

    /// The zone `room` plays in.
    pub fn zone(&self, client: &Client, room: &str) -> Result<ZoneDto, Failure> {
        self.guard(client).authorize("get_zone", true)?;
        let households = self.households()?;
        let target = resolve(&households, room)?;
        Ok(zone_for_target(&households, &target, |c| {
            self.transport_state(&households, c)
        }))
    }

    fn transport_state(
        &self,
        households: &[HouseholdState],
        coordinator: &fsonos_types::PlayerId,
    ) -> TransportState {
        control::playback(&*self.transport, households, coordinator)
            .map_or(TransportState::Unknown, |p| p.transport.state)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A [`Surface`] over a canned transport and fixed households, for the
    //! HTTP and MCP surfaces' tests.

    use super::*;
    use fsonos_core::clock::SystemClock;
    use fsonos_proto::ProtoError;
    use std::net::IpAddr;
    use std::sync::Arc;

    /// Answers every SOAP action with success and `out_args`, recording the
    /// action names.
    pub struct Canned {
        pub out_args: &'static str,
        pub sent: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for Canned {
        fn soap_post(
            &self,
            _: IpAddr,
            _: &str,
            action: &str,
            _: &str,
        ) -> Result<String, ProtoError> {
            let action = action
                .trim_matches('"')
                .rsplit('#')
                .next()
                .unwrap_or_default()
                .to_string();
            self.sent.lock().unwrap().push(action.clone());
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:x\">{}</u:{action}Response></s:Body></s:Envelope>",
                self.out_args
            ))
        }
    }

    /// A surface over `households`; returns the SOAP action log too.
    pub fn surface(
        out_args: &'static str,
        households: Vec<HouseholdState>,
    ) -> (Surface, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let canned = Canned {
            out_args,
            sent: Arc::clone(&sent),
        };
        let survey: Survey = Box::new(move |_| Ok(households.clone()));
        let s = Surface::new(
            Box::new(canned),
            survey,
            Policy::default(),
            Box::new(SystemClock),
        );
        (s, sent)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::surface;
    use super::*;
    use crate::plan::{TransportAction, plan_transport};
    use crate::request::ZoneRequest;
    use crate::zones::fixtures::households;

    #[test]
    fn control_authorizes_plans_and_executes() {
        let (s, sent) = surface("", households());
        let out = s
            .control(&Client::McpStdio, "pause", |h| {
                plan_transport(
                    h,
                    &ZoneRequest {
                        zone: "kitchen@s1".into(),
                    },
                    TransportAction::Pause,
                )
            })
            .unwrap();
        assert_eq!(out.done, "paused Den's group");
        assert_eq!(*sent.lock().unwrap(), ["Pause"]);
        let denied = s
            .control(&Client::Unknown, "pause", |_| unreachable!())
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn zones_and_one_zone_read_live_state() {
        let (s, _) = surface(
            "<CurrentTransportState>PAUSED_PLAYBACK</CurrentTransportState>",
            households(),
        );
        let zones = s.zones(&Client::Unknown).unwrap();
        assert_eq!(zones.len(), 4);
        assert!(zones.iter().all(|z| z.transport_state == "paused"));
        let den = s.zone(&Client::Unknown, "Kitchen@S1").unwrap();
        assert_eq!(den.coordinator_room, "Den");
    }

    #[test]
    fn nothing_discovered_is_not_ready() {
        let (s, _) = surface("", Vec::new());
        assert_eq!(
            s.zones(&Client::LoopbackHttp).unwrap_err().code,
            ErrorCode::NotReady
        );
    }
}
