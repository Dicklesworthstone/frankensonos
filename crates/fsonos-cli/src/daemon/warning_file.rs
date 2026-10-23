//! Keeps the daemon's warning counts ([`fsonos_core::warnings`]) across a
//! restart in the data directory's `warnings.json` (co55.7), so a kind's
//! usual hour, the baseline a storm is judged against, survives one. A
//! chronically noisy kind would otherwise read as a storm for an hour after
//! every start.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The file in the data directory.
pub const FILE: &str = "warnings.json";
/// How often the counts are written while the daemon runs.
const EVERY: Duration = Duration::from_secs(600);

/// Restore the counts kept in `data_dir`, then keep them there every ten
/// minutes until `stop` is set; [`finish`] writes them once more.
pub fn keep(data_dir: &Path, stop: Arc<AtomicBool>) -> Option<JoinHandle<()>> {
    let path = data_dir.join(FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if let Err(e) = fsonos_core::warnings::restore_json(&text) {
                tracing::warn!("the kept warning counts were not read: {e}");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("the kept warning counts were not read: {e}"),
    }
    let spawned = thread::Builder::new()
        .name("fsonos-warning-counts".into())
        .spawn(move || {
            let mut saved = Instant::now();
            while !stop.load(Ordering::Acquire) {
                if saved.elapsed() >= EVERY {
                    save(&path);
                    saved = Instant::now();
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    spawned
        .inspect_err(|e| tracing::warn!("the warning counts are kept only when serve stops: {e}"))
        .ok()
}

/// Once the daemon has stopped: wait for the saver [`keep`] started, then
/// write the counts a last time, the warnings of the shutdown with them.
pub fn finish(data_dir: &Path, saver: Option<JoinHandle<()>>) {
    if saver.is_some_and(|saver| saver.join().is_err()) {
        tracing::warn!("the warning counts' saver stopped with a panic");
    }
    save(&data_dir.join(FILE));
}

/// Write the counts to `path` through a temporary file beside it.
fn save(path: &Path) {
    let text = fsonos_core::warnings::to_json();
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        tracing::warn!("the warning counts were not kept: {e}");
    }
}
