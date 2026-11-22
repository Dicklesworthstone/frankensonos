//! Isolate native objects without rebuilding service-owned playback metadata.

use super::{DidlDesc, DidlKind, DidlObject, parse_object, xml_escape};
use crate::{ProtoError, xml};

/// One parsed object beside its original, self-contained DIDL document.
/// The original root and object bytes retain inherited namespaces, attributes,
/// additional resources/descriptors and extensions outside the modeled fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDidlObject {
    pub object: DidlObject,
    pub metadata: String,
}

/// Parse at most `max_objects` native items/containers and retain their XML.
///
/// Each object is isolated under the original DIDL root. Other objects are
/// omitted; the selected object's bytes are not rewritten. Check the
/// aggregate size of all resulting documents before
/// allocating them: a large shared root must not multiply past `max_bytes`.
pub fn parse_didl_with_metadata(
    doc_text: &str,
    max_objects: usize,
    max_bytes: usize,
) -> Result<Vec<NativeDidlObject>, ProtoError> {
    if doc_text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let doc = xml::parse(doc_text)?;
    let root = doc.root_element();
    if root.tag_name().name() != "DIDL-Lite" {
        return Err(ProtoError::Malformed(format!(
            "expected DIDL-Lite, got <{}>",
            root.tag_name().name()
        )));
    }
    let Some(first) = root.children().next() else {
        return Ok(Vec::new());
    };
    let last = root
        .children()
        .next_back()
        .expect("the root has a first child");
    let prefix = &doc_text[root.range().start..first.range().start];
    let suffix = &doc_text[last.range().end..root.range().end];
    let objects = || {
        root.children().filter_map(|node| {
            if !node.is_element() {
                return None;
            }
            match node.tag_name().name() {
                "item" => Some((node, DidlKind::Item)),
                "container" => Some((node, DidlKind::Container)),
                _ => None,
            }
        })
    };
    let mut bytes = 0_usize;
    for (index, (node, _)) in objects().enumerate() {
        if index >= max_objects {
            return Err(ProtoError::Malformed(
                "native DIDL exceeds the requested object limit".into(),
            ));
        }
        bytes = bytes
            .checked_add(prefix.len())
            .and_then(|bytes| bytes.checked_add(node.range().len()))
            .and_then(|bytes| bytes.checked_add(suffix.len()))
            .filter(|bytes| *bytes <= max_bytes)
            .ok_or_else(|| {
                ProtoError::Malformed("native DIDL exceeds the retained metadata byte limit".into())
            })?;
    }
    objects()
        .map(|(node, kind)| {
            let object = parse_object(node, kind)?;
            let native = &doc_text[node.range()];
            let mut metadata = String::with_capacity(prefix.len() + native.len() + suffix.len());
            metadata.push_str(prefix);
            metadata.push_str(native);
            metadata.push_str(suffix);
            Ok(NativeDidlObject { object, metadata })
        })
        .collect()
}

fn native_item<'a, 'input: 'a>(
    document: &'a roxmltree::Document<'input>,
) -> Result<roxmltree::Node<'a, 'input>, ProtoError> {
    let root = document.root_element();
    if root.tag_name().name() != "DIDL-Lite" {
        return Err(ProtoError::Malformed(
            "native replay metadata must be DIDL-Lite".into(),
        ));
    }
    let mut objects = root
        .children()
        .filter(|node| node.is_element() && matches!(node.tag_name().name(), "item" | "container"));
    match (objects.next(), objects.next()) {
        (Some(item), None) if item.tag_name().name() == "item" => Ok(item),
        _ => Err(ProtoError::Malformed(
            "native replay metadata needs exactly one item".into(),
        )),
    }
}

fn attribute<'a, 'input: 'a>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Attribute<'a, 'input>> {
    node.attributes()
        .find(|attribute| attribute.namespace().is_none() && attribute.name() == name)
}

