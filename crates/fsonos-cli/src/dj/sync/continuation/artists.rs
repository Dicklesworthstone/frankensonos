//! A full read admits a finite cohort of artist samples. Background passes
//! consume that cohort without replacing full-refresh or directory progress.

use super::{ARTIST_NOTE, Checkpoint};
use fsonos_api::surface::dj_sync::ArtistSamplingDto;
use fsonos_spotify::cache::ArtistContinuation;

impl Checkpoint {
    pub(super) fn artist_due(&self, now: i64) -> bool {
        !self.requires_refresh
            && !self.reconcile
            && now >= self.not_before
            && now >= self.artist_retry_at
            && self.artist_samples.as_ref().is_some_and(|samples| {
                samples.pending > 0
                    && !samples.limited
                    && !samples.requires_refresh
                    && samples.generation.is_some()
                    && samples.owner.as_deref().is_some_and(|owner| {
                        !owner.trim().is_empty()
                            && owner.len() <= 1_024
                            && !owner.chars().any(char::is_control)
                    })
            })
    }

    pub(super) fn reserve_artists(&self, now: i64) -> Self {
        let mut reserved = self.clone();
        reserved.artist_stalled = reserved.artist_stalled.saturating_add(1);
        reserved.artist_retry_at = now
            .saturating_add(Self::wait(reserved.artist_stalled))
            .max(reserved.not_before);
        reserved
    }

    pub(super) fn observed_artists(&mut self, report: &ArtistContinuation, now: i64) {
        if report.owner.is_none() && !report.requires_refresh {
            self.artist_samples = None;
            self.artist_stalled = 0;
            self.artist_retry_at = 0;
            return;
        }
        let progress = self.artist_samples.as_ref().is_none_or(|previous| {
            report.pending < previous.pending
                || report.completed > previous.completed
                || report.owner != previous.owner
                || report.generation != previous.generation
                || report.cycle != previous.cycle
        });
        self.artist_stalled = if progress || report.pending == 0 {
            0
        } else {
            self.artist_stalled.saturating_add(1)
        };
        self.artist_retry_at = now.saturating_add(Self::wait(self.artist_stalled));
        if report.requires_refresh {
            self.requires_refresh = true;
            self.retry_at = self.artist_retry_at;
        }
        self.artist_samples = Some(report.clone());
        self.cooldown(now, report.retry_after_secs);
    }

    pub(super) fn artist_note(&self) -> Option<String> {
        let samples = self.artist_samples.as_ref()?;
        if samples.requires_refresh {
            return Some(format!(
                "{ARTIST_NOTE}the Spotify account needs a full library refresh"
            ));
        }
        if samples.limited {
            return Some(format!(
                "{ARTIST_NOTE}the private cohort reached its cache limit; automatic sample reads are paused"
            ));
        }
        (samples.pending > 0 || samples.completed > 0).then(|| {
            format!(
                "{ARTIST_NOTE}{} artist sample(s) completed in this cohort, {} unfinished; {}",
                samples.completed,
                samples.pending,
                if samples.pending == 0 {
                    "the admitted cohort is complete"
                } else {
                    "the next bounded pass follows the saved retry deadline"
                }
            )
        })
    }
}

pub(super) fn artist_status(
    saved: &Checkpoint,
    running: bool,
    now: i64,
) -> Option<ArtistSamplingDto> {
    let samples = saved.artist_samples.as_ref()?;
    Some(ArtistSamplingDto {
        pending: samples.pending,
        completed: samples.completed,
        attempted: samples.attempted,
        limited: samples.limited,
        running,
        retry_at: ((samples.pending > 0 && !samples.limited) || samples.requires_refresh)
            .then_some(saved.artist_retry_at),
        not_before: (saved.not_before > now).then_some(saved.not_before),
        requires_refresh: samples.requires_refresh || saved.requires_refresh,
    })
}

#[cfg(test)]
mod tests;
