//! Native analog inputs, described by the player's AudioIn service.
//!
//! GetAudioInputAttributes supplies the owner's input name and icon; it
//! does not report cable connection or signal activity. Those are separate
//! event variables, so a successful read never implies that a cable is plugged
//! in. The wire contract is documented from device SCPDs at
//! <https://sonos.svrooij.io/services/audio-in>.

use crate::soap::{self, AUDIO_IN};
use crate::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

/// The name and icon the owner assigned to one physical input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputAttributes {
    pub name: String,
    pub icon: String,
}

/// Read one input's attributes. AudioIn has no InstanceID argument.
pub fn attributes<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
) -> Result<InputAttributes, ProtoError> {
    let response = soap::call(transport, host, &AUDIO_IN, "GetAudioInputAttributes", "")?;
    Ok(InputAttributes {
        name: response.require("CurrentName")?.to_string(),
        icon: response.require("CurrentIcon")?.to_string(),
    })
}

/// Select this player's input through AVTransport on the destination coordinator.
/// The exact source player matters, including the hidden half of a stereo pair.
pub fn source_uri(source: &PlayerId) -> Result<String, ProtoError> {
    if source.0.is_empty()
        || source.0.len() > 128
        || !source
            .0
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(ProtoError::Malformed(
            "invalid line-in player identifier".into(),
        ));
    }
    Ok(format!("x-rincon-stream:{}", source.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Reply<'a> {
        body: &'a str,
        calls: Cell<usize>,
    }

    impl Transport for Reply<'_> {
        fn soap_post(
            &self,
            _: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            assert_eq!(path, "/AudioIn/Control");
            assert_eq!(
                action,
                "\"urn:schemas-upnp-org:service:AudioIn:1#GetAudioInputAttributes\""
            );
            assert!(!body.contains("InstanceID"));
            self.calls.set(self.calls.get() + 1);
            Ok(self.body.into())
        }
    }

    fn response(args: &str) -> String {
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:GetAudioInputAttributesResponse xmlns:u=\"urn:schemas-upnp-org:service:AudioIn:1\">\
             {args}</u:GetAudioInputAttributesResponse></s:Body></s:Envelope>"
        )
    }

    #[test]
    fn attributes_keep_the_owners_label_without_inventing_connection_state() {
        let body =
            response("<CurrentName>Vinyl &amp; CD</CurrentName><CurrentIcon>Phono</CurrentIcon>");
        let transport = Reply {
            body: &body,
            calls: Cell::new(0),
        };
        assert_eq!(
            attributes(&transport, [192, 0, 2, 10].into()).unwrap(),
            InputAttributes {
                name: "Vinyl & CD".into(),
                icon: "Phono".into()
            }
        );
        assert_eq!(transport.calls.get(), 1);
    }

    #[test]
    fn empty_labels_are_valid_but_missing_fields_are_not() {
        for args in ["<CurrentName/>", "<CurrentIcon/>", ""] {
            let body = response(args);
            let transport = Reply {
                body: &body,
                calls: Cell::new(0),
            };
            assert!(attributes(&transport, [192, 0, 2, 10].into()).is_err());
        }
        let body = response("<CurrentName/><CurrentIcon/>");
        let transport = Reply {
            body: &body,
            calls: Cell::new(0),
        };
        assert_eq!(
            attributes(&transport, [192, 0, 2, 10].into()).unwrap().name,
            ""
        );
    }

    #[test]
    fn exact_player_identifiers_form_one_native_input_uri() {
        assert_eq!(
            source_uri(&PlayerId("RINCON_SYNTHETIC01400".into())).unwrap(),
            "x-rincon-stream:RINCON_SYNTHETIC01400"
        );
        for id in [
            "",
            " ",
            "source:other",
            "source#0",
            "source?x",
            "source\n",
            "源",
        ] {
            assert!(source_uri(&PlayerId(id.into())).is_err(), "{id:?}");
        }
        assert!(source_uri(&PlayerId("x".repeat(129))).is_err());
    }
}
