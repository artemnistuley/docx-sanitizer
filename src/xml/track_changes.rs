//! `--remove-track-changes`: collapse tracked changes to their "accepted"
//! state.
//!
//! Per DESIGN.md's "Tracked Changes": tracked revision structure is
//! preserved by default, and this is the opt-in, convenience-oriented
//! alternative that collapses a document to its current visible-text view.
//! Unlike the rest of this crate, this is a *structural* transformation
//! (elements are removed or unwrapped), not a payload replacement -- it's
//! explicitly not the default preservation path.
//!
//! Scope:
//! - Fully removed (element and all its content): `w:del` (deleted text
//!   must not survive into the accepted view), `w:moveFrom` (content that
//!   moved away), the "previous formatting" change records (`w:rPrChange`,
//!   `w:pPrChange`, `w:sectPrChange`, `w:tblPrChange`, `w:tcPrChange`,
//!   `w:trPrChange`, `w:tblGridChange`, `w:numberingChange`), and the
//!   move-range bookmark markers (`w:moveFromRangeStart`/`End`,
//!   `w:moveToRangeStart`/`End` -- empty markers, no content either way).
//! - Unwrapped (wrapper removed, content kept): `w:ins` (inserted text
//!   becomes ordinary text), `w:moveTo` (content at its new location
//!   becomes ordinary text).
//! - Out of scope for v1: table-row/cell-level tracked changes
//!   (`w:cellIns`/`w:cellDel`/`w:cellMerge`) -- rarer and structurally more
//!   involved than run/paragraph-level changes.
//!
//! Applied *before* the normal payload rewrite passes: once `w:del`
//! content is gone, there is nothing left in it that needs sanitizing.

use quick_xml::NsReader;
use quick_xml::events::Event;

use crate::Error;
use crate::xml::rewrite::{WORDPROCESSINGML_NS, matches_namespace};

const FULL_REMOVE_NAMES: &[&[u8]] = &[
    b"del",
    b"moveFrom",
    b"rPrChange",
    b"pPrChange",
    b"sectPrChange",
    b"tblPrChange",
    b"tcPrChange",
    b"trPrChange",
    b"tblGridChange",
    b"numberingChange",
    b"moveFromRangeStart",
    b"moveFromRangeEnd",
    b"moveToRangeStart",
    b"moveToRangeEnd",
];

const UNWRAP_NAMES: &[&[u8]] = &[b"ins", b"moveTo"];

/// Collapse tracked changes in a WordprocessingML body-story part to their
/// accepted state (see module docs for exactly which elements are removed
/// vs. unwrapped).
pub fn remove_track_changes(xml: &[u8]) -> Result<Vec<u8>, Error> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out = Vec::with_capacity(xml.len());
    let mut cursor = 0usize;
    let mut remove_depth: usize = 0;
    // Per currently-open element (outside any removed region): true if it's
    // an unwrap tag whose Start/End bytes were skipped and must stay
    // skipped symmetrically at its End.
    let mut unwrap_stack: Vec<bool> = Vec::new();

    loop {
        let (resolved, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Empty(start) => {
                // A self-closing unwrap tag (e.g. `<w:ins/>`) has no
                // content to preserve, so "unwrap" and "remove" produce the
                // same result here: drop it, same as a full-remove tag.
                let drop = is_full_remove(&resolved, start.local_name().as_ref())
                    || is_unwrap(&resolved, start.local_name().as_ref());
                let after = reader.buffer_position() as usize;
                if remove_depth > 0 || drop {
                    cursor = after;
                } else {
                    out.extend_from_slice(&xml[cursor..after]);
                    cursor = after;
                }
            }
            Event::Start(start) => {
                let full_remove = is_full_remove(&resolved, start.local_name().as_ref());
                let unwrap = is_unwrap(&resolved, start.local_name().as_ref());
                let after = reader.buffer_position() as usize;
                if remove_depth > 0 {
                    remove_depth += 1;
                    cursor = after;
                } else if full_remove {
                    remove_depth = 1;
                    cursor = after;
                } else if unwrap {
                    unwrap_stack.push(true);
                    cursor = after;
                } else {
                    unwrap_stack.push(false);
                    out.extend_from_slice(&xml[cursor..after]);
                    cursor = after;
                }
            }
            Event::End(_) => {
                let after = reader.buffer_position() as usize;
                if remove_depth > 0 {
                    remove_depth -= 1;
                    cursor = after;
                } else if unwrap_stack.pop().unwrap_or(false) {
                    cursor = after;
                } else {
                    out.extend_from_slice(&xml[cursor..after]);
                    cursor = after;
                }
            }
            _ => {
                let after = reader.buffer_position() as usize;
                if remove_depth > 0 {
                    cursor = after;
                } else {
                    out.extend_from_slice(&xml[cursor..after]);
                    cursor = after;
                }
            }
        }
    }
    out.extend_from_slice(&xml[cursor..]);

    Ok(out)
}

