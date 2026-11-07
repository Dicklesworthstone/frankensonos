//! Deterministic SOAP reads around the production Batch orchestration.
//! Mutation injection is explicit; the parent tests also use real S1/S2 sockets.

use super::super::*;
use fsonos_proto::didl::xml_escape;
use fsonos_proto::soap::{self, Service};
use std::cell::RefCell;
use std::fmt::Write as _;

pub(super) struct State {
    pub(super) uris: Vec<String>,
    pub(super) revision: u32,
    pub(super) boot: Option<u32>,
    pub(super) generation: Option<u8>,
    pub(super) source: String,
    pub(super) extra_member: bool,
}

pub(super) struct Model {
    pub(super) state: RefCell<State>,
    pub(super) reads: Cell<usize>,
    pub(super) browses: Cell<usize>,
    pub(super) pages: Cell<usize>,
    pub(super) writes: Cell<usize>,
    pub(super) omit_source: Cell<bool>,
    read_failures: Cell<usize>,
    edit_before_head: Cell<bool>,
}

#[derive(Clone, Copy)]
pub(super) enum Effect {
    Healthy,
    Lost,
    WrongAcknowledgement,
    SkipRevision,
    PrefixEdit,
    Reject,
    RejectWithExternalInsertion,
    Reboot,
    SourceChange,
    MissingSource,
    GroupChange,
    Unreadable,
    TransientReadFailure,
    BeforeFinalFence,
}

pub(super) fn host() -> IpAddr {
    "192.0.2.10".parse().unwrap()
}

pub(super) fn coordinator() -> PlayerId {
    PlayerId("RINCON_COORDINATOR".into())
}

fn response(service: &Service, action: &str, args: &[(&str, &str)]) -> String {
    soap::envelope(service, &format!("{action}Response"), &soap::args_xml(args))
}

