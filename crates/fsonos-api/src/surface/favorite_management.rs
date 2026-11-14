//! Author My Sonos favorites from observed household metadata. Catalog writes
//! run once and leave the playing source, queue, levels and DJ ownership alone.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{control, favorites};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::Surface;
use crate::request::zone_name;
use crate::{ErrorCode, Failure, FavoriteDto};

/// Read the exact household-local favorite before changing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectFavoriteRequest {
    pub zone: String,
    /// Exact FV:2/<n> id from list_favorites, never a title or catalog number.
    pub favorite: String,
}

impl InspectFavoriteRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        native_id(&self.favorite)
    }

    pub fn path(&self) -> Result<String, Failure> {
        self.validate()?;
        Ok(format!(
            "/favorites/inspect?zone={}&favorite={}",
            segment(self.zone.trim()),
            segment(self.favorite.trim()),
        ))
    }
}

/// Encode a current-source preview for the selected daemon.
pub fn current_path(zone: &str) -> Result<String, Failure> {
    zone_name("zone", zone)?;
    Ok(format!("/favorites/current?zone={}", segment(zone.trim())))
}

fn segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn native_id(id: &str) -> Result<(), Failure> {
    if id.trim().len() > 1024
        || !id.trim().strip_prefix("FV:2/").is_some_and(|number| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(Failure::invalid(
            "favorite changes require an exact household-local FV:2/<n> id",
        ));
    }
    Ok(())
}

fn revision(value: &str) -> Result<(), Failure> {
    if value.trim().is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(Failure::invalid(
            "expected_revision must be the nonempty observed revision, at most 4096 bytes without control characters",
        ));
    }
    Ok(())
}

/// The speaker supplies renderer metadata; callers supply only a reviewed
/// source revision and the name to store in this household's favorites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveFavoriteRequest {
    pub zone: String,
    pub expected_revision: String,
    pub title: String,
}

impl SaveFavoriteRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        revision(&self.expected_revision)?;
        favorites::validate_title(&self.title).map_err(Failure::invalid)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameFavoriteRequest {
    pub zone: String,
    pub favorite: String,
    pub expected_revision: String,
    pub title: String,
}

impl RenameFavoriteRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        validate_change(&self.zone, &self.favorite, &self.expected_revision)?;
        favorites::validate_title(&self.title).map_err(Failure::invalid)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveFavoriteRequest {
    pub zone: String,
    pub favorite: String,
    pub expected_revision: String,
}

impl RemoveFavoriteRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        validate_change(&self.zone, &self.favorite, &self.expected_revision)
    }
}

fn validate_change(zone: &str, favorite: &str, expected: &str) -> Result<(), Failure> {
    zone_name("zone", zone)?;
    native_id(favorite)?;
    revision(expected)
}

/// An observed native entry, or an eligible current track/station. A current
/// source has an empty favorite.id until Sonos confirms its saved identity.
/// No service account descriptor, renderer URI or DIDL is exposed here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ObservedFavoriteDto {
    pub zone: String,
    pub favorite: FavoriteDto,
    pub revision: String,
}