fn is_full_remove(resolved: &quick_xml::name::ResolveResult, local: &[u8]) -> bool {
    FULL_REMOVE_NAMES
        .iter()
        .any(|name| matches_namespace(resolved, local, name, WORDPROCESSINGML_NS))
}

fn is_unwrap(resolved: &quick_xml::name::ResolveResult, local: &[u8]) -> bool {
    UNWRAP_NAMES
        .iter()
        .any(|name| matches_namespace(resolved, local, name, WORDPROCESSINGML_NS))
}

#[cfg(test)]
mod tests {
    use super::remove_track_changes;

    const XMLNS_W: &str =
        r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn strip(body: &str) -> String {
        let wrapped = format!(r#"<w:root {XMLNS_W}>{body}</w:root>"#);
        let out = remove_track_changes(wrapped.as_bytes()).unwrap();
        let out = String::from_utf8(out).unwrap();
        out.strip_prefix(&format!(r#"<w:root {XMLNS_W}>"#))
            .unwrap()
            .strip_suffix("</w:root>")
            .unwrap()
            .to_string()
    }

    #[test]
    fn removes_deleted_text_entirely() {
        let input = r#"<w:p><w:del w:id="1" w:author="Alice"><w:r><w:delText>Old</w:delText></w:r></w:del></w:p>"#;
        assert_eq!(strip(input), r#"<w:p></w:p>"#);
    }

    #[test]
    fn unwraps_inserted_text_keeping_content() {
        let input = r#"<w:p><w:ins w:id="2" w:author="Alice"><w:r><w:t>New</w:t></w:r></w:ins></w:p>"#;
        assert_eq!(strip(input), r#"<w:p><w:r><w:t>New</w:t></w:r></w:p>"#);
    }

    #[test]
    fn combines_deletion_and_insertion_into_accepted_text() {
        let input = r#"<w:p><w:del w:author="Alice"><w:r><w:delText>Old</w:delText></w:r></w:del><w:ins w:author="Alice"><w:r><w:t>New</w:t></w:r></w:ins></w:p>"#;
        assert_eq!(strip(input), r#"<w:p><w:r><w:t>New</w:t></w:r></w:p>"#);
    }

    #[test]
    fn removes_move_from_and_unwraps_move_to() {
        let input = r#"<w:p><w:moveFromRangeStart w:id="1" w:name="m1"/><w:r><w:moveFrom w:author="A"><w:r><w:t>Moved</w:t></w:r></w:moveFrom></w:r><w:moveFromRangeEnd w:id="1"/></w:p><w:p><w:moveToRangeStart w:id="2" w:name="m2"/><w:r><w:moveTo w:author="A"><w:r><w:t>Moved</w:t></w:r></w:moveTo></w:r><w:moveToRangeEnd w:id="2"/></w:p>"#;
        assert_eq!(
            strip(input),
            r#"<w:p><w:r></w:r></w:p><w:p><w:r><w:r><w:t>Moved</w:t></w:r></w:r></w:p>"#
        );
    }

    #[test]
    fn removes_formatting_change_records() {
        let input = r#"<w:r><w:rPr><w:b/><w:rPrChange w:id="9" w:author="Alice"><w:rPr/></w:rPrChange></w:rPr><w:t>Styled</w:t></w:r>"#;
        assert_eq!(strip(input), r#"<w:r><w:rPr><w:b/></w:rPr><w:t>Styled</w:t></w:r>"#);
    }

    #[test]
    fn leaves_document_without_track_changes_untouched() {
        let input = r#"<w:p><w:r><w:t>Plain text</w:t></w:r></w:p>"#;
        assert_eq!(strip(input), input);
    }

    #[test]
    fn removes_self_closing_empty_insertion_and_move_to_wrappers() {
        // A content-free w:ins/w:moveTo has nothing to unwrap-and-keep, so
        // it must vanish entirely, same as the full-remove tags -- not be
        // passed through untouched.
        let input = r#"<w:p><w:ins w:id="1" w:author="A"/><w:moveTo w:id="2" w:author="A"/><w:r><w:t>Kept</w:t></w:r></w:p>"#;
        assert_eq!(strip(input), r#"<w:p><w:r><w:t>Kept</w:t></w:r></w:p>"#);
    }

    #[test]
    fn preserves_surrounding_whitespace_and_attribute_quoting() {
        let input =
            r#"<w:p><w:r><w:t xml:space='preserve'>Kept </w:t></w:r><w:del w:author="A"><w:r><w:delText>x</w:delText></w:r></w:del></w:p>"#;
        assert_eq!(
            strip(input),
            r#"<w:p><w:r><w:t xml:space='preserve'>Kept </w:t></w:r></w:p>"#
        );
    }

    #[test]
    fn removes_nested_run_properties_change_inside_deleted_content() {
        // A deletion whose run also carries other track-changes-adjacent
        // markup must vanish as one unit, not leave orphaned fragments.
        let input = r#"<w:del w:author="A"><w:r><w:rPr><w:rPrChange w:author="A"><w:rPr/></w:rPrChange></w:rPr><w:delText>Old</w:delText></w:r></w:del>"#;
        assert_eq!(strip(input), "");
    }
}
