//! Native ContentDirectory catalog mutations. Each helper sends one action;
//! the caller supplies the observed identity and confirms the resulting catalog.

use crate::didl::xml_escape;
use crate::{ProtoError, Transport, soap};
use std::net::IpAddr;

/// Create one native catalog entry from an observed DIDL-Lite document.
/// `Elements` is XML inside a SOAP string argument. The returned identity is
/// authoritative only after the caller confirms it in the same catalog.
/// No lost or malformed response is retried.
pub fn create_object<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    container_id: &str,
    elements: &str,
) -> Result<String, ProtoError> {
    let response = soap::call(
        transport,
        host,
        &soap::CONTENT_DIRECTORY,
        "CreateObject",
        &soap::args_xml(&[("ContainerID", container_id), ("Elements", elements)]),
    )?;
    let id = response.require("ObjectID")?;
    if id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
        return Err(ProtoError::Malformed(
            "CreateObject did not return a usable native identity".into(),
        ));
    }
    Ok(id.to_owned())
}

/// Change only the title of the exact observed native object. The title
/// fragments are XML inside SOAP string arguments, so both layers are escaped.
/// Commas and backslashes remain literal title text. This follows Sonos's
/// `UpdateObject` handling documented by SoCo's `Favorites.update`: applying
/// generic UPnP CSV escaping would store the added backslashes in the title.
/// See <https://github.com/SoCo/SoCo/blob/master/soco/favorites.py>.
pub fn rename_object<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    object_id: &str,
    current_title: &str,
    new_title: &str,
) -> Result<(), ProtoError> {
    let current = format!("<dc:title>{}</dc:title>", xml_escape(current_title));
    let next = format!("<dc:title>{}</dc:title>", xml_escape(new_title));
    soap::call(
        transport,
        host,
        &soap::CONTENT_DIRECTORY,
        "UpdateObject",
        &soap::args_xml(&[
            ("ObjectID", object_id),
            ("CurrentTagValue", &current),
            ("NewTagValue", &next),
        ]),
    )?;
    Ok(())
}

/// Remove one exact native catalog object. A failure is returned unchanged;
/// this helper cannot retry, recreate the object, or alter the playing queue.
pub fn destroy_object<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    object_id: &str,
) -> Result<(), ProtoError> {
    soap::call(
        transport,
        host,
        &soap::CONTENT_DIRECTORY,
        "DestroyObject",
        &soap::args_xml(&[("ObjectID", object_id)]),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Replies {
        calls: RefCell<Vec<(String, String, String)>>,
        fault: u8,
    }

    impl Transport for Replies {
        fn soap_post(
            &self,
            host: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            self.calls
                .borrow_mut()
                .push((path.into(), action.into(), body.into()));
            match self.fault {
                1 => Err(ProtoError::SoapFault {
                    code: 702,
                    reason: "changed title".into(),
                }),
                2 => Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "lost reply".into(),
                }),
                3 => Ok("not a SOAP acknowledgement".into()),
                _ => {
                    let name = action.trim_matches('"').rsplit('#').next().unwrap();
                    Ok(soap::envelope(
                        &soap::CONTENT_DIRECTORY,
                        &format!("{name}Response"),
                        if name == "CreateObject" {
                            "<ObjectID>FV:2/7</ObjectID><Result/>"
                        } else {
                            ""
                        },
                    ))
                }
            }
        }
    }

    fn replies(fault: u8) -> Replies {
        Replies {
            calls: RefCell::default(),
            fault,
        }
    }

    #[test]
    fn rename_escapes_both_xml_layers_and_preserves_literal_title_punctuation() {
        let transport = replies(0);
        rename_object(
            &transport,
            "192.0.2.10".parse().unwrap(),
            "SQ:7",
            r"Old\, & <mix>",
            r"Dvořák, Bach & friends \ Live",
        )
        .unwrap();
        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 1);
        let (path, action, body) = &calls[0];
        assert_eq!(path, "/MediaServer/ContentDirectory/Control");
        assert_eq!(
            action,
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#UpdateObject\""
        );
        let doc = roxmltree::Document::parse(body).unwrap();
        let field = |name| {
            doc.descendants()
                .find(|node| node.has_tag_name(name))
                .unwrap()
                .text()
                .unwrap()
                .to_string()
        };
        assert_eq!(field("ObjectID"), "SQ:7");
        assert_eq!(
            field("CurrentTagValue"),
            r"<dc:title>Old\, &amp; &lt;mix&gt;</dc:title>"
        );
        assert_eq!(
            field("NewTagValue"),
            r"<dc:title>Dvořák, Bach &amp; friends \ Live</dc:title>"
        );
        assert!(!body.contains("InstanceID"));
    }

    #[test]
    fn removal_addresses_only_the_selected_catalog_object() {
        let transport = replies(0);
        destroy_object(&transport, "192.0.2.10".parse().unwrap(), "SQ:7").unwrap();
        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.ends_with("#DestroyObject\""));
        assert!(calls[0].2.contains("<ObjectID>SQ:7</ObjectID>"));
        assert!(!calls[0].2.contains("InstanceID"));
    }

    #[test]
    fn creation_preserves_the_complete_nested_document_as_one_soap_argument() {
        let transport = replies(0);
        let elements = "<DIDL-Lite><item id=\"\" parentID=\"FV:2\"><title>A &amp; B</title><resMD>&lt;DIDL-Lite&gt;&amp;amp;&lt;/DIDL-Lite&gt;</resMD></item></DIDL-Lite>";
        let id = create_object(
            &transport,
            "192.0.2.10".parse().unwrap(),
            "FV:2",
            elements,
        )
        .unwrap();
        assert_eq!(id, "FV:2/7");
        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/MediaServer/ContentDirectory/Control");
        assert!(calls[0].1.ends_with("#CreateObject\""));
        let doc = roxmltree::Document::parse(&calls[0].2).unwrap();
        assert_eq!(
            doc.descendants()
                .find(|node| node.has_tag_name("Elements"))
                .unwrap()
                .text(),
            Some(elements)
        );
        assert_eq!(
            doc.descendants()
                .find(|node| node.has_tag_name("ContainerID"))
                .unwrap()
                .text(),
            Some("FV:2")
        );
        assert!(!calls[0].2.contains("InstanceID"));
    }

    #[test]
    fn creation_never_repeats_an_uncertain_or_refused_request() {
        for fault in [1, 2, 3] {
            let transport = replies(fault);
            assert!(
                create_object(
                    &transport,
                    "192.0.2.10".parse().unwrap(),
                    "FV:2",
                    "<DIDL-Lite/>",
                )
                .is_err()
            );
            assert_eq!(transport.calls.borrow().len(), 1);
        }
    }

    #[test]
    fn explicit_and_uncertain_catalog_failures_never_repeat_a_write() {
        for fault in [1, 2, 3] {
            for remove in [false, true] {
                let transport = replies(fault);
                let host = "192.0.2.10".parse().unwrap();
                let error = if remove {
                    destroy_object(&transport, host, "SQ:7")
                } else {
                    rename_object(&transport, host, "SQ:7", "Before", "After")
                }
                .unwrap_err();
                assert!(match fault {
                    1 => matches!(error, ProtoError::SoapFault { code: 702, .. }),
                    2 => matches!(error, ProtoError::Network { .. }),
                    _ => matches!(error, ProtoError::Malformed(_)),
                });
                assert_eq!(transport.calls.borrow().len(), 1);
            }
        }
    }
}
