//! Coherent queue state for the control wire-contract tests.

use fsonos_proto::didl::xml_escape;
use fsonos_proto::soap;
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;

pub(super) struct QueueFixture {
    rows: RefCell<Vec<String>>,
    revision: Cell<u32>,
    serial: Cell<usize>,
}

impl Default for QueueFixture {
    fn default() -> Self {
        Self::new(4)
    }
}

impl QueueFixture {
    pub(super) fn new(count: usize) -> Self {
        Self {
            rows: RefCell::new((1..=count).map(|n| format!("x-file:track-{n}")).collect()),
            revision: Cell::new(1),
            serial: Cell::new(count),
        }
    }

    pub(super) fn browse(&self) -> String {
        let rows = self.rows.borrow();
        let mut items = String::new();
        for (index, uri) in rows.iter().enumerate() {
            write!(
                items,
                "<item id=\"Q:0/{}\"><res>{}</res></item>",
                index + 1,
                xml_escape(uri)
            )
            .unwrap();
        }
        let out = soap::args_xml(&[
            ("Result", &format!("<DIDL-Lite>{items}</DIDL-Lite>")),
            ("NumberReturned", &rows.len().to_string()),
            ("TotalMatches", &rows.len().to_string()),
            ("UpdateID", &self.revision.get().to_string()),
        ]);
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
             <s:Body><u:BrowseResponse xmlns:u=\"urn:x\">{out}</u:BrowseResponse>\
             </s:Body></s:Envelope>"
        )
    }

    pub(super) fn add(&self) -> String {
        let first = self.rows.borrow().len() + 1;
        let serial = self.serial.get() + 1;
        self.serial.set(serial);
        self.rows.borrow_mut().push(format!("x-file:track-{serial}"));
        self.revision.set(self.revision.get() + 1);
        format!(
            "<FirstTrackNumberEnqueued>{first}</FirstTrackNumberEnqueued>\
             <NumTracksAdded>1</NumTracksAdded>"
        )
    }

    pub(super) fn remove(&self, body: &str) -> String {
        let number = |name: &str| {
            let tag = format!("<{name}>");
            body.split_once(&tag)
                .unwrap()
                .1
                .split_once('<')
                .unwrap()
                .0
                .parse::<u32>()
                .unwrap()
        };
        assert_eq!(number("UpdateID"), self.revision.get());
        assert_ne!(number("UpdateID"), 0);
        let start = usize::try_from(number("StartingIndex") - 1).unwrap();
        let count = usize::try_from(number("NumberOfTracks")).unwrap();
        self.rows.borrow_mut().drain(start..start + count);
        self.revision.set(self.revision.get() + 1);
        format!("<NewUpdateID>{}</NewUpdateID>", self.revision.get())
    }
}
