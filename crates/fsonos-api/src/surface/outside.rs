//! Actions a caller carries out itself, outside the planned paths, logged
//! like any other: `fsonos setup --test-play`'s short test play, and the
//! restore at the end of `fsonos stream`.

use fsonos_core::policy::Client;

use super::Surface;

impl Surface {
    /// Log an action `client` carried out itself (allowed, not undoable):
    /// `intent` says what, `result` how it went.
    pub fn log_done(&self, client: &Client, intent: String, result: String) {
        self.record(client, intent, "allow".into(), result, None);
    }
}
