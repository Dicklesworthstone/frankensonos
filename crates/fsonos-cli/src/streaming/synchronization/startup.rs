//! One explicit audible calibration before the ordinary passive mpv loop.
//! All windows must belong to one uninterrupted playback generation.

use anyhow::{Context as _, ensure};

use super::{Options, Stop, capture, measurement, mpv, probe};

pub(super) fn run(
    controller: &mut mpv::Controller,
    options: &Options,
    stop: &Stop,
    probe: probe::Config,
    verify: impl FnMut(bool) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    with_capture(
        controller,
        options.max_lag_ms,
        stop,
        probe,
        verify,
        |controller, window, probe| {
            capture::record_guarded(options, stop, Some(probe), || {
                uninterrupted(controller, window)
            })
        },
    )
}

fn uninterrupted(controller: &mut mpv::Controller, window: mpv::Window) -> anyhow::Result<()> {
    ensure!(
        controller.finish_window(window)?,
        "mpv playback changed during startup calibration; probe not restarted"
    );
    Ok(())
}

fn with_capture(
    controller: &mut mpv::Controller,
    max_lag_ms: u32,
    stop: &Stop,
    mut probe: probe::Config,
    mut verify: impl FnMut(bool) -> anyhow::Result<bool>,
    mut record: impl FnMut(
        &mut mpv::Controller,
        mpv::Window,
        &probe::Config,
    ) -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<()> {
    ensure!(
        !stop.requested(),
        "startup calibration cancelled or expired"
    );
    let window = controller
        .begin_window()?
        .context("start mpv playback before requesting audible startup calibration")?;
    // The existing estimator and three-window agreement remain unchanged.
    // Config supplies the stricter three-attempt audible budget, consumed once.
    let measured = measurement::collect(max_lag_ms, stop, || {
        ensure!(
            verify(false)? && !stop.requested(),
            "startup room check cancelled"
        );
        uninterrupted(controller, window)?;
        probe.reserve_window()?;
        let bytes = record(controller, window, &probe)?;
        if bytes.is_some() && !stop.requested() {
            ensure!(
                verify(true)? && !stop.requested(),
                "startup room check cancelled"
            );
            uninterrupted(controller, window)?;
        }
        Ok(bytes)
    })?;
    // Recheck after estimation, before touching the player's delay. No audio
    // process remains here: record_guarded has already reaped both children.
    ensure!(
        verify(true)? && !stop.requested(),
        "startup correction cancelled"
    );
    uninterrupted(controller, window)?;
    ensure!(!stop.requested(), "startup correction cancelled");
    let lag = measured.estimate.seconds();
    if controller.apply(lag, window)? {
        eprintln!("fsonos sync: startup acoustic lag {lag:.3} s; mpv correction confirmed");
    } else {
        // A within-hysteresis result is valid; an interrupted apply is not.
        uninterrupted(controller, window)?;
        eprintln!(
            "fsonos sync: startup acoustic lag {lag:.3} s; existing delay is within tolerance"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
