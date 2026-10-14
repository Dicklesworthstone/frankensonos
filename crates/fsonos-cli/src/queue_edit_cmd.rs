//! Revision-bound edits through the existing queue API, never clear/rebuild.
//! A saved `queue --json` page is an observation, not a source of destinations:
//! the caller names the room and the backend checks the snapshot's revision.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::queue::{
    MAX_PAGE, QueueDto, QueueMoveRequest, QueueMovedDto, QueueRemoveRequest, QueueRemovedDto,
};
use fsonos_core::policy::Client;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Args)]
pub struct QueueSelection {
    /// A room, Room@S1 / Room@S2, here, or a single-room alias.
    #[arg(value_parser = nonblank)]
    zone: String,
    /// First selected position in the ORIGINAL queue, starting at one.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    position: u32,
    /// Exact renderer URI from queue --json. Repeat for consecutive entries,
    /// in queue order, preserving duplicates. No comma splitting is performed.
    #[arg(
        long = "expected-uri",
        value_name = "URI",
        value_parser = renderer_uri,
        required_unless_present = "snapshot",
        conflicts_with = "snapshot"
    )]
    expected_uris: Vec<String>,
    /// Exact revision from the same queue observation as the selected URIs.
    #[arg(
        long,
        value_name = "REVISION",
        value_parser = nonblank,
        required_unless_present = "snapshot",
        conflicts_with = "snapshot"
    )]
    expected_revision: Option<String>,
    /// Read a saved queue --json page (at most 1 MiB); - reads standard input.
    /// The page must contain the entire selected range and its revision.
    #[arg(long, value_name = "FILE")]
    snapshot: Option<PathBuf>,
    /// With --snapshot, select this many consecutive entries (default: one).
    // clap waives `requires = "snapshot"` once an argument that conflicts
    // with --snapshot is present, so the explicit selection's arguments must
    // conflict with --count directly.
    #[arg(
        long,
        value_name = "N",
        requires = "snapshot",
        conflicts_with_all = ["expected_uris", "expected_revision"],
        value_parser = clap::value_parser!(u32).range(1..=100)
    )]
    count: Option<u32>,
}

#[derive(Debug, Args)]
pub struct QueueRemoveArgs {
    #[command(flatten)]
    selection: QueueSelection,
}

#[derive(Debug, Args)]
pub struct QueueMoveArgs {
    #[command(flatten)]
    selection: QueueSelection,
    /// Move before this ORIGINAL queue position; total + 1 moves to the end.
    /// Positions are never adjusted for the selected block by the CLI.
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    insert_before: u32,
}

fn nonblank(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("the value must not be blank".into())
    } else {
        Ok(value.to_owned())
    }
}

fn renderer_uri(value: &str) -> Result<String, String> {
    if value == "NOT_IMPLEMENTED" {
        Err("the queue entry has no renderer URI".into())
    } else {
        nonblank(value)
    }
}

