//! Ordinary controls share the daemon's ownership, policy, log and live state.

use super::{Backend, TransportAction, plan};
use fastmcp::FinalCallToolResult;
use fastmcp::prelude::McpResult;
use fsonos_api::{
    GroupRequest, MuteRequest, PartyRequest, PlayRequest, SeekRequest, VolumeRequest, ZoneRequest,
};

mod reviewed_undo;

impl Backend {
    /// Play through the daemon, or the same planner in standalone mode.
    pub fn play(&self, req: &PlayRequest) -> McpResult<FinalCallToolResult> {
        self.write("/play", req, || {
            self.control("play", |h| plan::plan_play(h, req))
        })
    }

    /// Pause/resume/next/previous through one authority, including room sets.
    pub fn transport(
        &self,
        req: &ZoneRequest,
        action: TransportAction,
    ) -> McpResult<FinalCallToolResult> {
        let (path, tool) = match action {
            TransportAction::Pause => ("/pause", "pause"),
            TransportAction::Resume => ("/resume", "resume"),
            TransportAction::Next => ("/next", "next"),
            TransportAction::Previous => ("/previous", "previous"),
            TransportAction::Seek(_) => {
                return Err(crate::tool_error(&fsonos_api::Failure::invalid(
                    "use the seek request for a position change",
                )));
            }
        };
        self.write(path, req, || {
            self.control_set(tool, |h| plan::plan_transport_set(h, req, action))
        })
    }

    /// Seek with the daemon's current playback observation.
    pub fn seek(&self, req: &SeekRequest) -> McpResult<FinalCallToolResult> {
        self.write("/seek", req, || {
            self.control("seek", |h| plan::plan_seek(h, req))
        })
    }

    /// Absolute and relative volume changes also supersede daemon-owned fades.
    pub fn set_volume(&self, req: &VolumeRequest) -> McpResult<FinalCallToolResult> {
        self.write("/volume", req, || {
            self.control_set("set_volume", |h| plan::plan_volume_set(h, req))
        })
    }

    /// Mute a room or set with the same policy and undo owner.
    pub fn mute(&self, req: &MuteRequest) -> McpResult<FinalCallToolResult> {
        self.write("/mute", req, || {
            self.control_set("mute", |h| plan::plan_mute_set(h, req))
        })
    }

    /// Group using the daemon's topology and session ownership.
    pub fn group(&self, req: &GroupRequest) -> McpResult<FinalCallToolResult> {
        self.write("/group", req, || {
            self.control("group", |h| plan::plan_group(h, req))
        })
    }

    /// Ungroup one room or a complete target set.
    pub fn ungroup(&self, req: &ZoneRequest) -> McpResult<FinalCallToolResult> {
        self.write("/ungroup", req, || {
            self.control_set("ungroup", |h| plan::plan_ungroup_set(h, req))
        })
    }

    /// Party mode uses the same authority as individual group edits.
    pub fn group_all(&self, req: &PartyRequest) -> McpResult<FinalCallToolResult> {
        self.write("/party", req, || {
            self.control("group_all", |h| plan::plan_party(h, req))
        })
    }
}
