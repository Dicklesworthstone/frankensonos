//! Corrupt only topology evidence while all speaker traffic uses real sockets.

use fsonos_proto::{ProtoError, Transport, soap};
use fsonos_sim::SimLan;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct IncoherentTopology {
    pub inner: SimLan,
    pub enabled: Arc<AtomicBool>,
}

impl Transport for IncoherentTopology {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if !action.ends_with("#GetZoneGroupState\"") || !self.enabled.load(Ordering::SeqCst) {
            return Ok(response);
        }
        let parsed = soap::parse_response(&response, "GetZoneGroupState")?;
        let document = parsed.require("ZoneGroupState")?;
        // The selected group is unchanged. A consumer checking only its
        // members would overlook this unaddressable coordinator elsewhere.
        let invalid = r#"<ZoneGroup Coordinator="RINCON_MISSING" ID="invalid-group">
          <ZoneGroupMember UUID="RINCON_UNTRUSTED" ZoneName="Untrusted"
            Location="http://192.0.2.254:1400/xml/device_description.xml"/>
        </ZoneGroup>"#;
        assert!(document.contains("</ZoneGroups>"));
        let document = document.replace("</ZoneGroups>", &format!("{invalid}</ZoneGroups>"));
        Ok(soap::envelope(
            &soap::ZONE_GROUP_TOPOLOGY,
            "GetZoneGroupStateResponse",
            &soap::args_xml(&[("ZoneGroupState", &document)]),
        ))
    }
}