fn read_snapshot(input: impl Read) -> Result<QueueDto, Failure> {
    let mut bytes = Vec::new();
    input
        .take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Failure::invalid(format!("cannot read queue snapshot: {e}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
        return Err(Failure::invalid("queue snapshot exceeds 1 MiB"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| Failure::invalid(format!("invalid queue --json snapshot: {e}")))
}

pub(crate) fn load_snapshot(path: &Path) -> Result<QueueDto, Failure> {
    if path == Path::new("-") {
        read_snapshot(std::io::stdin().lock())
    } else {
        let file = std::fs::File::open(path).map_err(|e| {
            Failure::invalid(format!(
                "cannot open queue snapshot {}: {e}",
                path.display()
            ))
        })?;
        read_snapshot(file)
    }
}

pub(crate) fn snapshot_selection(
    page: &QueueDto,
    position: u32,
    count: u32,
) -> Result<(Vec<String>, String), Failure> {
    let revision = page
        .revision
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .ok_or_else(|| Failure::invalid("the queue snapshot has no revision; read it again"))?;
    if position == 0 || !(1..=MAX_PAGE).contains(&count) {
        return Err(Failure::invalid(
            "select one to 100 entries starting at position one or later",
        ));
    }
    let last = position
        .checked_add(count - 1)
        .filter(|last| *last <= page.total)
        .ok_or_else(|| Failure::invalid("selected range exceeds the snapshot's queue length"))?;
    if page.items.len() > usize::try_from(MAX_PAGE).unwrap_or(100)
        || page
            .items
            .iter()
            .any(|i| i.position == 0 || i.position > page.total)
        || page
            .items
            .windows(2)
            .any(|w| w[0].position.checked_add(1) != Some(w[1].position))
    {
        return Err(Failure::invalid(
            "the snapshot is not a consecutive queue page",
        ));
    }
    let selected: Vec<_> = page
        .items
        .iter()
        .filter(|item| (position..=last).contains(&item.position))
        .collect();
    if selected.len() != usize::try_from(count).unwrap_or(usize::MAX) {
        return Err(
            Failure::invalid("the snapshot does not contain the entire selected range").with_hint(
                "Read queue --offset <position minus one> --limit <count> --json again.",
            ),
        );
    }
    let uris = selected
        .iter()
        .map(|item| {
            item.uri
                .as_deref()
                .ok_or_else(|| Failure::invalid("a selected queue entry has no renderer URI"))
                .and_then(|uri| renderer_uri(uri).map_err(Failure::invalid))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((uris, revision.to_owned()))
}

impl QueueSelection {
    fn resolve(&self, insert_before: Option<u32>) -> Result<(Vec<String>, String), Failure> {
        if let Some(path) = &self.snapshot {
            let page = load_snapshot(path)?;
            if insert_before.is_some_and(|at| u64::from(at) > u64::from(page.total) + 1) {
                return Err(Failure::invalid(
                    "move destination exceeds the original queue length plus one",
                ));
            }
            return snapshot_selection(&page, self.position, self.count.unwrap_or(1));
        }
        let count = u32::try_from(self.expected_uris.len())
            .map_err(|_| Failure::invalid("too many selected entries"))?;
        if !(1..=MAX_PAGE).contains(&count) || self.position.checked_add(count - 1).is_none() {
            return Err(Failure::invalid(
                "select one to 100 entries without overflowing queue positions",
            ));
        }
        let revision = self.expected_revision.clone().ok_or_else(|| {
            Failure::invalid("give --expected-revision with the selected renderer URIs")
        })?;
        Ok((self.expected_uris.clone(), revision))
    }
}

impl QueueRemoveArgs {
    fn request(&self) -> Result<QueueRemoveRequest, Failure> {
        let (expected_uris, revision) = self.selection.resolve(None)?;
        Ok(QueueRemoveRequest {
            zone: self.selection.zone.clone(),
            position: self.selection.position,
            expected_uris,
            expected_revision: Some(revision),
        })
    }
}

impl QueueMoveArgs {
    fn request(&self) -> Result<QueueMoveRequest, Failure> {
        let (expected_uris, expected_revision) =
            self.selection.resolve(Some(self.insert_before))?;
        Ok(QueueMoveRequest {
            zone: self.selection.zone.clone(),
            position: self.selection.position,
            insert_before: self.insert_before,
            expected_uris,
            expected_revision,
        })
    }
}

pub fn remove(global: &GlobalArgs, args: &QueueRemoveArgs) -> anyhow::Result<()> {
    // Resolve once, before even probing a daemon. Never silently reselect on error.
    let request = args.request()?;
    let result: QueueRemovedDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.post("/queue/remove", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli").queue_remove(&Client::Cli, &request)?
    };
    emit(global.json, &result, QueueRemovedDto::text)
}

pub fn move_entries(global: &GlobalArgs, args: &QueueMoveArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: QueueMovedDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.post("/queue/move", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli").queue_move(&Client::Cli, &request)?
    };
    emit(global.json, &result, QueueMovedDto::text)
}

#[cfg(test)]
pub(crate) mod fixture;

#[cfg(test)]
mod tests;
