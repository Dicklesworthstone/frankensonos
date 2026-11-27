//! Copy exact native queue occurrences through the selected daemon once.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::queue::{QueueSelectionDto, QueueSelectionRequest};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct QueueSelectionArgs {
    /// Original room/group used to read queue --json or queue-search.
    pub zone: String,
    /// One to ten one-based occurrences, in the desired order; repeats are kept.
    #[arg(required = true, num_args = 1..=10, value_parser = clap::value_parser!(u32).range(1..))]
    pub positions: Vec<u32>,
    /// Destination room, alias or all@S1 / all@S2 within the source household.
    /// Defaults to the source group. Already grouped rooms receive one copy.
    #[arg(long)]
    pub target: Option<String>,
    /// Source queue revision from queue --json or queue-search.
    #[arg(
        long = "revision",
        visible_alias = "expected-revision",
        value_name = "REVISION"
    )]
    pub expected_revision: String,
    /// Play replaces and starts; next and append preserve playback and volume.
    #[arg(long, value_parser = ["play", "next", "append"])]
    pub mode: String,
    /// Optional physical destination audience revision from resolve-rooms.
    #[arg(
        long = "target-revision",
        visible_alias = "expected-target-revision",
        value_name = "REVISION"
    )]
    pub expected_target_revision: Option<String>,
    /// Optional destination queue revision, only for one destination group.
    #[arg(
        long = "queue-revision",
        visible_alias = "expected-queue-revision",
        value_name = "REVISION"
    )]
    pub expected_queue_revision: Option<String>,
}

impl QueueSelectionArgs {
    fn request(&self) -> Result<QueueSelectionRequest, Failure> {
        let request = QueueSelectionRequest {
            zone: self.zone.clone(),
            target: self.target.clone(),
            positions: self.positions.clone(),
            expected_revision: self.expected_revision.clone(),
            mode: self.mode.parse()?,
            expected_target_revision: self.expected_target_revision.clone(),
            expected_queue_revision: self.expected_queue_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn run(global: &GlobalArgs, args: &QueueSelectionArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let selected: QueueSelectionDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.post("/queue/selection", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        surface.queue_selection(&Client::Cli, &request)?
    };
    emit(global.json, &selected, QueueSelectionDto::text)?;
    if !selected.complete() {
        anyhow::bail!(
            "the native queue selection did not complete on every group; inspect per-group receipts before another request"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
