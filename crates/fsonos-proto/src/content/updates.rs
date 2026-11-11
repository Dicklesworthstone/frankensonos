//! ContentDirectory event counters are invalidation evidence, not queue data.

use crate::ProtoError;
use crate::gena::Notify;
use std::collections::BTreeMap;

/// Bound one event and the controller's retained container counters.
pub const MAX_UPDATE_CONTAINERS: usize = 128;
const MAX_UPDATE_BYTES: usize = 32 * 1024;
const MAX_TOKEN_BYTES: usize = 256;

/// A validated ContentDirectory notification. Sonos's favorite and saved-queue
/// counters are opaque strings; only SystemUpdateID and container revisions
/// have the UPnP unsigned-integer contract.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContentUpdate {
    pub system_update_id: Option<u32>,
    pub containers: BTreeMap<String, u32>,
    pub favorites_update_id: Option<String>,
    pub saved_queues_update_id: Option<String>,
    /// An empty ContainerUpdateIDs property is valid initial evidence.
    pub has_container_updates: bool,
}

impl ContentUpdate {
    #[must_use]
    pub fn has_counters(&self) -> bool {
        self.system_update_id.is_some()
            || self.has_container_updates
            || self.favorites_update_id.is_some()
            || self.saved_queues_update_id.is_some()
    }
}

fn malformed() -> ProtoError {
    ProtoError::Malformed("invalid or oversized ContentDirectory update counters".into())
}

fn token(value: &str) -> Result<&str, ProtoError> {
    let value = value.trim();
    if value.len() > MAX_TOKEN_BYTES || value.chars().any(char::is_control) {
        return Err(malformed());
    }
    Ok(value)
}

fn counter(value: &str) -> Result<u32, ProtoError> {
    let value = token(value)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }
    value.parse().map_err(|_| malformed())
}

/// Parse the evented SystemUpdateID, ContainerUpdateIDs, FavoritesUpdateID and
/// SavedQueuesUpdateID properties. Unrelated service properties are ignored.
/// Duplicate properties/containers, incomplete pairs and invalid counters do
/// not become an empty observation; the caller can recover the subscription.
pub fn parse_update(notify: &Notify) -> Result<ContentUpdate, ProtoError> {
    let mut update = ContentUpdate::default();
    let mut bytes = 0_usize;
    for (name, value) in &notify.properties {
        if !matches!(
            name.as_str(),
            "SystemUpdateID" | "ContainerUpdateIDs" | "FavoritesUpdateID" | "SavedQueuesUpdateID"
        ) {
            continue;
        }
        bytes = bytes.saturating_add(value.len());
        if bytes > MAX_UPDATE_BYTES {
            return Err(malformed());
        }
        match name.as_str() {
            "SystemUpdateID" => {
                if update.system_update_id.replace(counter(value)?).is_some() {
                    return Err(malformed());
                }
            }
            "FavoritesUpdateID" | "SavedQueuesUpdateID" => {
                let slot = if name == "FavoritesUpdateID" {
                    &mut update.favorites_update_id
                } else {
                    &mut update.saved_queues_update_id
                };
                if slot.replace(token(value)?.to_owned()).is_some() {
                    return Err(malformed());
                }
            }
            _ => {
                if update.has_container_updates {
                    return Err(malformed());
                }
                update.has_container_updates = true;
                if value.trim().is_empty() {
                    continue;
                }
                let mut fields = value.split(',');
                while let Some(container) = fields.next() {
                    let container = token(container)?;
                    let revision = counter(fields.next().ok_or_else(malformed)?)?;
                    if container.is_empty()
                        || update.containers.len() == MAX_UPDATE_CONTAINERS
                        || update
                            .containers
                            .insert(container.to_owned(), revision)
                            .is_some()
                    {
                        return Err(malformed());
                    }
                }
            }
        }
    }
    Ok(update)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(properties: &[(&str, &str)]) -> Notify {
        Notify {
            sid: "uuid:content-test".into(),
            seq: 0,
            path: "/ContentDirectory".into(),
            properties: properties
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        }
    }

    #[test]
    fn reads_bounded_pairs_and_retains_opaque_sonos_counters() {
        let update = parse_update(&event(&[
            ("SystemUpdateID", "4294967295"),
            ("ContainerUpdateIDs", "Q:0,0,FV:2,12,SQ:7,81"),
            ("FavoritesUpdateID", "favorite-revision"),
            ("SavedQueuesUpdateID", "saved-revision"),
            ("ShareIndexInProgress", "1"),
        ]))
        .unwrap();
        assert_eq!(update.system_update_id, Some(u32::MAX));
        assert_eq!(update.containers["Q:0"], 0);
        assert_eq!(update.containers["FV:2"], 12);
        assert_eq!(update.containers["SQ:7"], 81);
        assert_eq!(
            update.favorites_update_id.as_deref(),
            Some("favorite-revision")
        );
        assert_eq!(
            update.saved_queues_update_id.as_deref(),
            Some("saved-revision")
        );
        assert!(update.has_counters());
    }

    #[test]
    fn empty_lists_are_evidence_but_unrelated_properties_are_not() {
        assert!(
            parse_update(&event(&[("ContainerUpdateIDs", "")]))
                .unwrap()
                .has_counters()
        );
        assert!(
            !parse_update(&event(&[("ShareIndexInProgress", "0")]))
                .unwrap()
                .has_counters()
        );
    }

    #[test]
    fn malformed_or_ambiguous_evidence_is_not_silently_truncated() {
        for value in [
            "Q:0",
            "Q:0,1,",
            "Q:0,no",
            "Q:0,-1",
            "Q:0,4294967296",
            ",1",
            "Q:0,1,Q:0,2",
        ] {
            assert!(
                parse_update(&event(&[("ContainerUpdateIDs", value)])).is_err(),
                "{value}"
            );
        }
        for name in [
            "SystemUpdateID",
            "ContainerUpdateIDs",
            "FavoritesUpdateID",
            "SavedQueuesUpdateID",
        ] {
            let value = if name == "ContainerUpdateIDs" {
                "Q:0,1"
            } else {
                "1"
            };
            assert!(parse_update(&event(&[(name, value), (name, value)])).is_err());
        }
        let many = (0..=MAX_UPDATE_CONTAINERS)
            .map(|n| format!("SQ:{n},1"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_update(&event(&[("ContainerUpdateIDs", &many)])).is_err());
        assert!(
            parse_update(&event(&[(
                "FavoritesUpdateID",
                &"a".repeat(MAX_TOKEN_BYTES + 1)
            )]))
            .is_err()
        );
        assert!(parse_update(&event(&[("FavoritesUpdateID", "a\nb")])).is_err());
        assert!(
            parse_update(&event(&[(
                "ContainerUpdateIDs",
                &"a".repeat(MAX_UPDATE_BYTES + 1)
            )]))
            .is_err()
        );
    }
}