fn argument(body: &str, name: &str) -> u32 {
    let tag = format!("<{name}>");
    body.split(tag.as_str())
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

impl Model {
    pub(super) fn new(length: usize) -> Self {
        Self {
            state: RefCell::new(State {
                uris: (0..length)
                    .map(|index| format!("file://test/{index}.flac"))
                    .collect(),
                revision: 7,
                boot: Some(3),
                generation: Some(2),
                source: "x-rincon-queue:RINCON_COORDINATOR#0".into(),
                extra_member: false,
            }),
            reads: Cell::new(0),
            browses: Cell::new(0),
            pages: Cell::new(0),
            writes: Cell::new(0),
            omit_source: Cell::new(false),
            read_failures: Cell::new(0),
            edit_before_head: Cell::new(false),
        }
    }

    pub(super) fn add(
        &self,
        batch: &mut Batch,
        uri: &str,
        desired: u32,
        effect: Effect,
    ) -> Result<(u32, Option<u32>), FeedError> {
        batch.confirmed(self, host(), uri, desired, || {
            self.insert(uri, desired, effect)
        })
    }

    fn insert(
        &self,
        uri: &str,
        desired: u32,
        effect: Effect,
    ) -> Result<(u32, Option<u32>), FeedError> {
        self.writes.set(self.writes.get() + 1);
        let rejection = || {
            FeedError::Proto(ProtoError::SoapFault {
                code: 501,
                reason: "injected explicit refusal".into(),
            })
        };
        if matches!(effect, Effect::Reject) {
            return Err(rejection());
        }
        let mut state = self.state.borrow_mut();
        let index = if desired == 0 {
            state.uris.len()
        } else {
            usize::try_from(desired - 1).unwrap()
        };
        state.uris.insert(index, uri.to_owned());
        state.revision = state.revision.wrapping_add(1);
        let position = u32::try_from(index + 1).unwrap();
        let length = u32::try_from(state.uris.len()).unwrap();
        match effect {
            Effect::Healthy => {}
            Effect::Lost => return Err(Replies::lost(host()).into()),
            Effect::WrongAcknowledgement => return Ok((position + 1, Some(length))),
            Effect::SkipRevision => state.revision = state.revision.wrapping_add(1),
            Effect::PrefixEdit => {
                state.uris[0] = "file://external/replacement.flac".into();
                state.revision = state.revision.wrapping_add(1);
            }
            Effect::RejectWithExternalInsertion => return Err(rejection()),
            Effect::Reboot => state.boot = state.boot.map(|boot| boot + 1),
            Effect::SourceChange => state.source = "x-sonosapi-stream:external".into(),
            Effect::MissingSource => self.omit_source.set(true),
            Effect::GroupChange => state.extra_member = true,
            Effect::Unreadable => self.read_failures.set(usize::MAX),
            Effect::TransientReadFailure => self.read_failures.set(1),
            Effect::BeforeFinalFence => self.edit_before_head.set(true),
            Effect::Reject => unreachable!("returned before the request"),
        }
        Ok((position, Some(length)))
    }

    fn topology(&self) -> String {
        let state = self.state.borrow();
        let boot = state
            .boot
            .map_or_else(String::new, |boot| format!(r#" BootSeq="{boot}""#));
        let generation = state.generation.map_or_else(String::new, |generation| {
            format!(r#" SWGen="{generation}""#)
        });
        let extra = if state.extra_member {
            r#"<ZoneGroupMember UUID="RINCON_OTHER" Location="http://192.0.2.11:1400/"/>"#
        } else {
            ""
        };
        let topology = format!(
            r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_COORDINATOR" ID="group:1">
            <ZoneGroupMember UUID="RINCON_COORDINATOR" Location="http://192.0.2.10:1400/"
            {boot} {generation}/>{extra}</ZoneGroup></ZoneGroups>"#
        );
        response(
            &soap::ZONE_GROUP_TOPOLOGY,
            "GetZoneGroupState",
            &[("ZoneGroupState", &topology)],
        )
    }

    fn browse(&self, body: &str) -> Result<String, ProtoError> {
        assert!(body.contains("<ObjectID>Q:0</ObjectID>"));
        self.browses.set(self.browses.get() + 1);
        if self.read_failures.get() > 0 {
            self.read_failures.set(self.read_failures.get() - 1);
            return Err(Replies::lost(host()));
        }
        let offset = usize::try_from(argument(body, "StartingIndex")).unwrap();
        let count = usize::try_from(argument(body, "RequestedCount")).unwrap();
        assert!((1..=100).contains(&count));
        if count == 100 {
            self.pages.set(self.pages.get() + 1);
        }
        if count == 1 && offset == 0 && self.edit_before_head.replace(false) {
            let mut state = self.state.borrow_mut();
            state.uris.swap(0, 1);
            state.revision = state.revision.wrapping_add(1);
        }
        let state = self.state.borrow();
        let mut didl = String::from(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
            xmlns:dc="http://purl.org/dc/elements/1.1/">"#,
        );
        let mut returned = 0;
        for (index, uri) in state.uris.iter().enumerate().skip(offset).take(count) {
            write!(
                didl,
                r#"<item id="Q:0/{}" parentID="Q:0" restricted="1">
                <dc:title>Test track</dc:title><res protocolInfo="http-get:*:audio/flac:*">{}</res>
                </item>"#,
                index + 1,
                xml_escape(uri)
            )
            .unwrap();
            returned += 1;
        }
        didl.push_str("</DIDL-Lite>");
        Ok(response(
            &soap::CONTENT_DIRECTORY,
            "Browse",
            &[
                ("Result", &didl),
                ("NumberReturned", &returned.to_string()),
                ("TotalMatches", &state.uris.len().to_string()),
                ("UpdateID", &state.revision.to_string()),
            ],
        ))
    }
}

impl Transport for Model {
    fn soap_post(
        &self,
        target: IpAddr,
        _: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        assert_eq!(target, host());
        self.reads.set(self.reads.get() + 1);
        if action.contains("#Browse") {
            self.browse(body)
        } else if action.contains("#GetZoneGroupState") {
            Ok(self.topology())
        } else if action.contains("#GetMediaInfo") {
            if self.omit_source.get() {
                return Ok(response(&soap::AV_TRANSPORT, "GetMediaInfo", &[]));
            }
            Ok(response(
                &soap::AV_TRANSPORT,
                "GetMediaInfo",
                &[("CurrentURI", &self.state.borrow().source)],
            ))
        } else {
            panic!("unexpected action: {action}");
        }
    }
}
