//! One weakly owned event worker per MCP server, independent of request lifetimes.
//! It follows journal continuity only while authorized Sonos resources have
//! subscribers; it never reads local event payloads, surveys speakers or
//! replays a control operation. A stdio bridge retains its daemon identity.

use fastmcp::McpResult;
use fastmcp::common_types::AbsoluteUri;
use fastmcp_server::{
    MAX_RESOURCE_UPDATE_BATCH, MAX_RESOURCE_UPDATE_BATCH_BYTES, ResourceSubscriptions,
    ResourceUpdates,
};
use fsonos_api::Failure;
use fsonos_api::events::ResourceChanges;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const IDLE: Duration = Duration::from_millis(100);
const RETRY: Duration = Duration::from_secs(1);
const WAIT_SECS: u64 = 2;

pub(crate) fn start(updates: ResourceUpdates) {
    spawn(updates, |since, wait| {
        crate::tools::backend()?.resource_changes_answer(since, wait)
    })
    .expect("start the MCP resource event worker");
}

fn spawn(
    updates: ResourceUpdates,
    read: impl Fn(Option<u64>, u64) -> Result<ResourceChanges, Failure> + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("mcp-resource-events".into())
        .spawn(move || {
            let mut feed = Feed::default();
            while !updates.is_closed() {
                let delay = feed.poll(&updates, &read);
                pause(&updates, delay);
            }
        })
}

fn pause(updates: &ResourceUpdates, delay: Duration) {
    let deadline = Instant::now() + delay;
    while !updates.is_closed() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(IDLE));
    }
}

fn sonos(mut selected: ResourceSubscriptions) -> ResourceSubscriptions {
    selected.uris.retain(|uri| {
        (uri == "sonos://zones" || uri == "sonos://dj" || uri.starts_with("sonos://zones/"))
            && AbsoluteUri::parse(uri.as_str()).is_ok()
    });
    selected
}

#[derive(Default)]
struct Feed {
    since: Option<u64>,
    selected: ResourceSubscriptions,
    dirty: bool,
}

impl Feed {
    fn poll(
        &mut self,
        updates: &ResourceUpdates,
        read: &impl Fn(Option<u64>, u64) -> Result<ResourceChanges, Failure>,
    ) -> Duration {
        if sonos(updates.subscriptions()).uris.is_empty() {
            *self = Self::default();
            return IDLE;
        }
        let wait = if self.since.is_none() { 0 } else { WAIT_SECS };
        let answer = read(self.since, wait);
        if updates.is_closed() {
            return Duration::ZERO;
        }
        // A subscribe/unsubscribe can complete during the long poll. Publish
        // against the current exact identities, never the pre-wait snapshot.
        let selected = sonos(updates.subscriptions());
        if selected.uris.is_empty() {
            *self = Self::default();
            return IDLE;
        }
        let Ok(answer) = answer else {
            self.dirty = true;
            return RETRY;
        };
        let changed = self.since.is_none()
            || self.dirty
            || self.selected.subscribers != selected.subscribers
            || answer.reset.is_some()
            || self.since.is_some_and(|since| answer.last_id != since);
        // Establish the tail before the first invalidation. Any event racing
        // that initial notification remains visible to the next long poll.
        self.since = Some(answer.last_id);
        let uris = selected
            .uris
            .iter()
            .filter(|uri| changed || self.selected.uris.binary_search(uri).is_err())
            .cloned()
            .collect::<Vec<_>>();
        self.dirty = publish(updates, uris).is_err();
        if self.dirty {
            RETRY
        } else {
            self.selected = selected;
            IDLE
        }
    }
}

/// Split by both public budgets. A large house still gets every invalidation
/// without making a single oversized transport publication.
fn publish(updates: &ResourceUpdates, uris: Vec<String>) -> McpResult<()> {
    let mut batch = Vec::new();
    let mut bytes = 0;
    for uri in uris {
        if !batch.is_empty()
            && (batch.len() == MAX_RESOURCE_UPDATE_BATCH
                || bytes + uri.len() > MAX_RESOURCE_UPDATE_BATCH_BYTES)
        {
            updates.publish(&batch)?;
            batch.clear();
            bytes = 0;
        }
        bytes += uri.len();
        batch.push(uri);
    }
    if !batch.is_empty() {
        updates.publish(&batch)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
