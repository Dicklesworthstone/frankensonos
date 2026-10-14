//! Queue shuffle/repeat settings on the coordinator's AVTransport service.

use crate::soap::{self, AV_TRANSPORT};
use crate::{ProtoError, Transport};
use fsonos_types::PlayMode;
use std::net::IpAddr;

/// Read the mode, without changing the source, queue, track, or playback.
/// A missing or unknown value is an error, never an assumed NORMAL mode.
pub fn get_play_mode<T: Transport + ?Sized>(t: &T, host: IpAddr) -> Result<PlayMode, ProtoError> {
    let response = soap::call(
        t,
        host,
        &AV_TRANSPORT,
        "GetTransportSettings",
        &soap::args_xml(&[("InstanceID", "0")]),
    )?;
    let value = response.require("PlayMode")?;
    PlayMode::from_wire(value)
        .ok_or_else(|| ProtoError::Malformed(format!("unrecognized PlayMode: {value:?}")))
}

/// Set one of the supported shuffle/repeat combinations. This does not start
/// playback or change the source. The caller resolves the group coordinator.
pub fn set_play_mode<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    mode: PlayMode,
) -> Result<(), ProtoError> {
    soap::call(
        t,
        host,
        &AV_TRANSPORT,
        "SetPlayMode",
        &soap::args_xml(&[("InstanceID", "0"), ("NewPlayMode", mode.as_str())]),
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::net::Ipv4Addr;

    struct Canned {
        output: String,
        sent: RefCell<Vec<(String, String)>>,
    }

    impl Canned {
        fn new(output: &str) -> Self {
            Self {
                output: output.into(),
                sent: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Canned {
        fn soap_post(
            &self,
            _: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            assert_eq!(path, AV_TRANSPORT.control_path);
            let name = action.trim_matches('"').rsplit('#').next().unwrap();
            self.sent.borrow_mut().push((name.into(), body.into()));
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                 <s:Body><u:{name}Response xmlns:u=\"urn:x\">{}\
                 </u:{name}Response></s:Body></s:Envelope>",
                self.output
            ))
        }
    }

    #[test]
    fn all_modes_are_read_from_transport_settings() {
        for mode in PlayMode::ALL {
            let t = Canned::new(&format!("<PlayMode>{}</PlayMode>", mode.as_str()));
            assert_eq!(get_play_mode(&t, Ipv4Addr::LOCALHOST.into()).unwrap(), mode);
            let sent = t.sent.borrow();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].0, "GetTransportSettings");
            assert!(sent[0].1.contains("<InstanceID>0</InstanceID>"));
        }
    }

    #[test]
    fn setting_a_mode_sends_only_that_setting() {
        for mode in PlayMode::ALL {
            let t = Canned::new("");
            set_play_mode(&t, Ipv4Addr::LOCALHOST.into(), mode).unwrap();
            let sent = t.sent.borrow();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].0, "SetPlayMode");
            assert!(sent[0].1.contains("<InstanceID>0</InstanceID>"));
            let expected = format!("<NewPlayMode>{}</NewPlayMode>", mode.as_str());
            assert!(sent[0].1.contains(&expected));
        }
    }

    #[test]
    fn missing_empty_and_unknown_settings_are_not_normal() {
        for output in [
            "",
            "<PlayMode></PlayMode>",
            "<PlayMode>NOT_IMPLEMENTED</PlayMode>",
            "<PlayMode>FUTURE_MODE</PlayMode>",
        ] {
            let t = Canned::new(output);
            assert!(matches!(
                get_play_mode(&t, Ipv4Addr::LOCALHOST.into()),
                Err(ProtoError::Malformed(_))
            ));
        }
    }

    #[test]
    fn an_unsupported_action_keeps_its_original_failure() {
        struct Unsupported;
        impl Transport for Unsupported {
            fn soap_post(
                &self,
                _: IpAddr,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<String, ProtoError> {
                Err(ProtoError::SoapFault {
                    code: 401,
                    reason: "unsupported settings".into(),
                })
            }
        }
        assert!(matches!(
            get_play_mode(&Unsupported, Ipv4Addr::LOCALHOST.into()),
            Err(ProtoError::SoapFault { code: 401, .. })
        ));
    }
}
