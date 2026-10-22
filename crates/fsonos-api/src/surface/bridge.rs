//! What the legacy Sonos bridge's HTTP API reads, in its shapes (vg3s.9).
//! [`crate::http::bridge`] serves them; its writes go through
//! [`Surface::control_set`] like every other caller's, so the house policy,
//! the action log and undo cover them.

use super::{Surface, room_view};
use crate::failure::Failure;
use crate::plan::resolve;
use fsonos_core::favorites;
use fsonos_core::policy::Client;
use fsonos_proto::control as soap;
use fsonos_types::PlayerId;
use serde_json::{Value, json};

/// The most favorites the bridge listed.
const MAX_FAVORITES: usize = 30;
/// The longest favorite DIDL it returned, in characters.
const MAX_FAVORITE_META: usize = 1500;
/// The longest current-URI DIDL it returned, in characters.
const MAX_MEDIA_META: usize = 2000;

impl Surface {
    /// How many players the house has (`GET /health`'s `devices`): none
    /// while the first survey has found none, as the bridge answered then.
    pub(crate) fn bridge_devices(&self) -> usize {
        self.households().map_or(0, |households| {
            households.iter().map(|h| h.players.len()).sum()
        })
    }

    /// The room names a caller can use, sorted (an unknown zone's `known`).
    pub(crate) fn bridge_known(&self) -> Vec<String> {
        let mut known: Vec<String> = self
            .households()
            .unwrap_or_default()
            .iter()
            .flat_map(|h| h.rooms.iter().map(|r| r.name.clone()))
            .collect();
        known.sort();
        known.dedup();
        known
    }

    /// The room that coordinates `zone`'s group: the name the bridge
    /// answered a write with.
    pub(crate) fn bridge_coordinator(
        &self,
        client: &Client,
        zone: &str,
    ) -> Result<String, Failure> {
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        Ok(target.coordinator.room_name.clone())
    }

    /// Every player with its group (`GET /zones`).
    pub(crate) fn bridge_zones(&self, client: &Client) -> Result<Value, Failure> {
        let zones = self.zones(client)?;
        let households = self.households()?;
        let state = |room: &str| {
            zones
                .iter()
                .find(|z| z.coordinator_room == room)
                .map_or("UNKNOWN", |z| upnp_state(&z.transport_state))
        };
        let mut listed = Vec::new();
        for household in &households {
            let player = |id: &PlayerId| household.players.iter().find(|p| p.id == *id);
            for group in &household.groups {
                let Some(coordinator) = player(&group.coordinator) else {
                    continue;
                };
                // A stereo pair's players share a room name: list it once.
                let mut members: Vec<&str> = Vec::new();
                for member in group.members.iter().filter_map(&player) {
                    if !members.contains(&member.room_name.as_str()) {
                        members.push(&member.room_name);
                    }
                }
                for member in group.members.iter().filter_map(&player) {
                    listed.push(json!({
                        "name": member.room_name,
                        "ip": member.ip.to_string(),
                        "model": member.model,
                        "is_coordinator": member.id == group.coordinator,
                        "coordinator": coordinator.room_name,
                        "group_members": members,
                        "transport_state": state(&coordinator.room_name),
                    }));
                }
            }
        }
        Ok(json!({ "zones": listed }))
    }

    /// What `zone`'s group plays (`GET /status`).
    pub(crate) fn bridge_status(&self, client: &Client, zone: &str) -> Result<Value, Failure> {
        let state = self.zone_state(client, zone)?;
        let track = state.track.as_ref();
        let text = |field: Option<&String>| field.cloned().unwrap_or_default();
        Ok(json!({
            "zone": state.zone.coordinator_room,
            "transport_state": upnp_state(&state.transport_state),
            "title": text(track.and_then(|t| t.title.as_ref())),
            "artist": text(track.and_then(|t| t.creator.as_ref())),
            "album": text(track.and_then(|t| t.album.as_ref())),
            "position": clock(track.and_then(|t| t.position_secs)),
            "duration": clock(track.and_then(|t| t.duration_secs)),
            "volume": state.volume,
            "muted": state.mute,
        }))
    }

    /// The favorites of `zone`'s household, with what plays them
    /// (`GET /favorites`).
    pub(crate) fn bridge_favorites(&self, client: &Client, zone: &str) -> Result<Value, Failure> {
        self.guard(client).authorize("list_favorites", true)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let listed = favorites::list(&*self.transport, &households, &target.coordinator.id)?;
        let favorites: Vec<Value> = listed
            .iter()
            .take(MAX_FAVORITES)
            .map(|f| {
                json!({
                    "title": f.title,
                    "uri": f.uri,
                    "meta": clip(&f.metadata, MAX_FAVORITE_META),
                })
            })
            .collect();
        Ok(json!({ "zone": target.coordinator.room_name, "favorites": favorites }))
    }

    /// What `zone`'s group is set to play (`GET /mediainfo`).
    pub(crate) fn bridge_media_info(&self, client: &Client, zone: &str) -> Result<Value, Failure> {
        self.guard(client).authorize("get_zone_state", true)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let media = soap::get_media_info(&*self.transport, target.coordinator.ip)
            .map_err(|e| Failure::from(fsonos_core::CoreError::from(e)))?;
        Ok(json!({
            "zone": target.coordinator.room_name,
            "current_uri": media.uri,
            "current_uri_metadata": clip(&media.uri_metadata, MAX_MEDIA_META),
        }))
    }

    /// Survey the speakers again now (`POST /rediscover`): how many players
    /// it found.
    pub(crate) fn bridge_rediscover(&self, client: &Client) -> Result<usize, Failure> {
        self.guard(client).authorize("list_zones", true)?;
        self.invalidate();
        let households = self.resurvey().map_or_else(|| self.households(), Ok)?;
        Ok(households.iter().map(|h| h.players.len()).sum())
    }
}

/// A transport state as UPnP (and so the bridge) names it.
fn upnp_state(state: &str) -> &'static str {
    match state {
        "playing" => "PLAYING",
        "paused" => "PAUSED_PLAYBACK",
        "stopped" => "STOPPED",
        "transitioning" => "TRANSITIONING",
        _ => "UNKNOWN",
    }
}

/// Seconds as UPnP writes a track time, `H:MM:SS` (`0:00:00` when unknown).
fn clock(secs: Option<u32>) -> String {
    let secs = secs.unwrap_or(0);
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// The first `max` characters of `text`.
fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_and_times_read_as_the_bridge_wrote_them() {
        assert_eq!(upnp_state("playing"), "PLAYING");
        assert_eq!(upnp_state("paused"), "PAUSED_PLAYBACK");
        assert_eq!(upnp_state("stopped"), "STOPPED");
        assert_eq!(upnp_state("transitioning"), "TRANSITIONING");
        assert_eq!(upnp_state("unknown"), "UNKNOWN");
        assert_eq!(clock(None), "0:00:00");
        assert_eq!(clock(Some(83)), "0:01:23");
        assert_eq!(clock(Some(3 * 3600 + 5 * 60 + 9)), "3:05:09");
        assert_eq!(clip("añb", 2), "añ");
        assert_eq!(clip("ab", 5), "ab");
    }
}