fn account_descriptor(item: roxmltree::Node<'_, '_>) -> Result<Option<DidlDesc>, ProtoError> {
    let mut descriptors = item.children().filter(|node| {
        node.is_element()
            && node.tag_name().name() == "desc"
            && node.tag_name().namespace() == item.tag_name().namespace()
            && attribute(*node, "id").is_some_and(|id| id.value() == "cdudn")
    });
    let Some(descriptor) = descriptors.next() else {
        return Ok(None);
    };
    if descriptors.next().is_some() || descriptor.children().any(|node| node.is_element()) {
        return Err(ProtoError::Malformed(
            "native playback account descriptor is ambiguous".into(),
        ));
    }
    let value = descriptor
        .children()
        .filter(roxmltree::Node::is_text)
        .filter_map(|node| node.text())
        .collect::<String>();
    Ok(Some(DidlDesc {
        id: "cdudn".into(),
        name_space: attribute(descriptor, "nameSpace")
            .map_or("", |attribute| attribute.value())
            .into(),
        value,
    }))
}

/// Read the single native playback-account descriptor without discarding
/// other service descriptors. Ambiguous account evidence is rejected.
pub fn native_account_descriptor(metadata: &str) -> Result<Option<DidlDesc>, ProtoError> {
    let document = xml::parse(metadata)?;
    account_descriptor(native_item(&document)?)
}

