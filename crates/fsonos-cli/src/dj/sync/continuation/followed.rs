//! Followed-artist directory intake has its own deadline and reconciliation.

use super::{Checkpoint, FOLLOWED_NOTE, FollowedArtistContinuation};
use fsonos_api::surface::dj_sync::{FollowedArtistsDto, FollowedArtistsPhase};
use fsonos_spotify::taste::FollowedArtistPhase;

impl Checkpoint {
    pub(super) fn followed_due(&self, now: i64) -> Option<String> {
        let followed = self.followed.as_ref()?;
        if self.requires_refresh
            || self.reconcile
            || followed.requires_refresh
            || followed.limited
            || !followed.pending
            || followed.ready()
            || now < self.not_before
            || now < self.followed_retry_at
        {
            return None;
        }
        followed.owner.clone().filter(|owner| {
            !owner.trim().is_empty() && owner.len() <= 1_024 && !owner.chars().any(char::is_control)
        })
    }

    pub(super) fn reserve_followed(&self, now: i64) -> Self {
        let mut reserved = self.clone();
        reserved.followed_stalled = reserved.followed_stalled.saturating_add(1);
        reserved.followed_retry_at = now
            .saturating_add(Self::wait(reserved.followed_stalled))
            .max(reserved.not_before);
        reserved
    }

    pub(super) fn observed_followed(&mut self, report: &FollowedArtistContinuation, now: i64) {
        if report.owner.is_none() && !report.requires_refresh {
            // A grant without this scope made no directory observation. Do not
            // expose an empty default report as a completed membership read.
            self.followed = None;
            self.followed_stalled = 0;
            self.followed_retry_at = 0;
            return;
        }
        let progress = report.advanced > 0
            || self.followed.as_ref().is_none_or(|previous| {
                report.owner != previous.owner
                    || report.generation != previous.generation
                    || report.pages != previous.pages
                    || report.verified_pages != previous.verified_pages
                    || report.phase != previous.phase
            });
        self.followed_stalled = if progress || !report.pending {
            0
        } else {
            self.followed_stalled.saturating_add(1)
        };
        self.followed_retry_at = now.saturating_add(Self::wait(self.followed_stalled));
        if report.requires_refresh {
            self.requires_refresh = true;
            self.retry_at = self.followed_retry_at;
        } else if report.ready() {
            // The directory is complete, but only a successful full refresh
            // can reconcile it with saves, playlists and the other signals.
            self.reconcile = true;
            // Reusing the same ready directory while another taste source
            // keeps failing must not force a full reread every check forever.
            self.retry_at = self.followed_retry_at;
        }
        self.followed = Some(report.clone());
        self.cooldown(now, report.retry_after_secs);
    }

    pub(super) fn followed_note(&self) -> Option<String> {
        let followed = self.followed.as_ref()?;
        if followed.requires_refresh {
            return Some(format!(
                "{FOLLOWED_NOTE}the Spotify account needs a full library refresh"
            ));
        }
        let state = if followed.limited {
            "the private cache limit was reached; membership remains incomplete"
        } else if followed.ready() {
            "verified directory awaits full library reconciliation"
        } else if followed.pending {
            "the next bounded pass follows the saved retry deadline"
        } else {
            return None;
        };
        Some(format!(
            "{FOLLOWED_NOTE}{} artist(s), {} page(s), {} verified; {state}",
            followed.artists, followed.pages, followed.verified_pages
        ))
    }
}

pub(super) fn followed_status(
    saved: &Checkpoint,
    running: bool,
    now: i64,
) -> Option<FollowedArtistsDto> {
    let followed = saved.followed.as_ref()?;
    Some(FollowedArtistsDto {
        pending: followed.pending,
        pages: followed.pages,
        artists: followed.artists,
        verified_pages: followed.verified_pages,
        phase: match followed.phase {
            FollowedArtistPhase::Reading => FollowedArtistsPhase::Reading,
            FollowedArtistPhase::Verifying => FollowedArtistsPhase::Verifying,
            FollowedArtistPhase::Ready => FollowedArtistsPhase::Ready,
        },
        limited: followed.limited,
        running,
        retry_at: (followed.pending || followed.requires_refresh).then_some(if followed.ready() {
            saved.retry_at
        } else {
            saved.followed_retry_at
        }),
        not_before: (saved.not_before > now).then_some(saved.not_before),
        requires_refresh: followed.requires_refresh || saved.requires_refresh,
    })
}

#[cfg(test)]
mod tests;
