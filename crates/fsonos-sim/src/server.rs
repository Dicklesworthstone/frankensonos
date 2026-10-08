//! One asupersync HTTP/1.1 listener per virtual player on `127.0.0.1:0`,
//! each on its own thread and runtime, all sharing one [`State`].

use crate::docs::{self, ServiceDef};
use crate::gena::{EventService, first_callback};
use crate::model::{Args, INVALID_ACTION, State};
use crate::{GenaEvent, SimError, SoapLogEntry};
use asupersync::http::h1::server::HostPolicy;
use asupersync::http::h1::types::{Method, Request, Response};
use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use asupersync::server::shutdown::ShutdownSignal;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A running listener: its address, and how to stop it.
pub(crate) struct Listener {
    pub addr: SocketAddr,
    pub shutdown: ShutdownSignal,
    pub thread: JoinHandle<()>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// Start the listener for player `index`.
pub(crate) fn start(state: Arc<Mutex<State>>, index: usize) -> Result<Listener, SimError> {
    let config = Http1ListenerConfig::default().http_config(Http1Config::default().host_policy(
        HostPolicy::allow_list(vec!["127.0.0.1".into(), "localhost".into()]),
    ));
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name(format!("fsonos-sim-player-{index}"))
        .spawn(move || {
            let runtime = match create_reactor().and_then(|reactor| {
                RuntimeBuilder::current_thread()
                    .with_reactor(reactor)
                    .build()
                    .map_err(std::io::Error::other)
            }) {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let handle = runtime.handle();
            runtime.block_on(async move {
                let listener = Http1Listener::bind_with_config(
                    "127.0.0.1:0",
                    move |req: Request| {
                        let response = respond(&state, index, &req);
                        async move { response }
                    },
                    config,
                )
                .await;
                let listener = match listener {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let ready = listener
                    .local_addr()
                    .map(|addr| (addr, listener.shutdown_signal()))
                    .map_err(|e| e.to_string());
                let failed = ready.is_err();
                let _ = ready_tx.send(ready);
                if !failed {
                    let _ = listener.run(&handle).await;
                }
            });
        })
        .map_err(|e| SimError::Start(e.to_string()))?;
    let (addr, shutdown) = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|e| SimError::Start(format!("listener {index} never reported: {e}")))?
        .map_err(SimError::Start)?;
    Ok(Listener {
        addr,
        shutdown,
        thread,
    })
}

fn text(status: u16, reason: &str, body: String) -> Response {
    Response::new(status, reason, body.into_bytes())
        .with_header("Content-Type", "text/xml; charset=\"utf-8\"")
        .with_header("Server", "Linux UPnP/1.0 Sonos/86.10-80260 (fsonos-sim)")
}

fn respond(shared: &Arc<Mutex<State>>, index: usize, req: &Request) -> Response {
    // Injected latency and unreachability apply before anything is read,
    // and never while holding the state lock.
    let (latency, down) = match shared.lock() {
        Ok(s) => {
            let p = &s.players[index];
            let rebooting = p
                .faults
                .unreachable_until
                .is_some_and(|t| Instant::now() < t);
            (p.faults.latency, p.offline || rebooting)
        }
        Err(_) => return text(500, "Internal Server Error", String::new()),
    };
    if !latency.is_zero() {
        thread::sleep(latency);
    }
    if down {
        return text(503, "Service Unavailable", String::new());
    }
    let Ok(mut state) = shared.lock() else {
        return text(500, "Internal Server Error", String::new());
    };
    state.settle_joins(Instant::now());
    state.settle();
    let path = req.uri.split('?').next().unwrap_or_default();
    let model = state.players[index].model;
    let response = match &req.method {
        Method::Get if path == "/xml/device_description.xml" => {
            let sw_gen = state.households[state.players[index].household].sw_gen;
            text(
                200,
                "OK",
                docs::device_description(&state.players[index], sw_gen),
            )
        }
        // Album art (`docs::art_path`): the same small image for any track.
        Method::Get if path == "/getaa" && req.uri.contains("u=") => {
            Response::new(200, "OK", docs::ART_PNG.to_vec())
                .with_header("Content-Type", "image/png")
                .with_header("Server", "Linux UPnP/1.0 Sonos/86.10-80260 (fsonos-sim)")
        }
        Method::Post => match docs::services(model)
            .into_iter()
            .find(|s| s.control == path)
        {
            Some(service) => soap(&mut state, index, &service, req),
            None => text(404, "Not Found", String::new()),
        },
        Method::Extension(m)
            if m.eq_ignore_ascii_case("SUBSCRIBE") || m.eq_ignore_ascii_case("UNSUBSCRIBE") =>
        {
            match EventService::from_path(path, model) {
                Some(service) => gena(&mut state, index, service, m, req),
                None => text(404, "Not Found", String::new()),
            }
        }
        _ => text(404, "Not Found", String::new()),
    };
    let fetches = std::mem::take(&mut state.pending_fetches);
    drop(state);
    for (player, url) in fetches {
        crate::fetch::spawn(Arc::clone(shared), player, url);
    }
    response
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
}

/// A `TIMEOUT: Second-N` request header, in seconds (`None`: unset/infinite).
fn requested_timeout(req: &Request) -> Option<u32> {
    header(req, "TIMEOUT").and_then(fsonos_proto::gena::parse_timeout)
}

/// SUBSCRIBE (new or renewal) and UNSUBSCRIBE, per UPnP Device Architecture
/// 4.1: a renewal carries only SID; a new subscription CALLBACK and NT; any
/// other mix is 400, and an unknown or lapsed SID is 412.
fn gena(
    state: &mut State,
    index: usize,
    service: EventService,
    method: &str,
    req: &Request,
) -> Response {
    let sid = header(req, "SID").filter(|s| !s.is_empty());
    let callback = header(req, "CALLBACK");
    let nt = header(req, "NT");
    let refuse = |state: &mut State, status: u16, reason: &str| {
        state.log_gena(
            index,
            service.name(),
            GenaEvent::Refused {
                status,
                reason: reason.to_string(),
            },
        );
        let phrase = if status == 412 {
            "Precondition Failed"
        } else {
            "Bad Request"
        };
        text(status, phrase, String::new())
    };
    let granted = |sid: &str, timeout: u32| {
        Response::new(200, "OK", Vec::new())
            .with_header("SID", sid)
            .with_header("TIMEOUT", format!("Second-{timeout}"))
            .with_header("Server", "Linux UPnP/1.0 Sonos/86.10-80260 (fsonos-sim)")
    };
    if method.eq_ignore_ascii_case("UNSUBSCRIBE") {
        return match sid {
            Some(sid) if callback.is_none() && nt.is_none() => {
                if state.unsubscribe(index, sid) {
                    Response::new(200, "OK", Vec::new())
                } else {
                    refuse(state, 412, "unknown or lapsed SID")
                }
            }
            Some(_) => refuse(state, 400, "SID with CALLBACK or NT"),
            None => refuse(state, 412, "UNSUBSCRIBE without SID"),
        };
    }
    match (sid, callback, nt) {
        (Some(sid), None, None) => match state.renew(index, sid, requested_timeout(req)) {
            Some(timeout) => granted(sid, timeout),
            None => refuse(state, 412, "unknown or lapsed SID"),
        },
        (None, Some(callback), Some("upnp:event")) => match first_callback(callback) {
            Some(url) => {
                let (sid, timeout) = state.subscribe(index, service, url, requested_timeout(req));
                granted(&sid, timeout)
            }
            None => refuse(state, 412, "no usable CALLBACK URL"),
        },
        (None, _, _) => refuse(state, 412, "SUBSCRIBE without CALLBACK and NT: upnp:event"),
        (Some(_), _, _) => refuse(state, 400, "SID with CALLBACK or NT"),
    }
}

fn soap(state: &mut State, index: usize, service: &ServiceDef, req: &Request) -> Response {
    let header = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("SOAPACTION"))
        .map(|(_, v)| v.trim().trim_matches('"').to_string());
    let body = String::from_utf8_lossy(&req.body);
    let parsed = parse_request(&body);
    let (action, args) = match (&header, parsed) {
        (Some(h), Some((action, args)))
            if h.split_once('#') == Some((service.service_type, action.as_str())) =>
        {
            (action, args)
        }
        (_, parsed) => {
            let (action, args) = parsed.unwrap_or_default();
            return fault(state, index, service, action, args, INVALID_ACTION.0);
        }
    };
    let injected = state.players[index]
        .faults
        .upnp
        .iter()
        .find(|(a, _)| *a == action)
        .map(|(_, code)| *code);
    if let Some(code) = injected {
        return fault(state, index, service, action, args, code);
    }
    match state.invoke(index, service, &action, &Args(&args)) {
        Ok(out) => {
            // Every change reaches the subscribers.
            state.flush_events();
            let body = docs::soap_response(service, &action, &out);
            log(
                state,
                index,
                service,
                action,
                args,
                Ok(out.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
            );
            text(200, "OK", body)
        }
        Err(f) => fault(state, index, service, action, args, f.0),
    }
}

fn fault(
    state: &mut State,
    index: usize,
    service: &ServiceDef,
    action: String,
    args: Vec<(String, String)>,
    code: u16,
) -> Response {
    log(state, index, service, action, args, Err(code));
    text(500, "Internal Server Error", docs::soap_fault(code))
}

fn log(
    state: &mut State,
    index: usize,
    service: &ServiceDef,
    action: String,
    args: Vec<(String, String)>,
    result: Result<Vec<(String, String)>, u16>,
) {
    let entry = SoapLogEntry {
        player: state.players[index].uuid.clone(),
        room: state.players[index].room.clone(),
        service: service.name.to_string(),
        action,
        args,
        result,
        at_ms: state.clock.now_ms(),
    };
    state.log.push(entry);
}

/// The action name and in-arguments of a SOAP request body.
pub(crate) fn parse_request(body: &str) -> Option<(String, Vec<(String, String)>)> {
    let doc = roxmltree::Document::parse(body).ok()?;
    let envelope = doc.root_element();
    let soap_body = envelope
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Body")?;
    let action = soap_body.children().find(roxmltree::Node::is_element)?;
    let args = action
        .children()
        .filter(roxmltree::Node::is_element)
        .map(|a| {
            (
                a.tag_name().name().to_string(),
                a.text().unwrap_or("").to_string(),
            )
        })
        .collect();
    Some((action.tag_name().name().to_string(), args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_action_and_decoded_args() {
        let body = fsonos_proto::soap::envelope(
            &fsonos_proto::soap::AV_TRANSPORT,
            "SetAVTransportURI",
            &fsonos_proto::soap::args_xml(&[
                ("InstanceID", "0"),
                ("CurrentURI", "x-rincon-queue:RINCON_X#0"),
                ("CurrentURIMetaData", "<a b=\"1\"/>"),
            ]),
        );
        let (action, args) = parse_request(&body).unwrap();
        assert_eq!(action, "SetAVTransportURI");
        assert_eq!(
            args[2],
            ("CurrentURIMetaData".to_string(), "<a b=\"1\"/>".to_string())
        );
        assert!(parse_request("not xml").is_none());
    }
}
