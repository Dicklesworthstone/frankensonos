//! Native My Sonos catalog writes, independent of the playing queue. A caller
//! reviews a source or exact favorite before one mutation; uncertain replies
//! never authorize replay.

use super::{Favorite, FavoriteKind, classify};
use crate::playlists::GroupProof;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, FAVORITES, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    DidlKind, DidlObject, favorite_metadata, item_metadata, parse_didl, xml_escape,
};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::net::IpAddr;

/// An exact catalog entry, or the currently selected track/station. A current
/// source has an empty `favorite.id` until Sonos creates its native identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedFavorite {
    pub favorite: Favorite,
    pub revision: String,
}

/// A confirmed catalog mutation. Removal returns the original entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FavoriteChange {
    pub favorite: Favorite,
    pub changed: bool,
}

/// Favorite titles share the native catalog's bounded, nonempty UTF-8 form.
pub fn validate_title(title: &str) -> Result<&str, &'static str> {
    crate::playlists::validate_title(title).map_err(
        |_| "a Sonos favorite title must contain 1–1023 UTF-8 bytes and no control characters",
    )
}

#[must_use]
pub fn native_id(id: &str) -> bool {
    id.strip_prefix("FV:2/")
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Observe one exact household-local favorite. Duplicate titles remain
/// distinct identities; numbered positions and fuzzy names are not accepted.
pub fn inspect<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    id: &str,
) -> Result<ObservedFavorite, CoreError> {
    validate_id(id)?;
    let owner = Owner::read(transport, households, coordinator)?;
    let catalog = Catalog::read(transport, owner.host)?;
    let favorite = catalog.selected(id)?.clone();
    catalog.confirm(transport, owner.host)?;
    owner.confirm(transport)?;
    Ok(ObservedFavorite {
        revision: owner.revision(
            "sonos-favorite-v1",
            &serde_json::json!([catalog.update_id, favorite_fields(&favorite)]),
        ),
        favorite,
    })
}

/// Capture the actual current queue occurrence or the direct track/station.
/// Elapsed seconds and transient radio-song metadata do not change the token.
/// Queue, group, physical-input and TV routing URIs cannot become favorites.
pub fn current<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<ObservedFavorite, CoreError> {
    let owner = Owner::read(transport, households, coordinator)?;
    let selection = Current::read(transport, owner.host, coordinator)?;
    selection.confirm(transport, owner.host, coordinator)?;
    owner.confirm(transport)?;
    Ok(ObservedFavorite {
        revision: selection.revision(&owner),
        favorite: selection.favorite,
    })
}

/// Save exactly the reviewed selected track/station, without changing playback,
/// queue contents, transport intent or levels. Native duplicate/full refusals
/// are returned as-is. A lost acknowledgement is uncertain, never a retry.
pub fn save_current<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    expected_revision: &str,
    title: &str,
) -> Result<FavoriteChange, CoreError> {
    validate_revision(expected_revision)?;
    let title = validate_title(title).map_err(invalid)?;
    let owner = Owner::read(transport, households, coordinator)?;
    let selection = Current::read(transport, owner.host, coordinator)?;
    if selection.revision(&owner) != expected_revision {
        return Err(changed("the reviewed current track or station changed"));
    }
    let before = Catalog::read(transport, owner.host)?;
    selection.confirm(transport, owner.host, coordinator)?;
    owner.confirm(transport)?;
    let mut expected = selection.favorite.clone();
    expected.title = title.into();
    expected.metadata = favorite_metadata(&expected.metadata)?;
    expected.description = expected
        .description
        .filter(|value| !value.trim().is_empty());
    expected.art_uri = expected.art_uri.filter(|value| !value.trim().is_empty());
    let elements = favorite_elements(&expected, selection.protocol_info.as_deref());
    let id = acknowledged(
        content::create_object(transport, owner.host, FAVORITES, &elements),
        "creation",
    )?;
    if !native_id(&id) || before.favorites.iter().any(|favorite| favorite.id == id) {
        return Err(uncertain(
            "creation",
            "Sonos did not return a new favorite identity",
        ));
    }
    expected.id = id.clone();
    let after = owner.catalog_after(transport, "creation")?;
    let saved = after
        .selected(&id)
        .map_err(|error| uncertain("creation", error))?;
    if !same_favorite(saved, &expected) {
        return Err(uncertain(
            "creation",
            "the saved catalog did not retain the reviewed source and metadata",
        ));
    }
    Ok(FavoriteChange {
        favorite: saved.clone(),
        changed: true,
    })
}

