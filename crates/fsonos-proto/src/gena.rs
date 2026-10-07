//! GENA event subscriptions.
//!
//! Sonos pushes state changes (transport, volume, topology) to subscribers via
//! GENA: the controller SUBSCRIBEs with a callback URL, and the player sends
//! NOTIFY requests carrying a property set. AVTransport and RenderingControl
//! put their changes in a `LastChange` document; ZoneGroupTopology and
//! GroupRenderingControl send plain properties. Subscriptions must be renewed
//! before their timeout. Header construction and NOTIFY parsing are pure here;
//! the requests and the callback sink live in [`crate::net`].

use crate::control::{parse_hms, transport_state};
use crate::didl::{DidlObject, parse_didl};
use crate::{ProtoError, xml};
use fsonos_types::TransportState;

/// A GENA subscription handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub sid: String,
    pub timeout_secs: u32,
}

/// Build the SUBSCRIBE request headers for `event_path` with the given callback
/// URL and requested timeout.
#[must_use]
pub fn subscribe_headers(callback_url: &str, timeout_secs: u32) -> Vec<(String, String)> {
    vec![
        ("CALLBACK".into(), format!("<{callback_url}>")),
        ("NT".into(), "upnp:event".into()),
        ("TIMEOUT".into(), format!("Second-{timeout_secs}")),
    ]
}

/// Headers that renew subscription `sid` (no CALLBACK or NT on a renewal).
#[must_use]
pub fn renew_headers(sid: &str, timeout_secs: u32) -> Vec<(String, String)> {
    vec![
        ("SID".into(), sid.into()),
        ("TIMEOUT".into(), format!("Second-{timeout_secs}")),
    ]
}

/// Parse the `TIMEOUT: Second-N` response header value into seconds.
#[must_use]
pub fn parse_timeout(value: &str) -> Option<u32> {
    value.trim().strip_prefix("Second-")?.parse().ok()
}

/// Read a SUBSCRIBE (or renewal) response's headers into a [`Subscription`].
pub fn subscription_from_headers(headers: &[(String, String)]) -> Result<Subscription, ProtoError> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    let sid = header("SID")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProtoError::Malformed("SUBSCRIBE response has no SID".into()))?;
    let timeout_secs = header("TIMEOUT")
        .and_then(parse_timeout)
        .ok_or_else(|| ProtoError::Malformed("SUBSCRIBE response has no TIMEOUT".into()))?;
    Ok(Subscription {
        sid: sid.to_string(),
        timeout_secs,
    })
}

/// One NOTIFY a player sent to the callback sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notify {
    /// The subscription it belongs to.
    pub sid: String,
    /// Event sequence number: 0 for the initial full-state event, then +1.
    pub seq: u32,
    /// The callback path it arrived on (names the service it came from).
    pub path: String,
    /// The property set, in document order, text entity-decoded.
    pub properties: Vec<(String, String)>,
}

impl Notify {
    /// The property `name`, if present.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The `LastChange` document of an AVTransport or RenderingControl event.
    pub fn last_change(&self) -> Result<Option<LastChange>, ProtoError> {
        self.property("LastChange")
            .map(parse_last_change)
            .transpose()
    }
}

/// Parse a NOTIFY body (`<e:propertyset>`) into its properties.
pub fn parse_propertyset(body: &str) -> Result<Vec<(String, String)>, ProtoError> {
    let doc = xml::parse(body)?;
    let root = doc.root_element();
    if root.tag_name().name() != "propertyset" {
        return Err(ProtoError::Malformed(format!(
            "expected a GENA propertyset, got <{}>",
            root.tag_name().name()
        )));
    }
    Ok(root
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "property")
        .flat_map(|p| p.children().filter(roxmltree::Node::is_element))
        .map(|v| {
            (
                v.tag_name().name().to_string(),
                v.text().unwrap_or("").to_string(),
            )
        })
        .collect())
}

/// One `<Name val="..."/>` of a `LastChange` event (instance 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventValue {
    pub name: String,
    /// The `channel` attribute (RenderingControl: `Master`, `LF`, `RF`).
    pub channel: Option<String>,
    pub val: String,
}

