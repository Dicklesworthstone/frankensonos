//! Add an ordered selection with one guarded, undoable queue operation.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::queue::{
    MAX_QUEUE_TRACKS, QueueAddedDto, QueueTrackInput, QueueTracksRequest,
};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct QueueTracksArgs {
    /// A room, Room@S1 / Room@S2, here, or a single-room alias.
    #[arg(value_parser = nonblank)]
    zone: String,
    /// Spotify track or episode URIs or share links, in the desired order.
    /// Supply 1-10; duplicate entries are retained. Containers are not accepted.
    #[arg(required = true, num_args = 1..=MAX_QUEUE_TRACKS, value_name = "URI", value_parser = nonblank)]
    source_uris: Vec<String>,
    /// Insert after the current item when the queue is selected; otherwise
    /// append. Neither mode starts playback or changes the selected source.
    #[arg(long)]
    next: bool,
    /// Exact destination queue revision from `queue --json`. Refuse if that
    /// queue changed; this is not a library or playlist revision.
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

impl QueueTracksArgs {
    fn request(&self) -> Result<QueueTracksRequest, Failure> {
        let request = QueueTracksRequest {
            zone: self.zone.clone(),
            tracks: self
                .source_uris
                .iter()
                .map(|source_uri| QueueTrackInput {
                    source_uri: source_uri.clone(),
                    title: None,
                })
                .collect(),
            next: self.next,
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn run(global: &GlobalArgs, args: &QueueTracksArgs) -> anyhow::Result<()> {
    // Reject the whole selection before probing a daemon or the LAN.
    let request = args.request()?;
    let added: QueueAddedDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        // A selected daemon owns the mutation, including ambiguous failures.
        remote.post("/queue/tracks", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        surface.queue_tracks(&Client::Cli, &request)?
    };
    emit(global.json, &added, QueueAddedDto::text)
}

#[cfg(test)]
mod tests;