/// Rename one observed `FV:2/<id>`. Only its title changes. A matching title is
/// a confirmed no-op after the same revision and household checks.
pub fn rename<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    id: &str,
    expected_revision: &str,
    title: &str,
) -> Result<FavoriteChange, CoreError> {
    let title = validate_title(title).map_err(invalid)?;
    let selected = Selected::read(transport, households, coordinator, id, expected_revision)?;
    if selected.favorite.title == title {
        return Ok(FavoriteChange {
            favorite: selected.favorite,
            changed: false,
        });
    }
    acknowledged(
        content::rename_object(
            transport,
            selected.owner.host,
            id,
            &selected.favorite.title,
            title,
        ),
        "rename",
    )?;
    let after = selected.owner.catalog_after(transport, "rename")?;
    let actual = after
        .selected(id)
        .map_err(|error| uncertain("rename", error))?;
    let mut expected = selected.favorite;
    expected.title = title.into();
    if !same_favorite(actual, &expected) {
        return Err(uncertain(
            "rename",
            "the catalog did not confirm the same source with its new title",
        ));
    }
    Ok(FavoriteChange {
        favorite: actual.clone(),
        changed: true,
    })
}

/// Remove one exact reviewed favorite, leaving existing queued or playing
/// copies intact. Sonos has no revision argument for `DestroyObject`; the
/// read-before-write checks cannot lock out another independent controller.
pub fn remove<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    id: &str,
    expected_revision: &str,
) -> Result<FavoriteChange, CoreError> {
    let selected = Selected::read(transport, households, coordinator, id, expected_revision)?;
    acknowledged(
        content::destroy_object(transport, selected.owner.host, id),
        "removal",
    )?;
    let after = selected.owner.catalog_after(transport, "removal")?;
    if after.favorites.iter().any(|favorite| favorite.id == id) {
        return Err(uncertain(
            "removal",
            "the selected favorite remains in the catalog",
        ));
    }
    Ok(FavoriteChange {
        favorite: selected.favorite,
        changed: true,
    })
}

struct Selected<'a> {
    owner: Owner<'a>,
    favorite: Favorite,
}

impl<'a> Selected<'a> {
    fn read<T: Transport + ?Sized>(
        transport: &T,
        households: &'a [HouseholdState],
        coordinator: &'a PlayerId,
        id: &str,
        expected_revision: &str,
    ) -> Result<Self, CoreError> {
        validate_id(id)?;
        validate_revision(expected_revision)?;
        let owner = Owner::read(transport, households, coordinator)?;
        let catalog = Catalog::read(transport, owner.host)?;
        let favorite = catalog.selected(id)?.clone();
        let revision = owner.revision(
            "sonos-favorite-v1",
            &serde_json::json!([catalog.update_id, favorite_fields(&favorite)]),
        );
        if revision != expected_revision {
            return Err(changed("the reviewed favorite is no longer current"));
        }
        catalog.confirm(transport, owner.host)?;
        owner.confirm(transport)?;
        Ok(Self { owner, favorite })
    }
}

struct Owner<'a> {
    house: &'a HouseholdState,
    coordinator: &'a PlayerId,
    host: IpAddr,
    group: GroupProof,
}

impl<'a> Owner<'a> {
    fn read<T: Transport + ?Sized>(
        transport: &T,
        households: &'a [HouseholdState],
        coordinator: &'a PlayerId,
    ) -> Result<Self, CoreError> {
        let mut houses = households
            .iter()
            .filter(|house| house.player(coordinator).is_some());
        let house = houses
            .next()
            .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
        if houses.next().is_some() {
            return Err(changed(
                "the favorite player belongs to more than one household",
            ));
        }
        let mut groups = house
            .groups
            .iter()
            .filter(|group| group.coordinator == *coordinator);
        let expected = groups
            .next()
            .ok_or_else(|| changed("the favorite player no longer leads its group"))?;
        if groups.next().is_some() {
            return Err(changed("the favorite player's group is ambiguous"));
        }
        let host = control::locate(households, coordinator)?.ip;
        let group = GroupProof::read(transport, host, coordinator)?;
        let mut members = expected.members.clone();
        members.sort_by(|left, right| left.0.cmp(&right.0));
        members.dedup();
        if group.members != members {
            return Err(changed("the favorite player's group membership changed"));
        }
        Ok(Self {
            house,
            coordinator,
            host,
            group,
        })
    }

