//! Owned scratch directory for queue snapshot tests; no extra test dependency.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct Directory(PathBuf);

impl Directory {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let path = std::env::temp_dir().join(format!(
            "fsonos-queue-tests-{}-{}-{serial}",
            std::process::id(),
            epoch.as_nanos()
        ));
        // A collision fails before ownership is acquired: never reuse or clean
        // somebody else's directory. All test contents are synthetic.
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
