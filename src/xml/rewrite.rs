//! Token/span-based XML rewriting.
//!
//! Model chosen after a Step 5 spike comparing Reader->Writer
//! re-serialization against manual byte-span copy-through on tricky
//! fixtures (self-closing elements, mixed attribute quoting,
//! `xml:space="preserve"`, pre-existing comments): both produced identical
//! output on that slice, because quick-xml's `Writer` writes passed-through
//! `BytesStart`/`BytesEnd` events back from their raw captured bytes rather
//! than re-serializing from parsed attribute structure. Byte-span
//! copy-through is used anyway, since later steps rewrite attribute values
//! (`w:author`, `w:date`, docProps) where constructing a new `BytesStart`
//! would force quick-xml to re-serialize attributes from scratch and risk
//! normalizing quote style or escaping -- exactly what DESIGN.md's
//! "Operational Definition of Preserve Shape" rules out. Byte-span
//! copy-through sidesteps that risk entirely by only ever touching the
//! payload byte range itself.
//!
//! Matching is namespace-aware (via `NsReader`), not prefix-literal: OOXML
//! producers may bind the WordprocessingML namespace to any prefix (by
//! convention almost always `w`, but the XML Namespaces spec allows any
//! prefix), and matching on a literal `"w:t"` string would silently leave
//! sensitive payload unsanitized in a namespace-valid document that happens
//! to use a different prefix.

use quick_xml::NsReader;
use quick_xml::escape::escape;
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};

use crate::Error;

/// The WordprocessingML namespace URI, used by `word/document.xml`,
/// headers/footers/footnotes/endnotes, and `word/comments.xml`.
pub const WORDPROCESSINGML_NS: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";

pub(crate) fn matches_namespace(
    resolved: &ResolveResult,
    local: &[u8],
    target_local: &[u8],
    target_ns: &[u8],
) -> bool {
    local == target_local && matches!(resolved, ResolveResult::Bound(Namespace(ns)) if *ns == target_ns)
}

/// Rewrite the text content of every element in namespace `namespace` whose
/// local name matches `local_name` (e.g. namespace
/// [`WORDPROCESSINGML_NS`], local name `b"t"`, for `w:t`), passing every
/// other byte range through unchanged.
///
/// Matching requires both the local name and the resolved namespace: OOXML
/// uses the same local name `t` for both `w:t` (WordprocessingML text) and
/// `a:t` (DrawingML text runs inside shapes/textboxes), and namespace scope
/// keeps a namespace-valid document from having its sensitive payload
/// missed just because it binds a different prefix than the `w:`/`a:`
/// convention.
///
/// `replace` receives the fully unescaped text content of each matched
/// element (or text run within it) and returns its replacement, which is
/// re-escaped before being written back. A matched element's content is
/// buffered across every `Text`/`GeneralRef`/`CData` event it contains and
/// flushed as a single `replace` call at its closing tag -- quick-xml
/// splits text at entity references (`&amp;`, `&#39;`, ...) into separate
/// events, so replacing per-event would silently skip or leak entity-coded
/// characters instead of treating them as part of the sensitive payload.
///
/// A self-closing element (e.g. `<w:t/>`) is only rewritten into an
/// explicit open/close pair if `replace` turns its empty payload into
/// non-empty content; otherwise its self-closing shape is preserved as-is.
/// The same applies to an already-explicit empty pair (`<w:t></w:t>`).
pub fn rewrite_text_elements(
    xml: &[u8],
    namespace: &[u8],
    local_name: &[u8],
    mut replace: impl FnMut(&str) -> String,
) -> Result<Vec<u8>, Error> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out = Vec::with_capacity(xml.len());
    let mut cursor = 0usize;
    let mut depth: usize = 0;
    let mut pending = String::new();

    loop {
        let (resolved, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Empty(start)
                if matches_namespace(&resolved, start.local_name().as_ref(), local_name, namespace) =>
            {
                let after = reader.buffer_position() as usize;
                let replaced = replace("");
                if replaced.is_empty() {
                    out.extend_from_slice(&xml[cursor..after]);
                } else {
                    write_open_tag(&mut out, &start);
                    out.extend_from_slice(escape(replaced.as_str()).as_bytes());
                    write_close_tag(&mut out, &start);
                }
                cursor = after;
            }
            Event::Start(start)
                if matches_namespace(&resolved, start.local_name().as_ref(), local_name, namespace) =>
            {
                let after = reader.buffer_position() as usize;
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
                depth += 1;
                if depth == 1 {
                    pending.clear();
                }
            }
            Event::End(end)
                if depth == 1
                    && matches_namespace(&resolved, end.local_name().as_ref(), local_name, namespace) =>
            {
                let after = reader.buffer_position() as usize;
                let replaced = replace(&pending);
                out.extend_from_slice(escape(replaced.as_str()).as_bytes());
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
                depth -= 1;
            }
            Event::End(end)
                if depth > 0
                    && matches_namespace(&resolved, end.local_name().as_ref(), local_name, namespace) =>
            {
                let after = reader.buffer_position() as usize;
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
                depth -= 1;
            }
            Event::Text(text) if depth > 0 => {
                let after = reader.buffer_position() as usize;
                let decoded = text.decode().map_err(quick_xml::Error::from)?;
                pending.push_str(&decoded);
                cursor = after;
            }
            Event::GeneralRef(reference) if depth > 0 => {
                let after = reader.buffer_position() as usize;
                pending.push(resolve_general_ref(&reference)?);
                cursor = after;
            }
            Event::CData(cdata) if depth > 0 => {
                let after = reader.buffer_position() as usize;
                let decoded = cdata.decode().map_err(quick_xml::Error::from)?;
                pending.push_str(&decoded);
                cursor = after;
            }
            _ => {
                let after = reader.buffer_position() as usize;
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
            }
        }
    }
    out.extend_from_slice(&xml[cursor..]);

    Ok(out)
}