    fn confirm<T: Transport + ?Sized>(&self, transport: &T) -> Result<(), CoreError> {
        if self.group != GroupProof::read(transport, self.host, self.coordinator)? {
            return Err(changed(
                "the favorite player's group or boot identity changed",
            ));
        }
        Ok(())
    }

    fn revision(&self, prefix: &str, selection: &serde_json::Value) -> String {
        let bytes = serde_json::to_vec(&serde_json::json!([
            self.house.id,
            self.house.generation(),
            self.coordinator,
            self.host,
            self.group.members,
            self.group.boot_seq,
            selection
        ]))
        .expect("favorite observation fields serialize");
        let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
        });
        format!("{prefix}-{hash:016x}")
    }

    fn catalog_after<T: Transport + ?Sized>(
        &self,
        transport: &T,
        operation: &str,
    ) -> Result<Catalog, CoreError> {
        self.confirm(transport)
            .map_err(|error| uncertain(operation, error))?;
        let catalog =
            Catalog::read(transport, self.host).map_err(|error| uncertain(operation, error))?;
        catalog
            .confirm(transport, self.host)
            .map_err(|error| uncertain(operation, error))?;
        self.confirm(transport)
            .map_err(|error| uncertain(operation, error))?;
        Ok(catalog)
    }
}

#[derive(PartialEq, Eq)]
struct Catalog {
    favorites: Vec<Favorite>,
    update_id: u32,
}

impl Catalog {
    fn read<T: Transport + ?Sized>(transport: &T, host: IpAddr) -> Result<Self, CoreError> {
        let snapshot = content::browse_snapshot(transport, host, FAVORITES)?;
        let mut ids = HashSet::new();
        let mut favorites = Vec::with_capacity(snapshot.objects.len());
        for object in &snapshot.objects {
            if object.parent_id != FAVORITES || !native_id(&object.id) || !ids.insert(&object.id) {
                return Err(invalid(
                    "the favorite catalog contains invalid or duplicate native identities",
                ));
            }
            favorites.push(classify(object));
        }
        Ok(Self {
            favorites,
            update_id: snapshot.update_id,
        })
    }

    fn selected(&self, id: &str) -> Result<&Favorite, CoreError> {
        self.favorites
            .iter()
            .find(|favorite| favorite.id == id)
            .ok_or_else(|| changed("the reviewed favorite is no longer in this household"))
    }

