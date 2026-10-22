//! Find identified occurrences in a group's existing queue. Each invocation
//! reads one bounded section; continuing keeps the previous queue revision.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::queue::{QueueSearchDto, QueueSearchRequest};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct QueueSearchArgs {
    /// A room, Room@S1 / Room@S2, here, or a single-room alias.
    #[arg(value_parser = nonblank)]
    zone: String,
    /// Title, artist or album words. Quote a multi-word search.
    #[arg(value_parser = nonblank)]
    query: String,
    /// Continue at the previous result's next_offset, with its revision.
    #[arg(long, default_value_t = 0)]
    offset: u32,
    /// Maximum matches (1-200). A bounded scan can return fewer with more
    /// queue positions still to examine; check complete and next_offset.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=200))]
    limit: u32,
    /// Exact revision from the previous search. Required when offset > 0.
    /// A changed queue must be searched afresh from offset zero.
    #[arg(long, value_name = "REVISION", value_parser = nonblank)]
    expected_revision: Option<String>,
}

fn nonblank(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("the value must not be blank".into())
    } else {
        Ok(value.to_owned())
    }
}

impl QueueSearchArgs {
    fn request(&self) -> Result<QueueSearchRequest, Failure> {
        let request = QueueSearchRequest {
            zone: self.zone.clone(),
            query: self.query.clone(),
            offset: self.offset,
            limit: self.limit,
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn run(global: &GlobalArgs, args: &QueueSearchArgs) -> anyhow::Result<()> {
    // Reject invalid continuations before probing the daemon or the LAN.
    let request = args.request()?;
    let path = request.path()?;
    let found: QueueSearchDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        // Once selected, the daemon remains the source of this observation.
        remote.get(&path)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        surface.queue_search(&Client::Cli, &request)?
    };
    emit(global.json, &found, QueueSearchDto::text)
}

#[cfg(test)]
mod tests;
