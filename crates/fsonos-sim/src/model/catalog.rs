//! The native saved-playlist catalog mutates independently of playback.

use super::{Args, Fault, Favorite, INVALID_ARGS, Out, State, TRANSITION_NOT_AVAILABLE};
use fsonos_proto::didl::{DidlKind, parse_didl};

impl State {
    pub(super) fn create_favorite(&mut self, player: usize, args: &Args<'_>) -> Result<Out, Fault> {
        self.require_coordinator(player)?;
        if args.get("ContainerID")? != "FV:2" {
            return Err(INVALID_ARGS);
        }
        let objects = parse_didl(args.get("Elements")?).map_err(|_| INVALID_ARGS)?;
        let [object] = objects.as_slice() else {
            return Err(INVALID_ARGS);
        };
        if object.kind != DidlKind::Item
            || !object.id.is_empty()
            || object.parent_id != "FV:2"
            || object.class != "object.itemobject.item.sonos-favorite"
            || object.favorite_type.as_deref() != Some("instantPlay")
            || object.title.trim().is_empty()
            || object.title.len() > 1023
            || object.title.chars().any(char::is_control)
        {
            return Err(INVALID_ARGS);
        }
        let resource = object.res.as_ref().ok_or(INVALID_ARGS)?;
        let metadata = object.res_md.as_deref().ok_or(INVALID_ARGS)?;
        if parse_didl(metadata).map_err(|_| INVALID_ARGS)?.len() != 1 {
            return Err(INVALID_ARGS);
        }
        let house = &mut self.households[self.players[player].household];
        if house
            .favorites
            .iter()
            .any(|favorite| favorite.uri == resource.uri)
        {
            return Err(Fault(803));
        }
        let id = house
            .favorites
            .iter()
            .map(|favorite| favorite.id)
            .max()
            .map_or(Some(0), |id| id.checked_add(1))
            .ok_or(INVALID_ARGS)?;
        let favorite = Favorite {
            id,
            title: object.title.clone(),
            uri: resource.uri.clone(),
            protocol_info: resource.protocol_info.clone().unwrap_or_default(),
            metadata: metadata.into(),
            kind: "instantPlay",
            description: object.favorite_description.clone().unwrap_or_default(),
            art_uri: object.album_art_uri.clone(),
        };
        let result = crate::docs::didl(&crate::docs::favorite_items(
            std::slice::from_ref(&favorite),
            house.favorites.len(),
        ));
        house.favorites.push(favorite);
        house.favorites_update_id = house.favorites_update_id.wrapping_add(1);
        Ok(vec![("ObjectID", format!("FV:2/{id}")), ("Result", result)])
    }

    pub(super) fn manage_favorite(
        &mut self,
        player: usize,
        action: &str,
        args: &Args<'_>,
    ) -> Result<Out, Fault> {
        self.require_coordinator(player)?;
        let id = args
            .get("ObjectID")?
            .strip_prefix("FV:2/")
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|id| id.parse::<usize>().ok())
            .ok_or(INVALID_ARGS)?;
        let house = &mut self.households[self.players[player].household];
        let index = house
            .favorites
            .iter()
            .position(|favorite| favorite.id == id)
            .ok_or(TRANSITION_NOT_AVAILABLE)?;
        if action == "UpdateObject" {
            let current = title_tag(args.get("CurrentTagValue")?).map_err(|()| Fault(702))?;
            let next = title_tag(args.get("NewTagValue")?).map_err(|()| Fault(703))?;
            if current != house.favorites[index].title {
                return Err(Fault(702));
            }
            if next.trim().is_empty() || next.len() > 1023 || next.chars().any(char::is_control) {
                return Err(Fault(703));
            }
            if current == next {
                return Ok(Vec::new());
            }
            house.favorites[index].title = next;
        } else {
            house.favorites.remove(index);
        }
        house.favorites_update_id = house.favorites_update_id.wrapping_add(1);
        Ok(Vec::new())
    }

    pub(super) fn manage_playlist(
        &mut self,
        player: usize,
        action: &str,
        args: &Args<'_>,
    ) -> Result<Out, Fault> {
        self.require_coordinator(player)?;
        let id = args
            .get("ObjectID")?
            .strip_prefix("SQ:")
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|id| id.parse::<u32>().ok())
            .ok_or(INVALID_ARGS)?;
        let house = &mut self.households[self.players[player].household];
        let index = house
            .saved_playlists
            .iter()
            .position(|saved| saved.id == id)
            .ok_or(TRANSITION_NOT_AVAILABLE)?;
        if action == "UpdateObject" {
            let current = title_tag(args.get("CurrentTagValue")?).map_err(|()| Fault(702))?;
            let next = title_tag(args.get("NewTagValue")?).map_err(|()| Fault(703))?;
            if current != house.saved_playlists[index].title {
                return Err(Fault(702));
            }
            if next.trim().is_empty() || next.len() > 1023 || next.chars().any(char::is_control) {
                return Err(Fault(703));
            }
            if current == next {
                return Ok(Vec::new());
            }
            house.saved_playlists[index].title = next;
        } else {
            house.saved_playlists.remove(index);
        }
        house.saved_playlists_update_id = house.saved_playlists_update_id.wrapping_add(1);
        Ok(Vec::new())
    }
}

fn title_tag(fragment: &str) -> Result<String, ()> {
    let xml = format!("<root xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{fragment}</root>");
    let document = roxmltree::Document::parse(&xml).map_err(|_| ())?;
    let mut elements = document
        .root_element()
        .children()
        .filter(|node| !node.is_text() || node.text().is_some_and(|text| !text.trim().is_empty()));
    let title = elements.next().ok_or(())?;
    if elements.next().is_some()
        || !title.has_tag_name(("http://purl.org/dc/elements/1.1/", "title"))
        || title.attributes().len() != 0
        || title.children().any(|node| !node.is_text())
    {
        return Err(());
    }
    Ok(title.children().filter_map(|node| node.text()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_title_fragments_preserve_punctuation_and_reject_other_properties() {
        assert_eq!(
            title_tag(r"<dc:title>Dvořák, Bach &amp; &lt;friends&gt; \ Live</dc:title>"),
            Ok(r"Dvořák, Bach & <friends> \ Live".into())
        );
        for invalid in [
            "",
            "<title>Wrong namespace</title>",
            "<dc:title><nested/></dc:title>",
            "<dc:title>A</dc:title>,<dc:title>B</dc:title>",
            "<dc:title>A</dc:title>trailing",
            "<!DOCTYPE root><dc:title>A</dc:title>",
        ] {
            assert!(title_tag(invalid).is_err(), "{invalid}");
        }
    }
}
