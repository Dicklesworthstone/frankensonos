//! Thin helpers over `roxmltree` for the XML Sonos speaks.
//!
//! Sonos documents are namespaced (`dc:`, `upnp:`, `r:`, `s:`) but never reuse
//! a local name across namespaces in a way that matters to us, so lookups
//! match on the local name. That keeps parsing robust to prefix changes
//! between firmware lines.

use crate::ProtoError;
use roxmltree::{Document, Node};

/// Parse `text` into a read-only DOM.
pub(crate) fn parse(text: &str) -> Result<Document<'_>, ProtoError> {
    Document::parse(text).map_err(|e| ProtoError::Malformed(format!("xml: {e}")))
}

/// First child element of `node` whose local name is `name`.
pub(crate) fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

/// Every child element of `node` whose local name is `name`.
pub(crate) fn children<'a, 'i: 'a>(
    node: Node<'a, 'i>,
    name: &'a str,
) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    node.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name)
}

/// Decoded text of the child element `name`; an empty element yields `""`.
pub(crate) fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(node, name).map(|c| c.text().unwrap_or(""))
}

/// Like [`child_text`], but `None` for an absent or empty element.
pub(crate) fn child_text_nonempty<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child_text(node, name).filter(|t| !t.is_empty())
}

/// Attribute `name`, failing with a descriptive error when it is missing.
pub(crate) fn require_attr<'a>(node: Node<'a, '_>, name: &str) -> Result<&'a str, ProtoError> {
    node.attribute(name).ok_or_else(|| {
        ProtoError::Malformed(format!(
            "<{}> is missing attribute {name}",
            node.tag_name().name()
        ))
    })
}
