//! Resource documents built from more than one surface read.

use fsonos_api::{DjMoodsDto, DjStatusDto, Failure};
use serde::Serialize;

use super::{Backend, from_daemon, json_document, segment};

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
        let zones = self.surface.zones(&self.client)?;
        if let Some(daemon) = &self.daemon
            && let Some(moods) = daemon.call("GET", "/dj/moods", None)
        {
            let moods: DjMoodsDto = from_daemon(moods, "GET", "/dj/moods")?;
            let mut statuses = Vec::with_capacity(zones.len());
            for zone in &zones {
                let path = format!("/zones/{}/dj", segment(&zone.coordinator_room));
                let answer = daemon.call("GET", &path, None).ok_or_else(|| {
                    Failure::new(
                        fsonos_api::ErrorCode::NotReady,
                        "the daemon stopped answering; read sonos://dj again",
                    )
                })?;
                statuses.push(from_daemon(answer, "GET", &path)?);
            }
            return json_document(&DjDocument {
                zones: statuses,
                moods,
            });
        }
        let mut statuses = Vec::with_capacity(zones.len());
        for zone in &zones {
            statuses.push(
                self.surface
                    .dj_status(&self.client, &zone.coordinator_room)?,
            );
        }
        let moods = self.surface.dj_moods(&self.client, None)?;
        json_document(&DjDocument {
            zones: statuses,
            moods,
        })
    }
}