/// Rewrite the text-node payload of *every* element in `xml`, regardless of
/// element name or namespace -- used for `word/customXml/*` parts, whose
/// schema is arbitrary and unknown (unlike every other rewrite pass in this
/// module, which targets one known element).
///
/// Only "leaf" elements (no child element, `Empty` or `Start`/`End`, inside
/// them) have their text replaced. An element that contains both text and a
/// child element (mixed content, e.g. `<a>before<b>x</b>after</a>`) is
/// structurally unusual for a data-storage schema -- rather than risk
/// silently reordering `a`'s "before"/"after" text relative to `b` (which a
/// naive single-buffer-per-element implementation would do, since `b`'s
/// replacement is written at `b`'s original position while `a`'s
/// accumulated text would flush only once at `a`'s closing tag), `a`'s own
/// direct text is left untouched, byte-for-byte, when it has a child.
/// Determining "is this a leaf" requires seeing whether a `Start`/`Empty`
/// event occurs before the matching `End`, which isn't known until that
/// point in the stream -- so this makes two passes: a scan to classify each
/// element as leaf or not (by traversal order), then a rewrite pass that
/// consults that classification.
///
/// A leaf element's text is replaced only if it contains at least one
/// non-whitespace character; a whitespace-only leaf (e.g. pretty-printing
/// indentation between elements) is left byte-for-byte untouched rather than
/// passed through `replace` and re-escaped, so purely cosmetic whitespace
/// isn't gratuitously rewritten.
///
/// As with [`rewrite_text_elements`], entities are unescaped before
/// `replace` sees them and the result is re-escaped on the way back out,
/// and a self-closing element only becomes an explicit open/close pair if
/// `replace` turns its empty payload into non-empty content.
///
/// Returns `(rewritten_xml, skipped_payload)`: `skipped_payload` is `true`
/// if a non-leaf element's own direct text contained at least one
/// non-whitespace character (including any entity reference, conservatively
/// -- an entity almost never decodes to pure whitespace) that was left
/// unrewritten per the mixed-content rule above. Callers must not treat
/// output as fully sanitized when this is `true` -- per DESIGN.md's Default
/// Safety Mode, this should surface as an unsupported-payload finding
/// (fail-closed), the same way an unrecognized `w:instrText` pattern does,
/// not be silently reported as clean.
pub fn rewrite_all_text_nodes(xml: &[u8], mut replace: impl FnMut(&str) -> String) -> Result<(Vec<u8>, bool), Error> {
    let leaf_flags = scan_leaf_flags(xml)?;

    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out = Vec::with_capacity(xml.len());
    let mut cursor = 0usize;
    // Per currently-open element: (traversal index into `leaf_flags`,
    // decoded pending text, byte offset where its content began).
    let mut stack: Vec<(usize, String, usize)> = Vec::new();
    let mut next_index = 0usize;
    let mut skipped_payload = false;

    loop {
        let (_, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Empty(start) => {
                let after = reader.buffer_position() as usize;
                let replaced = replace("");
                if replaced.is_empty() {
                    out.extend_from_slice(&xml[cursor..after]);
                } else {
                    write_open_tag(&mut out, &start);
                    out.extend_from_slice(escape(replaced.as_str()).as_bytes());
                    write_close_tag(&mut out, &start);
                }
                cursor = after;
            }
            Event::Start(_) => {
                let after = reader.buffer_position() as usize;
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
                stack.push((next_index, String::new(), cursor));
                next_index += 1;
            }
            Event::End(_) => {
                let after = reader.buffer_position() as usize;
                if let Some((index, pending, content_start)) = stack.pop()
                    && leaf_flags[index]
                {
                    // A leaf's text is buffered (not streamed through as
                    // encountered) precisely because leaves never contain a
                    // child whose own output could be duplicated by this --
                    // see the is_leaf check in the Text/CData/GeneralRef
                    // handlers below. `content_start..cursor` is safe to use
                    // here (unlike at a non-leaf's closing tag) exactly
                    // because a leaf has no child between its own Start and
                    // End to have already written bytes into that range.
                    if pending.trim().is_empty() {
                        out.extend_from_slice(&xml[content_start..cursor]);
                    } else {
                        let replaced = replace(&pending);
                        out.extend_from_slice(escape(replaced.as_str()).as_bytes());
                    }
                }
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
            }
            // Non-leaf elements (and content outside any element, e.g.
            // whitespace around the root) stream their text straight
            // through unchanged, exactly like the catch-all fallback below
            // -- only a leaf's text is buffered for a single replace() call
            // at its closing tag. Deciding this at Text/CData/GeneralRef
            // time (not retroactively at End) is what avoids duplicating a
            // child's already-emitted bytes into its non-leaf parent.
            Event::Text(text) if !stack.is_empty() => {
                let after = reader.buffer_position() as usize;
                let (index, ..) = *stack.last().unwrap();
                if leaf_flags[index] {
                    let decoded = text.decode().map_err(quick_xml::Error::from)?;
                    stack.last_mut().unwrap().1.push_str(&decoded);
                } else {
                    let decoded = text.decode().map_err(quick_xml::Error::from)?;
                    if !decoded.trim().is_empty() {
                        skipped_payload = true;
                    }
                    out.extend_from_slice(&xml[cursor..after]);
                }
                cursor = after;
            }
            Event::GeneralRef(reference) if !stack.is_empty() => {
                let after = reader.buffer_position() as usize;
                let (index, ..) = *stack.last().unwrap();
                if leaf_flags[index] {
                    stack.last_mut().unwrap().1.push(resolve_general_ref(&reference)?);
                } else {
                    // Conservative: an entity reference almost never
                    // decodes to pure whitespace, so treat its presence in
                    // a non-leaf's direct text as skipped payload without
                    // needing to resolve it.
                    skipped_payload = true;
                    out.extend_from_slice(&xml[cursor..after]);
                }
                cursor = after;
            }
            Event::CData(cdata) if !stack.is_empty() => {
                let after = reader.buffer_position() as usize;
                let (index, ..) = *stack.last().unwrap();
                if leaf_flags[index] {
                    let decoded = cdata.decode().map_err(quick_xml::Error::from)?;
                    stack.last_mut().unwrap().1.push_str(&decoded);
                } else {
                    let decoded = cdata.decode().map_err(quick_xml::Error::from)?;
                    if !decoded.trim().is_empty() {
                        skipped_payload = true;
                    }
                    out.extend_from_slice(&xml[cursor..after]);
                }
                cursor = after;
            }
            _ => {
                let after = reader.buffer_position() as usize;
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
            }
        }
    }
    out.extend_from_slice(&xml[cursor..]);

    Ok((out, skipped_payload))
}

