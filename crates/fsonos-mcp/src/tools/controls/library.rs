//! One cached listing authority for agent browsing, search and playback.

use crate::tools::{Backend, respond};
use fastmcp::FinalCallToolResult;
use fastmcp::prelude::McpResult;
use fsonos_api::surface::dj_sync::library::BrowseQuery;

impl Backend {
    /// Use the daemon's cache when present, never silently switch caches after
    /// an error. Standalone mode uses the same Surface projection and policy.
    pub fn browse_library(
        &self,
        query: &BrowseQuery,
        expected_revision: Option<&str>,
    ) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = query.path()?;
            let page = self.read(&path, || self.surface.browse_library(&self.client, query))?;
            page.validate_for(query, expected_revision)?;
            Ok((page.text(), page))
        })
    }
}

#[cfg(test)]
mod tests;
