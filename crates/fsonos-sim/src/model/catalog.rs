//! The native saved-playlist catalog mutates independently of playback.

use super::{Args, Fault, INVALID_ARGS, Out, State, TRANSITION_NOT_AVAILABLE};

impl State {
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