/// Classify every element in `xml` (in traversal/`Start`-event order) as a
/// leaf (`true`, no child `Start`/`Empty` event before its matching `End`)
/// or not (`false`), for [`rewrite_all_text_nodes`].
fn scan_leaf_flags(xml: &[u8]) -> Result<Vec<bool>, Error> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut leaf_flags: Vec<bool> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    loop {
        let (_, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Start(_) => {
                if let Some(&parent_index) = stack.last() {
                    leaf_flags[parent_index] = false;
                }
                leaf_flags.push(true);
                stack.push(leaf_flags.len() - 1);
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Empty(_) => {
                if let Some(&parent_index) = stack.last() {
                    leaf_flags[parent_index] = false;
                }
            }
            _ => {}
        }
    }

    Ok(leaf_flags)
}

/// Rewrite the value of every attribute in namespace `namespace` whose local
/// name matches `local_attr_name` (e.g. namespace [`WORDPROCESSINGML_NS`],
/// local name `b"author"`, for `w:author`), on any element, passing every
/// other byte through unchanged -- including the rest of the attribute
/// list, its order, and its quoting style.
///
/// Uses quick-xml's zero-copy borrowing reader (`NsReader::read_resolved_event`):
/// an `Attribute::value` then genuinely borrows from `xml` itself, so its
/// byte range within `xml` can be recovered via pointer arithmetic and
/// spliced directly, without reconstructing the surrounding tag (which
/// risks re-serialization drift, per this module's doc comment).
pub fn rewrite_attribute_values(
    xml: &[u8],
    namespace: &[u8],
    local_attr_name: &[u8],
    mut replace: impl FnMut(&str) -> String,
) -> Result<Vec<u8>, Error> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out = Vec::with_capacity(xml.len());
    let mut cursor = 0usize;

    loop {
        let (_, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Start(start) | Event::Empty(start) => {
                let after = reader.buffer_position() as usize;
                write_tag_with_rewritten_attribute(
                    &mut out,
                    xml,
                    cursor,
                    after,
                    &reader,
                    &start,
                    namespace,
                    local_attr_name,
                    &mut replace,
                )?;
                cursor = after;
            }
            _ => {
                let after = reader.buffer_position() as usize;
                out.extend_from_slice(&xml[cursor..after]);
                cursor = after;
            }
        }
    }
    out.extend_from_slice(&xml[cursor..]);

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn write_tag_with_rewritten_attribute(
    out: &mut Vec<u8>,
    xml: &[u8],
    tag_start: usize,
    tag_end: usize,
    reader: &NsReader<&[u8]>,
    start: &BytesStart,
    namespace: &[u8],
    local_attr_name: &[u8],
    replace: &mut impl FnMut(&str) -> String,
) -> Result<(), Error> {
    let mut target = None;
    for attribute in start.attributes().flatten() {
        let (resolved, local) = reader.resolve_attribute(attribute.key);
        if matches_namespace(&resolved, local.as_ref(), local_attr_name, namespace) {
            target = Some(attribute.value);
            break;
        }
    }

    let Some(value_bytes) = target else {
        out.extend_from_slice(&xml[tag_start..tag_end]);
        return Ok(());
    };

    let (value_start, value_end) = locate_within(xml, &value_bytes)
        .ok_or(Error::Unsupported("could not locate attribute value bytes for rewriting"))?;

    let decoded = String::from_utf8_lossy(&value_bytes);
    let original = quick_xml::escape::unescape(&decoded)?;
    let replaced = replace(&original);

    out.extend_from_slice(&xml[tag_start..value_start]);
    out.extend_from_slice(escape(replaced.as_str()).as_bytes());
    out.extend_from_slice(&xml[value_end..tag_end]);

    Ok(())
}

