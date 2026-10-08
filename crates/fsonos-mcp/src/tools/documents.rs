//! Resource documents built from more than one surface read.

use fsonos_api::{DjMoodsDto, DjStatusDto, Failure};
use serde::Serialize;

use super::{Backend, json_document};

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
    /// `sonos://dj`: the DJ in every zone, and its moods and program.
    pub fn dj_document(&self) -> Result<String, Failure> {
        let zones = self.surface.zones(&self.client)?;
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
