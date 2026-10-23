//! Counts this process's WARN and ERROR events by kind
//! ([`fsonos_core::warnings`]), for `GET /health` and `fsonos doctor`.
//! The kind is the event's call site; field values are never read, so the
//! addresses and ids they carry stay out of it.

use std::fmt;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// The counting layer, beside the log's own.
pub struct Counting;

impl<S: Subscriber> Layer<S> for Counting {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let meta = event.metadata();
        let error = *meta.level() == Level::ERROR;
        if !error && *meta.level() != Level::WARN {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        let kind = format!(
            "{}@{}:{}",
            meta.target(),
            meta.file().unwrap_or("?"),
            meta.line().unwrap_or(0)
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        fsonos_core::warnings::record(&kind, error, &message.0, now);
    }
}

/// An event's `message` field, and nothing else.
struct Message(String);

impl Visit for Message {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            value.clone_into(&mut self.0);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn a_warning_is_counted_by_its_call_site_and_never_by_its_fields() {
        let subscriber = tracing_subscriber::registry().with(Counting);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..25 {
                tracing::warn!(
                    player = "RINCON_000E58A0000401400",
                    addr = "192.0.2.77",
                    "renewal failed and so did a fresh subscription"
                );
            }
            tracing::info!("not counted");
        });
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let storms = fsonos_core::warnings::storms(now);
        let storm = storms
            .iter()
            .find(|s| s.sample == "renewal failed and so did a fresh subscription")
            .unwrap_or_else(|| panic!("{storms:?}"));
        assert!(storm.last_hour >= 25, "{storm:?}");
        assert!(!storm.error);
        assert!(storm.kind.contains("warning_counts::tests@"), "{storm:?}");
        assert!(storm.kind.contains("warning_counts.rs:"), "{storm:?}");
        let shown = format!("{storm:?}");
        assert!(
            !shown.contains("192.0.2.77") && !shown.contains("RINCON_000E58A"),
            "{shown}"
        );
    }
}
