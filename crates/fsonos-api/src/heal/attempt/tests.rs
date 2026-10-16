use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

struct Replies(RefCell<VecDeque<Result<String, ProtoError>>>);

impl Transport for Replies {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        self.0
            .borrow_mut()
            .pop_front()
            .expect("unexpected SOAP call")
    }

    fn http_get(&self, _: &str) -> Result<String, ProtoError> {
        Ok("description".into())
    }

    fn http_get_bytes(&self, _: &str) -> Result<HttpBody, ProtoError> {
        Ok(HttpBody {
            content_type: Some("image/png".into()),
            body: vec![1, 2, 3],
        })
    }

    fn discover(&self, _: u8, _: Duration) -> Discovered {
        Discovered {
            ssdp: Ok(Vec::new()),
            mdns: Ok(Vec::new()),
        }
    }
}

#[allow(clippy::unnecessary_wraps)] // a scripted reply, beside refused()
fn ack(action: &str) -> Result<String, ProtoError> {
    Ok(format!(
        "<Envelope><Body><{action}Response/></Body></Envelope>"
    ))
}

fn refused() -> Result<String, ProtoError> {
    Err(ProtoError::SoapFault {
        code: 800,
        reason: "not coordinator".into(),
    })
}

fn invoke(t: &impl Transport, action: &str) -> Result<soap::SoapResponse, ProtoError> {
    soap::call(
        t,
        std::net::Ipv4Addr::LOCALHOST.into(),
        &soap::AV_TRANSPORT,
        action,
        "",
    )
}

#[test]
fn initial_refusals_and_observations_leave_no_earlier_write() {
    let replies = Replies(RefCell::new(VecDeque::from([
        ack("GetPositionInfo"),
        refused(),
        ack("Browse"),
        refused(),
    ])));
    let t = Observed::new(&replies);
    invoke(&t, "GetPositionInfo").unwrap();
    assert!(invoke(&t, "Play").is_err());
    invoke(&t, "Browse").unwrap();
    assert!(invoke(&t, "Play").is_err());
    assert_eq!(t.preceding_writes(), 0);
}

#[test]
fn raw_fault_bodies_are_refusals_not_successful_writes() {
    let fault = "<Envelope><Body><Fault><detail><UPnPError><errorCode>800</errorCode>\
                 </UPnPError></detail></Fault></Body></Envelope>";
    let replies = Replies(RefCell::new(VecDeque::from([
        Ok(fault.into()),
        ack("Play"),
    ])));
    let t = Observed::new(&replies);
    assert!(matches!(
        invoke(&t, "Play"),
        Err(ProtoError::SoapFault { code: 800, .. })
    ));
    assert_eq!(t.preceding_writes(), 0);
    invoke(&t, "Play").unwrap();
    assert_eq!(t.preceding_writes(), 1);
}

#[test]
fn a_late_refusal_does_not_erase_an_earlier_accepted_source() {
    let replies = Replies(RefCell::new(VecDeque::from([
        ack("SetAVTransportURI"),
        ack("GetMediaInfo"),
        refused(),
    ])));
    let t = Observed::new(&replies);
    invoke(&t, "SetAVTransportURI").unwrap();
    invoke(&t, "GetMediaInfo").unwrap();
    assert!(invoke(&t, "Play").is_err());
    assert_eq!(t.preceding_writes(), 1);
}

#[test]
fn uncertain_writes_survive_a_later_read_back_failure() {
    for uncertain in [
        Err(ProtoError::Network {
            target: "synthetic".into(),
            detail: "lost".into(),
        }),
        Ok("unreadable".into()),
        Err(ProtoError::SoapFault {
            code: 402,
            reason: "leaving anyway".into(),
        }),
    ] {
        let replies = Replies(RefCell::new(VecDeque::from([uncertain, refused()])));
        let t = Observed::new(&replies);
        assert!(invoke(&t, "SetAVTransportURI").is_err());
        assert_eq!(
            t.preceding_writes(),
            0,
            "a lone repeat-safe operation can still relocate"
        );
        assert!(invoke(&t, "GetMediaInfo").is_err());
        assert_eq!(
            t.preceding_writes(),
            1,
            "read-back does not undo a possible effect"
        );
    }
}

#[test]
fn unknown_actions_are_not_assumed_to_be_reads() {
    let replies = Replies(RefCell::new(VecDeque::from([
        ack("GetAndChangeSomething"),
        refused(),
    ])));
    let t = Observed::new(&replies);
    invoke(&t, "GetAndChangeSomething").unwrap();
    assert!(invoke(&t, "Play").is_err());
    assert_eq!(t.preceding_writes(), 1);
}

#[test]
fn other_transport_channels_are_forwarded_without_losing_progress() {
    let replies = Replies(RefCell::new(VecDeque::from([ack("SetMute")])));
    let t = Observed::new(&replies);
    invoke(&t, "SetMute").unwrap();
    assert_eq!(t.http_get("synthetic").unwrap(), "description");
    assert_eq!(t.preceding_writes(), 1);
    assert_eq!(t.http_get_bytes("synthetic").unwrap().body, [1, 2, 3]);
    let found = t.discover(1, Duration::ZERO);
    assert!(found.ssdp.unwrap().is_empty());
    assert!(found.mdns.unwrap().is_empty());
    assert_eq!(t.preceding_writes(), 1);
}