/// A parsed `LastChange` document: the variables that changed (all of them in
/// the initial event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastChange {
    pub values: Vec<EventValue>,
}

/// Parse a `LastChange` document (already entity-decoded once from the
/// property set).
pub fn parse_last_change(doc_text: &str) -> Result<LastChange, ProtoError> {
    let doc = xml::parse(doc_text)?;
    let event = doc.root_element();
    if event.tag_name().name() != "Event" {
        return Err(ProtoError::Malformed(format!(
            "expected a LastChange Event, got <{}>",
            event.tag_name().name()
        )));
    }
    // AVTransport/RenderingControl nest under <InstanceID val="0">; Queue
    // events use <QueueID val="0"> instead (verified live, S1 57.23).
    let instance = event
        .children()
        .find(|n| n.is_element() && matches!(n.tag_name().name(), "InstanceID" | "QueueID"))
        .ok_or_else(|| ProtoError::Malformed("LastChange has no InstanceID/QueueID".into()))?;
    Ok(LastChange {
        values: instance
            .children()
            .filter(roxmltree::Node::is_element)
            .map(|v| EventValue {
                name: v.tag_name().name().to_string(),
                channel: v.attribute("channel").map(str::to_string),
                val: v.attribute("val").map_or_else(
                    // Sonos occasionally sends the value as element text instead
                    // of a `val` attribute (community-documented quirk).
                    || v.text().unwrap_or("").trim().to_string(),
                    str::to_string,
                ),
            })
            .collect(),
    })
}