/// Recover `sub`'s byte offset range within `parent`, assuming `sub` is a
/// genuine subslice of `parent` (true for attribute values read via
/// quick-xml's zero-copy borrowing reader). Returns `None` if `sub` does
/// not lie within `parent`'s memory range, which should not happen in
/// practice but is checked rather than assumed.
pub(crate) fn locate_within(parent: &[u8], sub: &[u8]) -> Option<(usize, usize)> {
    let parent_start = parent.as_ptr() as usize;
    let parent_end = parent_start + parent.len();
    let sub_start = sub.as_ptr() as usize;
    let sub_end = sub_start + sub.len();

    if sub_start >= parent_start && sub_end <= parent_end {
        Some((sub_start - parent_start, sub_end - parent_start))
    } else {
        None
    }
}

pub(crate) fn resolve_general_ref(reference: &BytesRef) -> Result<char, Error> {
    if let Some(c) = reference.resolve_char_ref()? {
        return Ok(c);
    }

    let name = reference.decode().map_err(quick_xml::Error::from)?;
    match name.as_ref() {
        "amp" => Ok('&'),
        "lt" => Ok('<'),
        "gt" => Ok('>'),
        "apos" => Ok('\''),
        "quot" => Ok('"'),
        _ => Err(Error::Unsupported("unrecognized XML entity in text payload")),
    }
}