impl ObservedFavoriteDto {
    #[must_use]
    pub fn text(&self) -> String {
        let identity = if self.favorite.id.is_empty() {
            "current source".into()
        } else {
            self.favorite.id.clone()
        };
        format!(
            "{}: {:?} ({}, {})\nrevision: {}\n",
            self.zone, self.favorite.title, self.favorite.kind, identity, self.revision,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FavoriteOperation {
    Save,
    Rename,
    Remove,
}

/// Confirmed native catalog result. A separate local logging failure cannot
/// authorize repeating a completed speaker write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ManagedFavoriteDto {
    pub done: String,
    pub zone: String,
    /// The saved/renamed entry, or the exact observed entry removed.
    pub favorite: FavoriteDto,
    pub operation: FavoriteOperation,
    pub changed: bool,
    pub logged: bool,
}

impl ManagedFavoriteDto {
    #[must_use]
    pub fn text(&self) -> String {
        if self.logged {
            format!("{}\n", self.done)
        } else {
            format!(
                "{}\nThe favorite result was confirmed, but the action log could not be written; do not repeat this operation.\n",
                self.done,
            )
        }
    }
}

impl Surface {
    pub fn current_favorite(
        &self,
        client: &Client,
        zone: &str,
    ) -> Result<ObservedFavoriteDto, Failure> {
        let guard = self.guard(client);
        guard.authorize("list_favorites", true)?;
        guard.authorize("get_zone_state", true)?;
        zone_name("zone", zone)?;
        self.observe_favorite(client, zone.trim(), None)
    }

    pub fn inspect_favorite(
        &self,
        client: &Client,
        req: &InspectFavoriteRequest,
    ) -> Result<ObservedFavoriteDto, Failure> {
        self.guard(client).authorize("list_favorites", true)?;
        req.validate()?;
        self.observe_favorite(client, req.zone.trim(), Some(req.favorite.trim()))
    }

    fn observe_favorite(
        &self,
        client: &Client,
        zone: &str,
        favorite: Option<&str>,
    ) -> Result<ObservedFavoriteDto, Failure> {
        let (households, group) = self.queue_households(client, zone)?;
        let player = control::locate(&households, &group.coordinator)?;
        let observed = match favorite {
            Some(id) => favorites::inspect(&*self.transport, &households, &group.coordinator, id)?,
            None => favorites::current(&*self.transport, &households, &group.coordinator)?,
        };
        Ok(ObservedFavoriteDto {
            zone: player.room_name.clone(),
            favorite: FavoriteDto::from(&observed.favorite),
            revision: observed.revision,
        })
    }

    pub fn save_favorite(
        &self,
        client: &Client,
        req: &SaveFavoriteRequest,
    ) -> Result<ManagedFavoriteDto, Failure> {
        self.authorize_logged(client, "save_favorite")?;
        req.validate()?;
        self.change_favorite(
            client,
            &req.zone,
            FavoriteOperation::Save,
            None,
            &req.expected_revision,
            Some(req.title.trim()),
        )
    }

    pub fn rename_favorite(
        &self,
        client: &Client,
        req: &RenameFavoriteRequest,
    ) -> Result<ManagedFavoriteDto, Failure> {
        self.authorize_logged(client, "rename_favorite")?;
        req.validate()?;
        self.change_favorite(
            client,
            &req.zone,
            FavoriteOperation::Rename,
            Some(req.favorite.trim()),
            &req.expected_revision,
            Some(req.title.trim()),
        )
    }

    pub fn remove_favorite(
        &self,
        client: &Client,
        req: &RemoveFavoriteRequest,
    ) -> Result<ManagedFavoriteDto, Failure> {
        self.authorize_logged(client, "remove_favorite")?;
        req.validate()?;
        self.change_favorite(
            client,
            &req.zone,
            FavoriteOperation::Remove,
            Some(req.favorite.trim()),
            &req.expected_revision,
            None,
        )
    }

    fn change_favorite(
        &self,
        client: &Client,
        zone: &str,
        operation: FavoriteOperation,
        id: Option<&str>,
        expected: &str,
        title: Option<&str>,
    ) -> Result<ManagedFavoriteDto, Failure> {
        let tool = match operation {
            FavoriteOperation::Save => "save_favorite",
            FavoriteOperation::Rename => "rename_favorite",
            FavoriteOperation::Remove => "remove_favorite",
        };
        let intent = format!(
            "{tool}: {} {} {:?}",
            zone.trim(),
            id.unwrap_or("current"),
            title.unwrap_or_default()
        );
        let _changing = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result: Result<ManagedFavoriteDto, Failure> = (|| {
            let (households, group) = self.queue_households(client, zone.trim())?;
            let player = control::locate(&households, &group.coordinator)?;
            self.queue_group_unchanged(player.ip, &group)?;
            let change = match operation {
                FavoriteOperation::Save => favorites::save_current(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    expected,
                    title.expect("save title"),
                )?,
                FavoriteOperation::Rename => favorites::rename(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    id.expect("rename identity"),
                    expected,
                    title.expect("rename title"),
                )?,
                FavoriteOperation::Remove => favorites::remove(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    id.expect("remove identity"),
                    expected,
                )?,
            };
            let action = match (operation, change.changed) {
                (FavoriteOperation::Save, _) => "saved",
                (FavoriteOperation::Rename, false) => "already named",
                (FavoriteOperation::Rename, true) => "renamed",
                (FavoriteOperation::Remove, _) => "removed",
            };
            Ok(ManagedFavoriteDto {
                done: format!(
                    "{action} Sonos favorite {:?} ({}) in {}'s household",
                    change.favorite.title, change.favorite.id, player.room_name
                ),
                zone: player.room_name.clone(),
                favorite: FavoriteDto::from(&change.favorite),
                operation,
                changed: change.changed,
                logged: false,
            })
        })();
        match result {
            Ok(mut managed) => {
                managed.logged =
                    self.record(client, intent, "allow".into(), managed.done.clone(), None);
                Ok(managed)
            }
            Err(mut failure) => {
                if failure.code == ErrorCode::OutcomeUncertain {
                    failure = failure.with_hint("Read the original household's Sonos favorites before another catalog edit; the operation was not automatically retried.");
                }
                self.notice(&failure);
                self.record(
                    client,
                    intent,
                    "allow".into(),
                    format!("failed: {}", failure.detail),
                    None,
                );
                Err(failure)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_requests_require_an_exact_identity_revision_and_bounded_title() {
        for id in [
            "1", "Dinner", "FV:2/", "FV:2/1/2", "SQ:1", "FV:2/-1", "FV:2/١",
        ] {
            assert!(native_id(id).is_err(), "{id}");
        }
        native_id("FV:2/12").unwrap();
        for expected in ["", "  ", "a\nb"] {
            assert!(revision(expected).is_err());
        }
        assert!(revision(&"a".repeat(4097)).is_err());
        for title in [String::new(), "a\nb".into(), "é".repeat(512)] {
            assert!(
                SaveFavoriteRequest {
                    zone: "Den".into(),
                    expected_revision: "r".into(),
                    title
                }
                .validate()
                .is_err()
            );
        }
        for value in [
            json!({"zone":"Den","title":"Song"}),
            json!({"zone":"Den","title":"Song","expected_revision":"r","uri":"http://injected"}),
            json!({"zone":"Den","title":"Song","expected_revision":"r","metadata":"injected"}),
        ] {
            assert!(serde_json::from_value::<SaveFavoriteRequest>(value).is_err());
        }
        assert!(
            serde_json::from_value::<RemoveFavoriteRequest>(
                json!({"zone":"Den","favorite":"FV:2/1","expected_revision":"r","force":true})
            )
            .is_err()
        );
    }

    #[test]
    fn reads_preserve_qualified_identity_and_encode_query_delimiters() {
        assert_eq!(
            current_path("Den & Study@S2").unwrap(),
            "/favorites/current?zone=Den%20%26%20Study%40S2"
        );
        assert_eq!(
            InspectFavoriteRequest {
                zone: "Den@S1".into(),
                favorite: "FV:2/7".into()
            }
            .path()
            .unwrap(),
            "/favorites/inspect?zone=Den%40S1&favorite=FV%3A2%2F7"
        );
    }
}