impl LastChange {
    /// The variable `name` (for channelled variables, its `Master` value).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        let mut matches = self.values.iter().filter(|v| v.name == name);
        let first = matches.next()?;
        if first.channel.is_none() {
            return Some(&first.val);
        }
        std::iter::once(first)
            .chain(matches)
            .find(|v| v.channel.as_deref() == Some("Master"))
            .map(|v| v.val.as_str())
    }

    #[must_use]
    pub fn transport_state(&self) -> Option<TransportState> {
        self.get("TransportState").map(transport_state)
    }

    /// The Master volume (RenderingControl).
    #[must_use]
    pub fn volume(&self) -> Option<u8> {
        self.get("Volume")?
            .trim()
            .parse::<u8>()
            .ok()
            .map(|v| v.min(100))
    }

    /// The Master mute (RenderingControl).
    #[must_use]
    pub fn mute(&self) -> Option<bool> {
        self.get("Mute").map(|v| v.trim() == "1")
    }

    #[must_use]
    pub fn current_track_uri(&self) -> Option<&str> {
        self.get("CurrentTrackURI").filter(|v| !v.is_empty())
    }

    #[must_use]
    pub fn current_track_duration_secs(&self) -> Option<u32> {
        self.get("CurrentTrackDuration").and_then(parse_hms)
    }

    /// The current track's DIDL-Lite metadata, when the event carries any.
    pub fn current_track_metadata(&self) -> Result<Option<DidlObject>, ProtoError> {
        match self.get("CurrentTrackMetaData").map(str::trim) {
            None | Some("" | "NOT_IMPLEMENTED") => Ok(None),
            Some(doc) => Ok(parse_didl(doc)?.into_iter().next()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AVT_S1: &str = include_str!("../tests/fixtures/gena_notify_avt_initial_s1.xml");
    const AVT_PAUSE_S1: &str = include_str!("../tests/fixtures/gena_notify_avt_pause_s1.xml");
    const AVT_S2: &str = include_str!("../tests/fixtures/gena_notify_avt_initial_s2.xml");
    const RCS_VOL_S1: &str = include_str!("../tests/fixtures/gena_notify_rcs_volume_s1.xml");
    const RCS_INIT_S1: &str = include_str!("../tests/fixtures/gena_notify_rcs_initial_s1.xml");
    const ZGT_S1: &str = include_str!("../tests/fixtures/gena_notify_zgt_s1.xml");

    fn notify(body: &str) -> Notify {
        Notify {
            sid: "uuid:test".into(),
            seq: 0,
            path: "/".into(),
            properties: parse_propertyset(body).unwrap(),
        }
    }

    #[test]
    fn builds_subscribe_and_renew_headers() {
        let h = subscribe_headers("http://192.0.2.1:3400/cb", 300);
        assert!(
            h.iter()
                .any(|(k, v)| k == "CALLBACK" && v == "<http://192.0.2.1:3400/cb>")
        );
        let r = renew_headers("uuid:RINCON_X_sub1", 600);
        assert_eq!(r[0], ("SID".into(), "uuid:RINCON_X_sub1".into()));
        assert!(!r.iter().any(|(k, _)| k == "CALLBACK" || k == "NT"));
    }

    #[test]
    fn parses_timeout_and_subscription_headers() {
        assert_eq!(parse_timeout("Second-300"), Some(300));
        assert_eq!(parse_timeout("infinite"), None);
        let sub = subscription_from_headers(&[
            ("sid".into(), "uuid:RINCON_X_sub1".into()),
            ("Timeout".into(), "Second-3600".into()),
        ])
        .unwrap();
        assert_eq!(sub.timeout_secs, 3600);
        assert!(subscription_from_headers(&[("TIMEOUT".into(), "Second-1".into())]).is_err());
    }

    #[test]
    fn avtransport_initial_event_carries_state_and_track() {
        let lc = notify(AVT_S1).last_change().unwrap().unwrap();
        assert!(lc.transport_state().is_some());
        assert!(lc.get("NumberOfTracks").is_some());
        // The initial event is the full state, metadata included.
        let md = lc.current_track_metadata().unwrap();
        if lc.current_track_uri().is_some() {
            assert!(md.is_some_and(|m| !m.title.is_empty()));
        }
    }

    #[test]
    fn pause_snapshot_reports_transitioning() {
        let lc = notify(AVT_PAUSE_S1).last_change().unwrap().unwrap();
        assert_eq!(lc.transport_state(), Some(TransportState::Transitioning));
    }

    #[test]
    fn idle_s2_player_parses() {
        let lc = notify(AVT_S2).last_change().unwrap().unwrap();
        assert!(lc.transport_state().is_some());
    }

    #[test]
    fn rendering_control_reads_master_channel() {
        let lc = notify(RCS_VOL_S1).last_change().unwrap().unwrap();
        assert_eq!(lc.volume(), Some(19), "Master, not LF/RF (both 100)");
        let init = notify(RCS_INIT_S1).last_change().unwrap().unwrap();
        assert!(init.volume().is_some());
        assert!(init.mute().is_some());
    }

    #[test]
    fn topology_event_is_plain_properties() {
        let n = notify(ZGT_S1);
        assert!(n.last_change().unwrap().is_none());
        let zgs =
            crate::topology::parse_zone_group_state(n.property("ZoneGroupState").unwrap()).unwrap();
        assert_eq!(zgs.groups.len(), 4, "the whole household's groups");
    }

    #[test]
    fn rejects_non_propertyset_and_non_event_documents() {
        assert!(parse_propertyset("<Event/>").is_err());
        assert!(parse_last_change("<propertyset/>").is_err());
        assert!(parse_last_change("<Event xmlns=\"urn:x\"/>").is_err());
    }

    #[test]
    fn accepts_values_as_element_text() {
        // Community-documented Sonos quirk: the value sometimes arrives as
        // element text instead of a `val` attribute.
        let lc = parse_last_change(
            "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">\
            <TransportState>PLAYING</TransportState></InstanceID></Event>",
        )
        .unwrap();
        assert_eq!(lc.get("TransportState"), Some("PLAYING"));
        // Absent val AND absent text still parses as empty (e.g. `<CurrentTrackURI/>`).
        let lc = parse_last_change(
            "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\">\
            <CurrentTrackURI/></InstanceID></Event>",
        )
        .unwrap();
        assert_eq!(lc.get("CurrentTrackURI"), Some(""));
    }

    #[test]
    fn queue_events_use_queueid_container() {
        // Verified live (S1 57.23): Queue mutations nest under QueueID, not
        // InstanceID, with an incrementing UpdateID.
        let lc = parse_last_change(
            "<Event xmlns=\"urn:schemas-sonos-com:metadata-1-0/Queue/\"><QueueID val=\"0\">\
            <UpdateID val=\"8\"/></QueueID></Event>",
        )
        .unwrap();
        assert_eq!(lc.get("UpdateID"), Some("8"));
    }
}