fn write_open_tag(out: &mut Vec<u8>, start: &BytesStart) {
    out.push(b'<');
    out.extend_from_slice(start);
    out.push(b'>');
}

fn write_close_tag(out: &mut Vec<u8>, start: &BytesStart) {
    out.extend_from_slice(b"</");
    out.extend_from_slice(start.name().as_ref());
    out.push(b'>');
}

#[cfg(test)]
mod tests {
    use super::{WORDPROCESSINGML_NS, rewrite_all_text_nodes, rewrite_attribute_values, rewrite_text_elements};

    const XMLNS_W: &str =
        r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn placeholder(text: &str) -> String {
        text.chars().map(|_| 'X').collect()
    }

    /// Wraps a body fragment in a root element declaring the WordprocessingML
    /// namespace under the conventional `w:` prefix, and strips that wrapper
    /// back off the result, so tests can focus on the fragment under test.
    fn with_w_namespace(body: &str) -> String {
        format!(r#"<w:root {XMLNS_W}>{body}</w:root>"#)
    }

    fn unwrap_w_namespace(xml: &str) -> String {
        let inner = xml
            .strip_prefix(&format!(r#"<w:root {XMLNS_W}>"#))
            .unwrap()
            .strip_suffix("</w:root>")
            .unwrap();
        inner.to_string()
    }

    fn rewrite_wt(body: &str) -> String {
        let wrapped = with_w_namespace(body);
        let out = rewrite_text_elements(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"t", placeholder).unwrap();
        unwrap_w_namespace(&String::from_utf8(out).unwrap())
    }

    #[test]
    fn replaces_simple_text() {
        let input = r#"<w:p><w:r><w:t>Hello</w:t></w:r></w:p>"#;
        assert_eq!(rewrite_wt(input), r#"<w:p><w:r><w:t>XXXXX</w:t></w:r></w:p>"#);
    }

    #[test]
    fn replaces_split_runs_independently() {
        let input =
            r#"<w:p><w:r><w:t xml:space="preserve">Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>"#;
        assert_eq!(
            rewrite_wt(input),
            r#"<w:p><w:r><w:t xml:space="preserve">XXXXXX</w:t></w:r><w:r><w:t>XXXXX</w:t></w:r></w:p>"#
        );
    }

    #[test]
    fn preserves_xml_space_preserve_and_attribute_quoting() {
        let input = r#"<w:p><w:t xml:space='preserve'>Hello  world</w:t></w:p>"#;
        assert_eq!(rewrite_wt(input), r#"<w:p><w:t xml:space='preserve'>XXXXXXXXXXXX</w:t></w:p>"#);
    }

    #[test]
    fn empty_runs_stay_empty() {
        let input = r#"<w:p><w:r><w:t></w:t></w:r></w:p>"#;
        assert_eq!(rewrite_wt(input), input);
    }

    #[test]
    fn self_closing_empty_element_stays_self_closing() {
        let input = r#"<w:p><w:r><w:t/></w:r></w:p>"#;
        assert_eq!(rewrite_wt(input), input);
    }

    #[test]
    fn self_closing_element_becomes_explicit_when_content_is_written() {
        let wrapped = with_w_namespace(r#"<w:t/>"#);
        let out = rewrite_text_elements(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"t", |_| "X".to_string()).unwrap();
        assert_eq!(unwrap_w_namespace(&String::from_utf8(out).unwrap()), r#"<w:t>X</w:t>"#);
    }

    #[test]
    fn preserves_xml_comments_and_untouched_siblings() {
        let input = r#"<w:p><w:r><w:t>A</w:t></w:r><!-- keep me --><w:r><w:t>B</w:t></w:r></w:p>"#;
        assert_eq!(
            rewrite_wt(input),
            r#"<w:p><w:r><w:t>X</w:t></w:r><!-- keep me --><w:r><w:t>X</w:t></w:r></w:p>"#
        );
    }

    #[test]
    fn unescapes_entities_before_replace_and_reescapes_output() {
        let wrapped = with_w_namespace(r#"<w:t>A &amp; B &lt;tag&gt;</w:t>"#);
        let out = rewrite_text_elements(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"t", |text| {
            assert_eq!(text, "A & B <tag>");
            "&<>".to_string()
        })
        .unwrap();
        assert_eq!(
            unwrap_w_namespace(&String::from_utf8(out).unwrap()),
            r#"<w:t>&amp;&lt;&gt;</w:t>"#
        );
    }

    #[test]
    fn resolves_numeric_and_apos_quot_entities() {
        let wrapped = with_w_namespace(r#"<w:t>&#39;&#x27;&apos;&quot;</w:t>"#);
        let out = rewrite_text_elements(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"t", |text| {
            assert_eq!(text, "'''\"");
            "R".to_string()
        })
        .unwrap();
        assert_eq!(unwrap_w_namespace(&String::from_utf8(out).unwrap()), r#"<w:t>R</w:t>"#);
    }

    #[test]
    fn treats_cdata_as_opaque_replaceable_payload() {
        let wrapped = with_w_namespace(r#"<w:t><![CDATA[secret]]></w:t>"#);
        let out = rewrite_text_elements(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"t", |text| {
            assert_eq!(text, "secret");
            placeholder(text)
        })
        .unwrap();
        assert_eq!(unwrap_w_namespace(&String::from_utf8(out).unwrap()), r#"<w:t>XXXXXX</w:t>"#);
    }

    #[test]
    fn leaves_unrelated_elements_untouched() {
        let input = r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>Hi</w:t></w:r></w:p>"#;
        assert_eq!(
            rewrite_wt(input),
            r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>XX</w:t></w:r></w:p>"#
        );
    }

    #[test]
    fn does_not_touch_drawingml_a_t_despite_matching_local_name() {
        // a:t (DrawingML text run inside a shape/textbox) shares the local
        // name "t" with w:t but must not be rewritten by a w:t-scoped pass.
        let input = r#"<w:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:r><w:t>Hi</w:t></w:r><mc:AlternateContent><a:t>Shape text</a:t></mc:AlternateContent></w:p>"#;
        assert_eq!(
            rewrite_wt(input),
            r#"<w:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:r><w:t>XX</w:t></w:r><mc:AlternateContent><a:t>Shape text</a:t></mc:AlternateContent></w:p>"#
        );
    }

    #[test]
    fn does_not_touch_same_local_name_bound_to_a_different_namespace_via_the_w_prefix() {
        // A pathological but namespace-valid document that rebinds the `w:`
        // prefix (locally, on one element) to a non-WordprocessingML
        // namespace must not have that element's `t` child rewritten --
        // matching must follow the resolved namespace, not the literal
        // prefix string.
        let input = r#"<w:p><w:weird xmlns:w="urn:not-wordprocessingml"><w:t>Not real payload</w:t></w:weird></w:p>"#;
        assert_eq!(rewrite_wt(input), input);
    }

    #[test]
    fn rewrites_element_regardless_of_prefix_bound_to_the_namespace() {
        // Same WordprocessingML namespace, different (but namespace-valid)
        // prefix -- must still be rewritten since matching is namespace-,
        // not prefix-, based.
        let input = r#"<ns0:root xmlns:ns0="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><ns0:p><ns0:r><ns0:t>Secret</ns0:t></ns0:r></ns0:p></ns0:root>"#;
        let out = rewrite_text_elements(input.as_bytes(), WORDPROCESSINGML_NS, b"t", placeholder).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"<ns0:root xmlns:ns0="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><ns0:p><ns0:r><ns0:t>XXXXXX</ns0:t></ns0:r></ns0:p></ns0:root>"#
        );
    }

    fn rewrite_author(body: &str) -> String {
        let wrapped = with_w_namespace(body);
        let out = rewrite_attribute_values(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"author", |_| "REDACTED".to_string())
            .unwrap();
        unwrap_w_namespace(&String::from_utf8(out).unwrap())
    }

    #[test]
    fn rewrites_double_quoted_attribute_value() {
        let input = r#"<w:ins w:id="1" w:author="Alice Cooper" w:date="2026-01-01T00:00:00Z"/>"#;
        assert_eq!(
            rewrite_author(input),
            r#"<w:ins w:id="1" w:author="REDACTED" w:date="2026-01-01T00:00:00Z"/>"#
        );
    }

    #[test]
    fn rewrites_single_quoted_attribute_value_and_keeps_quote_style() {
        let input = r#"<w:ins w:id='1' w:author='Alice Cooper'/>"#;
        assert_eq!(rewrite_author(input), r#"<w:ins w:id='1' w:author='REDACTED'/>"#);
    }

    #[test]
    fn leaves_tags_without_the_target_attribute_untouched() {
        let input = r#"<w:p><w:r w:author="Alice"/><w:t>Hi</w:t></w:p>"#;
        assert_eq!(
            rewrite_author(input),
            r#"<w:p><w:r w:author="REDACTED"/><w:t>Hi</w:t></w:p>"#
        );
    }

    #[test]
    fn does_not_touch_unrelated_attributes_or_elements() {
        let input = r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t>Hi</w:t></w:r></w:p>"#;
        assert_eq!(rewrite_author(input), input);
    }

    #[test]
    fn rewrites_multiple_elements_independently() {
        let input = r#"<w:del w:author="Alice"><w:r/></w:del><w:ins w:author="Bob"><w:r/></w:ins>"#;
        assert_eq!(
            rewrite_author(input),
            r#"<w:del w:author="REDACTED"><w:r/></w:del><w:ins w:author="REDACTED"><w:r/></w:ins>"#
        );
    }

    #[test]
    fn unescapes_entities_in_attribute_value_before_replace() {
        let wrapped = with_w_namespace(r#"<w:ins w:author="Alice &amp; Bob"/>"#);
        let out = rewrite_attribute_values(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"author", |value| {
            assert_eq!(value, "Alice & Bob");
            "X".to_string()
        })
        .unwrap();
        assert_eq!(
            unwrap_w_namespace(&String::from_utf8(out).unwrap()),
            r#"<w:ins w:author="X"/>"#
        );
    }

    #[test]
    fn reescapes_replacement_value_written_back() {
        let wrapped = with_w_namespace(r#"<w:ins w:author="Alice"/>"#);
        let out = rewrite_attribute_values(wrapped.as_bytes(), WORDPROCESSINGML_NS, b"author", |_| {
            r#"<"quoted">"#.to_string()
        })
        .unwrap();
        assert_eq!(
            unwrap_w_namespace(&String::from_utf8(out).unwrap()),
            r#"<w:ins w:author="&lt;&quot;quoted&quot;&gt;"/>"#
        );
    }

    #[test]
    fn does_not_touch_attribute_with_matching_local_name_but_no_namespace() {
        // A bare, unprefixed `author` attribute is not bound to any
        // namespace under XML namespace rules, even inside an element that
        // has a default (unprefixed) namespace -- default namespaces do not
        // apply to attributes. Must not be treated as `w:author`.
        let input = r#"<w:ins author="Alice"/>"#;
        assert_eq!(rewrite_author(input), input);
    }

    fn rewrite_all(xml: &str) -> String {
        let (out, _) = rewrite_all_text_nodes(xml.as_bytes(), placeholder).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn replaces_leaf_element_text_regardless_of_name_or_namespace() {
        let input = r#"<root xmlns:go="urn:example"><go:field>Secret</go:field></root>"#;
        assert_eq!(
            rewrite_all(input),
            r#"<root xmlns:go="urn:example"><go:field>XXXXXX</go:field></root>"#
        );
    }

    #[test]
    fn replaces_multiple_independently_nested_leaves() {
        let input = r#"<root><a><b>one</b><c>two</c></a></root>"#;
        assert_eq!(rewrite_all(input), r#"<root><a><b>XXX</b><c>XXX</c></a></root>"#);
    }

    #[test]
    fn leaves_mixed_content_elements_own_text_untouched() {
        // `a` has both text ("before"/"after") and a child element `b` --
        // `b`'s leaf text is still replaced at its original position, but
        // `a`'s own interleaved text is left byte-for-byte untouched rather
        // than risk reordering it.
        let input = r#"<a>before<b>x</b>after</a>"#;
        assert_eq!(rewrite_all(input), r#"<a>before<b>X</b>after</a>"#);
    }

    #[test]
    fn flags_skipped_payload_when_mixed_content_text_is_left_untouched() {
        let input = r#"<a>before<b>x</b>after</a>"#;
        let (_, skipped) = rewrite_all_text_nodes(input.as_bytes(), placeholder).unwrap();
        assert!(skipped);
    }

    #[test]
    fn does_not_flag_skipped_payload_when_there_is_no_mixed_content() {
        let input = r#"<root><a><b>one</b><c>two</c></a></root>"#;
        let (_, skipped) = rewrite_all_text_nodes(input.as_bytes(), placeholder).unwrap();
        assert!(!skipped);
    }

    #[test]
    fn does_not_flag_skipped_payload_for_whitespace_only_mixed_content() {
        // `a`'s own direct text is pure whitespace (pretty-printing), not
        // real payload -- must not be flagged as skipped.
        let input = "<a>\n  <b>x</b>\n</a>";
        let (_, skipped) = rewrite_all_text_nodes(input.as_bytes(), placeholder).unwrap();
        assert!(!skipped);
    }

    #[test]
    fn flags_skipped_payload_for_an_entity_reference_in_mixed_content() {
        let input = r#"<a>A &amp; B<b>x</b></a>"#;
        let (_, skipped) = rewrite_all_text_nodes(input.as_bytes(), placeholder).unwrap();
        assert!(skipped);
    }

    #[test]
    fn leaves_whitespace_only_leaf_text_untouched() {
        let input = "<root>\n  <a>   </a>\n  <b>Secret</b>\n</root>";
        assert_eq!(
            rewrite_all(input),
            "<root>\n  <a>   </a>\n  <b>XXXXXX</b>\n</root>"
        );
    }

    #[test]
    fn element_with_only_child_elements_has_no_text_to_replace() {
        let input = r#"<root><a><b>x</b><c>y</c></a></root>"#;
        assert_eq!(rewrite_all(input), r#"<root><a><b>X</b><c>X</c></a></root>"#);
    }

    #[test]
    fn self_closing_element_stays_self_closing() {
        let input = r#"<root><a/></root>"#;
        assert_eq!(rewrite_all(input), input);
    }

    #[test]
    fn self_closing_element_in_arbitrary_schema_becomes_explicit_when_content_is_written() {
        let (out, _) = rewrite_all_text_nodes(b"<root><a/></root>", |_| "X".to_string()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), r#"<root><a>X</a></root>"#);
    }

    #[test]
    fn unescapes_entities_and_supports_cdata_like_rewrite_text_elements() {
        let input = r#"<root><a>A &amp; B</a><b><![CDATA[secret]]></b></root>"#;
        let (out, skipped) = rewrite_all_text_nodes(input.as_bytes(), |text| text.to_uppercase()).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"<root><a>A &amp; B</a><b>SECRET</b></root>"#
        );
        assert!(!skipped, "both a and b are leaves, nothing should be skipped");
    }

    #[test]
    fn preserves_attributes_and_only_touches_text() {
        let input = r#"<root><item id="{GUID}" type="string">Secret</item></root>"#;
        assert_eq!(
            rewrite_all(input),
            r#"<root><item id="{GUID}" type="string">XXXXXX</item></root>"#
        );
    }
}
