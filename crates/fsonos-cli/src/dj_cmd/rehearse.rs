//! `fsonos dj rehearse`: the set the DJ would play (`dj_rehearse`).

use fsonos_api::surface::dj_rehearse::DjRehearseRequest;

use super::ConstraintArgs;

/// `fsonos dj rehearse`: the fields of `dj_rehearse`.
#[derive(clap::Args)]
pub struct RehearseArgs {
    /// The room; its group's DJ rehearses.
    zone: String,
    /// How many works, 1 to 500 [default: 12].
    #[arg(long, conflicts_with = "hours")]
    works: Option<u32>,
    /// Instead, the works that start within this many hours, 1 to 48.
    #[arg(long)]
    hours: Option<u32>,
    /// When the set starts: HH:MM (its next time) or an RFC 3339 time
    /// [default: now].
    #[arg(long)]
    at: Option<String>,
    /// Rehearse as if the group were steered by this mood (fsonos dj moods
    /// lists them), with any constraints below laid over it, in place of
    /// its steering and the time-of-day program.
    #[arg(long)]
    mood: Option<String>,
    #[command(flatten)]
    constraints: ConstraintArgs,
    /// The same number rehearses the same set [default: 1]. (`--seed` names
    /// a player address for discovery.)
    #[arg(long = "pick-seed", value_name = "N")]
    pick_seed: Option<u64>,
}

impl RehearseArgs {
    #[must_use]
    pub fn request(&self) -> DjRehearseRequest {
        DjRehearseRequest {
            zone: self.zone.clone(),
            works: self.works,
            hours: self.hours,
            at: self.at.clone(),
            mood: self.mood.clone(),
            constraints: self.constraints.constraints(),
            seed: self.pick_seed,
        }
    }
}
