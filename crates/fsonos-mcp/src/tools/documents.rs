//! Resource documents built from more than one surface read.

use fsonos_api::{DjMoodsDto, DjStatusDto, ErrorCode, Failure, ZoneDto};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{Backend, Daemon, from_daemon, json_document, segment};

/// `sonos://dj`.
#[derive(Serialize)]
struct DjDocument {
    /// The DJ in each zone, in `list_zones` order: whether it feeds the
    /// queue, what it plays and plays next, and how it is steered.
    zones: Vec<DjStatusDto>,
    /// The moods it can be steered to, and the time-of-day program that
    /// steers it otherwise.
    moods: DjMoodsDto,
}

impl Backend {
    /// `sonos://dj`: the DJ in every zone, and its moods and program. While
    /// a daemon runs, its DJ is the one that plays, so the statuses and moods
    /// are its own (as `dj_status` reads them).
    pub fn dj_document(&self) -> Result<String, Failure> {
        if let Some(daemon) = &self.daemon
            && let Some(zones) = daemon.call("GET", "/zones", None)
        {
            // The selected daemon owns the topology as well as the DJ. A
            // stdio agent may be off the speaker LAN, and even on it a local
            // survey can disagree with the model its next command uses.
            let zones: Vec<ZoneDto> = from_daemon(zones, "GET", "/zones")?;
            let moods = daemon_read(&**daemon, "/dj/moods")?;
            let mut statuses = Vec::with_capacity(zones.len());
            for zone in &zones {
                let room = format!("{}@{}", zone.coordinator_room, zone.household);
                statuses.push(daemon_read(
                    &**daemon,
                    &format!("/zones/{}/dj", segment(&room)),
                )?);
            }
            return json_document(&DjDocument {
                zones: statuses,
                moods,
            });
        }
        let zones = self.surface.zones(&self.client)?;
        let mut statuses = Vec::with_capacity(zones.len());
        for zone in &zones {
            let room = format!("{}@{}", zone.coordinator_room, zone.household);
            statuses.push(self.surface.dj_status(&self.client, &room)?);
        }
        let moods = self.surface.dj_moods(&self.client, None)?;
        json_document(&DjDocument {
            zones: statuses,
            moods,
        })
    }
}

/// After the first daemon read, every missing, failed or malformed reply is
/// terminal. A resource must never splice two different houses together.
fn daemon_read<T: DeserializeOwned>(daemon: &dyn Daemon, path: &str) -> Result<T, Failure> {
    let answer = daemon.call("GET", path, None).ok_or_else(|| {
        Failure::new(
            ErrorCode::NotReady,
            "the daemon stopped answering; read sonos://dj again",
        )
    })?;
    from_daemon(answer, "GET", path)
}

#[cfg(test)]
mod tests;
