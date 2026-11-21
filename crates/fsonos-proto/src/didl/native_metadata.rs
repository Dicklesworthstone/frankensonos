//! Isolate native objects without rebuilding service-owned playback metadata.

use super::{DidlKind, DidlObject, parse_object};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::didl::parse_didl;

    const NATIVE: &str = include_str!("../../tests/fixtures/didl_native_items.xml");

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
                .filter(|node| node.is_element())
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