    fn confirm<T: Transport + ?Sized>(&self, transport: &T, host: IpAddr) -> Result<(), CoreError> {
        if *self != Self::read(transport, host)? {
            return Err(changed("the favorite catalog changed while it was read"));
        }
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
struct Current {
    favorite: Favorite,
    protocol_info: Option<String>,
    media_uri: String,
    media_metadata: String,
    occurrence: Option<(u32, String, u32)>,
}

impl Current {
    fn read<T: Transport + ?Sized>(
        transport: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, CoreError> {
        let media = soap::get_media_info(transport, host)?;
        let (object, uri, occurrence, metadata, protocol_info) = if media.uri
            == format!("x-rincon-queue:{}#0", coordinator.0)
        {
            let (position, raw_metadata) = soap::get_position_info_with_metadata(transport, host)?;
            let offset = position
                .track
                .checked_sub(1)
                .ok_or_else(|| unavailable("the queue has no selected track to save"))?;
            let queue = content::browse_range_snapshot(transport, host, QUEUE, offset, 1)?;
            let [object] = queue.objects.as_slice() else {
                return Err(unavailable("the selected queue occurrence is unavailable"));
            };
            let uri = object
                .res
                .as_ref()
                .map(|resource| resource.uri.clone())
                .filter(|uri| eligible_uri(uri))
                .ok_or_else(|| {
                    unavailable("the selected queue occurrence has no reusable audio resource")
                })?;
            if object.kind != DidlKind::Item || !audio_class(&object.class) {
                return Err(unavailable(
                    "the selected queue occurrence is not an expanded audio item",
                ));
            }
            if position.uri != uri {
                return Err(changed(
                    "the queue's selected occurrence changed during observation",
                ));
            }
            // Browse identifies a queue occurrence (Q:0/n), and may omit the
            // original service descriptor. Position metadata retains the
            // actual track identity that Sonos can replay as a favorite.
            let native = match position.metadata.as_ref() {
                Some(native) if native.kind == DidlKind::Item && audio_class(&native.class) => {
                    native
                }
                None if !uri.starts_with("x-sonos-spotify:") => object,
                _ => {
                    return Err(unavailable(
                        "the selected track has no reusable native metadata",
                    ));
                }
            };
            let metadata = if position.metadata.is_some() {
                raw_metadata
            } else {
                item_metadata(native)
            };
            let native = native.clone();
            (
                native,
                uri,
                Some((position.track, position.uri, queue.update_id)),
                metadata,
                object
                    .res
                    .as_ref()
                    .and_then(|resource| resource.protocol_info.clone()),
            )
        } else {
            if !eligible_uri(&media.uri) {
                return Err(unavailable(
                    "the current source is empty, a group, an input, or a queue rather than a reusable track or station",
                ));
            }
            let objects = if media.uri_metadata.trim().is_empty()
                || media.uri_metadata.trim() == "NOT_IMPLEMENTED"
            {
                Vec::new()
            } else {
                parse_didl(&media.uri_metadata)?
            };
            let object = match objects.as_slice() {
                [object] if object.kind == DidlKind::Item && audio_class(&object.class) => {
                    object.clone()
                }
                [] if raw_stream(&media.uri) => bare_stream(&media.uri),
                _ => {
                    return Err(unavailable(
                        "the current source has no complete reusable audio metadata",
                    ));
                }
            };
            let metadata = if objects.is_empty() {
                item_metadata(&object)
            } else {
                media.uri_metadata.clone()
            };
            let protocol_info = object
                .res
                .as_ref()
                .and_then(|resource| resource.protocol_info.clone());
            (object, media.uri.clone(), None, metadata, protocol_info)
        };
        let kind = if object
            .class
            .starts_with("object.item.audioItem.audioBroadcast")
            || radio_uri(&uri)
        {
            FavoriteKind::Stream
        } else {
            FavoriteKind::Track
        };
        let favorite = Favorite {
            id: String::new(),
            title: object.title.clone(),
            kind,
            uri: Some(uri),
            metadata,
            description: object.creator.clone(),
            art_uri: object.album_art_uri.clone(),
        };
        Ok(Self {
            favorite,
            protocol_info,
            media_uri: media.uri,
            media_metadata: media.uri_metadata,
            occurrence,
        })
    }

    fn confirm<T: Transport + ?Sized>(
        &self,
        transport: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), CoreError> {
        if *self != Self::read(transport, host, coordinator)? {
            return Err(changed(
                "the selected current track or station changed while it was read",
            ));
        }
        Ok(())
    }

    fn revision(&self, owner: &Owner<'_>) -> String {
        owner.revision(
            "sonos-favorite-current-v1",
            &serde_json::json!([
                self.media_uri,
                self.media_metadata,
                self.occurrence,
                self.protocol_info,
                favorite_fields(&self.favorite)
            ]),
        )
    }
}

fn favorite_fields(favorite: &Favorite) -> serde_json::Value {
    serde_json::json!([
        favorite.id,
        favorite.title,
        favorite.uri,
        favorite.metadata,
        favorite.description,
        favorite.art_uri
    ])
}

fn same_favorite(actual: &Favorite, expected: &Favorite) -> bool {
    actual.id == expected.id
        && actual.title == expected.title
        && actual.kind == expected.kind
        && actual.uri == expected.uri
        && actual.description == expected.description
        && actual.art_uri == expected.art_uri
        // The parsed model does not contain every native service extension.
        // Only the complete retained resMD proves that those bytes survived.
        && actual.metadata == expected.metadata
}

fn favorite_elements(favorite: &Favorite, protocol_info: Option<&str>) -> String {
    // The class spelling and the nested resMD follow Sonos's own FV:2 format,
    // documented by SoCo's favorites.py. Each XML layer is escaped separately.
    let attribute = protocol_info
        .map(|value| format!(" protocolInfo=\"{}\"", xml_escape(value)))
        .unwrap_or_default();
    let mut elements = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
         xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">\
         <item id=\"\" parentID=\"FV:2\" restricted=\"false\">\
         <dc:title>{}</dc:title><upnp:class>object.itemobject.item.sonos-favorite</upnp:class>\
         <r:ordinal>0</r:ordinal><res{attribute}>{}</res><r:type>instantPlay</r:type>",
        xml_escape(&favorite.title),
        xml_escape(favorite.uri.as_deref().unwrap_or_default())
    );
    for (tag, value) in [
        ("upnp:albumArtURI", favorite.art_uri.as_deref()),
        ("r:description", favorite.description.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            elements.push_str(&format!("<{tag}>{}</{tag}>", xml_escape(value)));
        }
    }
    elements.push_str(&format!(
        "<r:resMD>{}</r:resMD></item></DIDL-Lite>",
        xml_escape(&favorite.metadata)
    ));
    elements
}

fn audio_class(class: &str) -> bool {
    class == "object.item.audioItem" || class.starts_with("object.item.audioItem.")
}

fn eligible_uri(uri: &str) -> bool {
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && !uri.chars().any(|c| c.is_control() || c.is_whitespace())
        && scheme
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
        && !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "x-rincon"
                | "x-rincon-queue"
                | "x-rincon-stream"
                | "x-sonos-htastream"
                | "x-sonos-vli"
                | "x-rincon-cpcontainer"
                | "spotify"
        )
        && !(scheme.eq_ignore_ascii_case("file")
            && rest
                .split(['?', '#'])
                .next()
                .is_some_and(|path| path.to_ascii_lowercase().ends_with(".rsq")))
}

fn radio_uri(uri: &str) -> bool {
    [
        "x-rincon-mp3radio:",
        "x-sonosapi-stream:",
        "x-sonosapi-radio:",
        "x-sonosapi-hls:",
        "aac:",
    ]
    .iter()
    .any(|prefix| uri.starts_with(prefix))
}

fn raw_stream(uri: &str) -> bool {
    ["http:", "https:", "x-rincon-mp3radio:", "aac:"]
        .iter()
        .any(|prefix| uri.starts_with(prefix))
}

fn bare_stream(uri: &str) -> DidlObject {
    DidlObject {
        kind: DidlKind::Item,
        id: "-1".into(),
        parent_id: "-1".into(),
        restricted: true,
        title: uri.into(),
        class: "object.item.audioItem.audioBroadcast".into(),
        creator: None,
        album: None,
        album_art_uri: None,
        res: None,
        desc: None,
        favorite_type: None,
        favorite_description: None,
        ordinal: None,
        res_md: None,
    }
}

fn validate_id(id: &str) -> Result<(), CoreError> {
    if !native_id(id) || id.len() > 1024 {
        return Err(invalid("an exact native FV:2/<id> favorite is required"));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<(), CoreError> {
    if revision.trim().is_empty() || revision.len() > 4096 || revision.chars().any(char::is_control)
    {
        return Err(invalid(
            "an observed favorite or current-source revision is required",
        ));
    }
    Ok(())
}

fn acknowledged<T>(result: Result<T, ProtoError>, operation: &str) -> Result<T, CoreError> {
    match result {
        Ok(value) => Ok(value),
        Err(error @ ProtoError::SoapFault { .. }) => Err(error.into()),
        Err(error) => Err(uncertain(operation, error)),
    }
}

fn invalid(detail: &str) -> CoreError {
    ProtoError::Malformed(detail.into()).into()
}
fn changed(detail: &str) -> CoreError {
    CoreError::FavoriteChanged(detail.into())
}
fn unavailable(detail: &str) -> CoreError {
    CoreError::FavoriteUnavailable(detail.into())
}
fn uncertain(operation: &str, detail: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "Sonos favorite {operation} could not be confirmed: {detail}; refresh favorites before requesting another change"
    ))
}
