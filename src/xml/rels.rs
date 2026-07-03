//! Hyperlink relationship target sanitization (`word/_rels/*.rels`).
//!
//! Best-effort/optional per DESIGN.md's coverage table ("Hyperlinks |
//! relationship targets in `word/_rels/*.rels` | Best-effort or optional in
//! v1"). A `w:hyperlink` element in a body-story part references a
//! relationship ID (`r:id`); the actual target URL lives in that part's
//! `.rels` file as a `Target` attribute, not inline in the body XML -- a
//! different mechanism from the `HYPERLINK` field code in `w:instrText`
//! (guaranteed scope, see [`crate::xml::instr_text`]).
//!
//! Only *external* hyperlink targets (`TargetMode="External"`) are
//! rewritten. Internal targets (same-package parts, e.g. a cross-reference
//! relationship) are structural package navigation, not user-controlled
//! payload, and are left untouched. Non-hyperlink relationships (styles,
//! images, ...) are always left untouched.
//!
//! Applied unconditionally to every `.rels` part found in the package: this
//! is additive best-effort cleanup, not part of the guaranteed
//! `--include`/`--exclude` scope vocabulary or the strict-mode pass/fail
//! contract.

use quick_xml::NsReader;
use quick_xml::escape::escape;
use quick_xml::events::{BytesStart, Event};

use crate::Error;
use crate::xml::rewrite::{locate_within, matches_namespace};
use crate::xml::text::CANONICAL_HYPERLINK_TARGET;

const PACKAGE_RELATIONSHIPS_NS: &[u8] = b"http://schemas.openxmlformats.org/package/2006/relationships";
const REL_TYPE_HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

/// Rewrite external hyperlink `Target` values in a `.rels` part to
/// [`CANONICAL_HYPERLINK_TARGET`], leaving every other relationship and
/// byte range untouched.
pub fn sanitize_hyperlink_targets(xml: &[u8]) -> Result<Vec<u8>, Error> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out = Vec::with_capacity(xml.len());
    let mut cursor = 0usize;

    loop {
        let (resolved, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Empty(start) | Event::Start(start)
                if matches_namespace(
                    &resolved,
                    start.local_name().as_ref(),
                    b"Relationship",
                    PACKAGE_RELATIONSHIPS_NS,
                ) =>
            {
                let after = reader.buffer_position() as usize;
                write_relationship_tag(&mut out, xml, cursor, after, &start)?;
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

fn write_relationship_tag(
    out: &mut Vec<u8>,
    xml: &[u8],
    tag_start: usize,
    tag_end: usize,
    start: &BytesStart,
) -> Result<(), Error> {
    let mut rel_type = String::new();
    let mut target_mode = String::new();
    let mut target_span = None;

    for attribute in start.attributes().flatten() {
        match attribute.key.as_ref() {
            b"Type" => rel_type = decode_attr(&attribute.value)?,
            b"TargetMode" => target_mode = decode_attr(&attribute.value)?,
            b"Target" => target_span = locate_within(xml, &attribute.value),
            _ => {}
        }
    }

    let is_external_hyperlink = rel_type == REL_TYPE_HYPERLINK && target_mode == "External";

    match (is_external_hyperlink, target_span) {
        (true, Some((value_start, value_end))) => {
            out.extend_from_slice(&xml[tag_start..value_start]);
            out.extend_from_slice(escape(CANONICAL_HYPERLINK_TARGET).as_bytes());
            out.extend_from_slice(&xml[value_end..tag_end]);
        }
        _ => out.extend_from_slice(&xml[tag_start..tag_end]),
    }

    Ok(())
}

fn decode_attr(value: &[u8]) -> Result<String, Error> {
    let lossy = String::from_utf8_lossy(value);
    Ok(quick_xml::escape::unescape(&lossy)?.into_owned())
}

#[cfg(test)]
mod tests {
    use super::sanitize_hyperlink_targets;

    const XMLNS: &str = r#"xmlns="http://schemas.openxmlformats.org/package/2006/relationships""#;

    fn sanitize(body: &str) -> String {
        let wrapped = format!(r#"<Relationships {XMLNS}>{body}</Relationships>"#);
        let out = sanitize_hyperlink_targets(wrapped.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        out.strip_prefix(&format!(r#"<Relationships {XMLNS}>"#))
            .unwrap()
            .strip_suffix("</Relationships>")
            .unwrap()
            .to_string()
    }

    #[test]
    fn rewrites_external_hyperlink_target() {
        let input = r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://internal.example.com/secret" TargetMode="External"/>"#;
        assert_eq!(
            sanitize(input),
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/redacted" TargetMode="External"/>"#
        );
    }

    #[test]
    fn leaves_internal_hyperlink_target_untouched() {
        // No TargetMode (or TargetMode="Internal") means the target is a
        // same-package part, not user-controlled payload.
        let input = r##"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="#_bookmark1"/>"##;
        assert_eq!(sanitize(input), input);
    }

    #[test]
    fn leaves_non_hyperlink_relationships_untouched() {
        let input = r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>"#;
        assert_eq!(sanitize(input), input);
    }

    #[test]
    fn rewrites_only_the_matching_relationship_among_several() {
        let input = r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://a.example" TargetMode="External"/>"#;
        assert_eq!(
            sanitize(input),
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/redacted" TargetMode="External"/>"#
        );
    }

    #[test]
    fn preserves_attribute_order_and_quoting_style() {
        let input = r#"<Relationship TargetMode='External' Target='https://a.example' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink' Id='rId1'/>"#;
        assert_eq!(
            sanitize(input),
            r#"<Relationship TargetMode='External' Target='https://example.invalid/redacted' Type='http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink' Id='rId1'/>"#
        );
    }
}