/// Replace queue-assigned item/parent IDs and supply a missing account
/// descriptor while retaining every other native XML byte. An existing
/// cdudn must already name the requested account; it is never overwritten.
/// Unknown attributes, resource alternatives, descriptors and namespaces
/// remain intact. The caller resolves the account from the same household
/// and bounds the complete retained result.
pub fn with_native_replay_identity(
    metadata: &str,
    id: &str,
    parent_id: &str,
    descriptor: &DidlDesc,
) -> Result<String, ProtoError> {
    if descriptor.id != "cdudn" || descriptor.value.trim().is_empty() {
        return Err(ProtoError::Malformed(
            "native replay needs a nonempty cdudn account descriptor".into(),
        ));
    }
    let document = xml::parse(metadata)?;
    let item = native_item(&document)?;
    let existing = account_descriptor(item)?;
    if existing
        .as_ref()
        .is_some_and(|existing| existing != descriptor)
    {
        return Err(ProtoError::Malformed(
            "native replay cannot replace a different playback account".into(),
        ));
    }
    let original_id = attribute(item, "id").ok_or_else(|| {
        ProtoError::Malformed("native replay item is missing its identity".into())
    })?;
    let mut edits = Vec::new();
    if original_id.value() != id {
        edits.push((original_id.range_value(), xml_escape(id)));
    }
    if let Some(parent) = attribute(item, "parentID") {
        if parent.value() != parent_id {
            edits.push((parent.range_value(), xml_escape(parent_id)));
        }
    } else {
        let at = original_id.range().end;
        edits.push((at..at, format!(" parentID=\"{}\"", xml_escape(parent_id))));
    }
    if existing.is_none() {
        let at = item
            .children()
            .next()
            .ok_or_else(|| {
                ProtoError::Malformed("native replay item has no playback metadata".into())
            })?
            .range()
            .start;
        let namespace = item
            .tag_name()
            .namespace()
            .map_or_else(String::new, |namespace| {
                format!(" xmlns=\"{}\"", xml_escape(namespace))
            });
        edits.push((
            at..at,
            format!(
                "<desc{namespace} id=\"cdudn\" nameSpace=\"{}\">{}</desc>",
                xml_escape(&descriptor.name_space),
                xml_escape(&descriptor.value),
            ),
        ));
    }
    edits.sort_by_key(|(range, _)| range.start);
    let mut result = String::with_capacity(metadata.len());
    let mut previous = 0;
    for (range, replacement) in edits {
        result.push_str(&metadata[previous..range.start]);
        result.push_str(&replacement);
        previous = range.end;
    }
    result.push_str(&metadata[previous..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::didl::parse_didl;

    const NATIVE: &str = include_str!("../../tests/fixtures/didl_native_items.xml");

    fn account() -> DidlDesc {
        DidlDesc {
            id: "cdudn".into(),
            name_space: "urn:example:account".into(),
            value: "household & account".into(),
        }
    }

    #[test]
    fn replay_identity_changes_only_requested_attributes_and_missing_account() {
        let native = concat!(
            "<d:DIDL-Lite xmlns:d='urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/' ",
            "xmlns:v='urn:example:vendor' xml:base='https://example.invalid/'>",
            "<d:item id = 'Q:0/2' parentID = 'Q:0' v:id='unchanged' restricted='true'>",
            "<d:res protocolInfo='audio/flac' bitrate='320000' v:codec='native'>track:first</d:res>",
            "<d:res>track:alternative</d:res><d:desc id='extra' nameSpace='urn:vendor'>keep</d:desc>",
            "<v:extension><![CDATA[exact <bytes> & context]]></v:extension></d:item></d:DIDL-Lite>",
        );
        let repaired =
            with_native_replay_identity(native, "service'item", "-1", &account()).unwrap();
        let expected = native
            .replace("id = 'Q:0/2'", "id = 'service&apos;item'")
            .replace("parentID = 'Q:0'", "parentID = '-1'")
            .replace(
                "<d:res protocolInfo",
                "<desc xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" id=\"cdudn\" nameSpace=\"urn:example:account\">household &amp; account</desc><d:res protocolInfo",
            );
        assert_eq!(repaired, expected);
        assert_eq!(
            native_account_descriptor(&repaired).unwrap(),
            Some(account())
        );
        assert_eq!(
            with_native_replay_identity(&repaired, "service'item", "-1", &account()).unwrap(),
            repaired
        );
        let parsed = parse_didl(&repaired).unwrap();
        assert_eq!(parsed[0].id, "service'item");
        assert_eq!(parsed[0].parent_id, "-1");
    }

    #[test]
    fn existing_accounts_keep_their_bytes_and_unknown_descriptor_order() {
        let native = concat!(
            "<DIDL-Lite><item id=\"old\" other=\"keep\">",
            "<desc id=\"extra\">other service data</desc>",
            "<desc id='cdudn' nameSpace='urn:example:account' extra='keep'>household ",
            "<!-- ignored formatting --><![CDATA[& account]]></desc></item></DIDL-Lite>",
        );
        let repaired = with_native_replay_identity(native, "new", "parent", &account()).unwrap();
        assert_eq!(
            repaired,
            native.replace("id=\"old\"", "id=\"new\" parentID=\"parent\"")
        );
        assert_eq!(
            native_account_descriptor(&repaired).unwrap(),
            Some(account())
        );
    }

    #[test]
    fn foreign_account_named_extensions_remain_opaque() {
        for native_account in [
            "",
            "<d:desc id='cdudn' nameSpace='urn:example:account'>household &amp; account</d:desc>",
        ] {
            let native = format!(
                "<d:DIDL-Lite xmlns:d='urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/' \
                 xmlns:v='urn:example:vendor'><d:item id='old'><d:res>track</d:res>\
                 <v:desc id='cdudn'>unrelated vendor context</v:desc>{native_account}\
                 </d:item></d:DIDL-Lite>"
            );
            let repaired =
                with_native_replay_identity(&native, "new", "parent", &account()).unwrap();
            assert!(repaired.contains("<v:desc id='cdudn'>unrelated vendor context</v:desc>"));
            assert_eq!(
                native_account_descriptor(&repaired).unwrap(),
                Some(account())
            );
            let document = xml::parse(&repaired).unwrap();
            let item = native_item(&document).unwrap();
            assert_eq!(
                item.children()
                    .filter(|node| {
                        node.is_element()
                            && node.tag_name().name() == "desc"
                            && node.tag_name().namespace() == item.tag_name().namespace()
                    })
                    .count(),
                1
            );
        }
    }

    #[test]
    fn replay_identity_rejects_different_ambiguous_and_non_item_accounts() {
        let descriptor =
            "<desc id='cdudn' nameSpace='urn:example:account'>household &amp; account</desc>";
        for native in [
            "<DIDL-Lite><container id='one'/></DIDL-Lite>".into(),
            "<DIDL-Lite><item id='one'/><item id='two'/></DIDL-Lite>".into(),
            "<DIDL-Lite><item><res>track</res></item></DIDL-Lite>".into(),
            "<DIDL-Lite><item id='one'><desc id='cdudn'>different account</desc></item></DIDL-Lite>".into(),
            format!("<DIDL-Lite><item id='one'>{descriptor}{descriptor}</item></DIDL-Lite>"),
            "<DIDL-Lite><item id='one'><desc id='cdudn'><unexpected/></desc></item></DIDL-Lite>".into(),
        ] {
            assert!(
                with_native_replay_identity(&native, "new", "parent", &account()).is_err(),
                "{native}"
            );
        }
    }

    #[test]
    fn original_objects_keep_namespaces_attributes_and_every_native_extension() {
        let retained = parse_didl_with_metadata(NATIVE, 2, 16_384).unwrap();
        let modeled = parse_didl(NATIVE).unwrap();
        assert_eq!(retained.len(), 2);
        for (index, selected) in retained.iter().enumerate() {
            assert_eq!(selected.object, modeled[index]);
            assert_eq!(
                parse_didl(&selected.metadata).unwrap(),
                [modeled[index].clone()]
            );
            let document = xml::parse(NATIVE).unwrap();
            let root = document.root_element();
            let node = root
                .children()
                .filter(roxmltree::Node::is_element)
                .nth(index)
                .unwrap();
            let expected = format!(
                "{}{}{}",
                &NATIVE[root.range().start..root.children().next().unwrap().range().start],
                &NATIVE[node.range()],
                &NATIVE[root.children().next_back().unwrap().range().end..root.range().end],
            );
            assert_eq!(
                selected.metadata, expected,
                "retain exact object and wrapper bytes"
            );
        }
        let first = &retained[0].metadata;
        for extension in [
            "xmlns:vendor=\"urn:example:native-service\"",
            "xml:base=\"https://example.invalid/media/\"",
            "vendor:context=\"source &amp; account\"",
            "refID=\"service-original-1\"",
            "bitrate=\"320000\"",
            "sampleFrequency=\"48000\"",
            "vendor:license=\"account-token\"",
            "<vendor:playback",
            "<desc id=\"extra\"",
            "alternate.flac",
        ] {
            assert!(first.contains(extension), "missing {extension}");
        }
        assert!(!first.contains("Other occurrence"));
        assert!(!retained[1].metadata.contains("service-original-1"));
    }

    #[test]
    fn repeated_root_bytes_are_bounded_before_parsing_or_materializing_items() {
        let text = format!(
            "<DIDL-Lite xmlns:vendor=\"urn:example:{}\"><item/><item/></DIDL-Lite>",
            "x".repeat(1024),
        );
        let error = parse_didl_with_metadata(&text, 2, text.len()).unwrap_err();
        assert!(error.to_string().contains("metadata byte limit"));
        // Within the retained budget, ordinary item validation still applies.
        assert!(
            parse_didl_with_metadata(&text, 2, 4096)
                .unwrap_err()
                .to_string()
                .contains("missing attribute id")
        );
    }

    #[test]
    fn object_and_exact_byte_limits_apply_without_changing_empty_documents() {
        let retained = parse_didl_with_metadata(NATIVE, 2, 16_384).unwrap();
        let bytes = retained.iter().map(|item| item.metadata.len()).sum();
        assert_eq!(
            parse_didl_with_metadata(NATIVE, 2, bytes).unwrap(),
            retained
        );
        assert!(parse_didl_with_metadata(NATIVE, 2, bytes - 1).is_err());
        assert!(parse_didl_with_metadata(NATIVE, 1, 16_384).is_err());
        for empty in ["", "  ", "<DIDL-Lite/>", "<DIDL-Lite> </DIDL-Lite>"] {
            assert!(parse_didl_with_metadata(empty, 0, 0).unwrap().is_empty());
        }
    }

    #[test]
    fn root_cdata_comments_and_processing_instructions_do_not_break_the_isolated_document() {
        let text = concat!(
            "<d:DIDL-Lite xmlns:d=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" ",
            "xmlns:v=\"urn:example:extension\" v:expression=\"a &gt; b\">",
            "<![CDATA[before]]><!-- before --><?fixture before?>",
            "<d:item id=\"one\" v:original=\"yes\"><v:child/></d:item>",
            "text<![CDATA[after]]><!-- after --><?fixture after?>",
            "</d:DIDL-Lite>",
        );
        let retained = parse_didl_with_metadata(text, 1, 4096).unwrap();
        assert_eq!(
            parse_didl(&retained[0].metadata).unwrap(),
            [retained[0].object.clone()]
        );
        assert!(
            retained[0]
                .metadata
                .contains("<d:item id=\"one\" v:original=\"yes\"><v:child/></d:item>")
        );
        assert!(
            retained[0]
                .metadata
                .contains("xmlns:v=\"urn:example:extension\"")
        );
    }
}
