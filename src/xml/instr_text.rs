//! Conservative sanitization for `w:instrText` field instructions.
//!
//! Per DESIGN.md's "Field Instruction Text": `w:instrText` is guaranteed
//! v1 scope, but only a narrow, explicitly recognized set of
//! field-instruction patterns is rewritten. Anything outside that set is an
//! unsupported payload surface, not silently passed through as if it were
//! safe -- callers get an [`InstrTextOutcome::Unrecognized`] signal so
//! Step 7's policy layer can turn it into a strict-mode failure (or a
//! recorded skip in `--best-effort`).
//!
//! Recognized patterns are deliberately narrow, not just "the field
//! keyword we know about": a `HYPERLINK` field is only recognized as the
//! exact form `HYPERLINK "<url>"` with nothing else present. Any switches
//! (`\o "tooltip"`, `\t "frame"`, ...) push it to `Unrecognized`, even
//! though we understand the `HYPERLINK` keyword, because those switches can
//! carry arbitrary user-controlled quoted text and rewriting only the URL
//! while leaving them untouched would give a false impression that the
//! whole field was sanitized.
//!
//! ## Field instructions spanning multiple `w:instrText` elements
//!
//! Word routinely splits a single field instruction's text across several
//! `w:instrText` elements in separate `w:r` runs (e.g. at formatting
//! boundaries, or after edits) -- `HYPERLINK "https://` in one run and
//! `example.com/x"` in the next is common in real documents. Classifying
//! each `w:instrText` element independently would miss this: neither
//! fragment alone matches a recognized pattern, so a naive per-element pass
//! would leave both untouched, silently leaking the literal URL text
//! byte-for-byte into the "sanitized" output while also emitting confusing
//! partial findings.
//!
//! [`sanitize_instr_text_elements`] avoids this with a two-pass approach:
//! [`plan_instr_text_actions`] walks the document tracking `w:fldChar
//! w:fldCharType="begin"/"end"` boundaries (`"separate"` doesn't affect
//! grouping -- per the OOXML schema `w:instrText` never appears after it),
//! concatenates every `w:instrText` between a `begin` and its matching
//! `end` into one logical instruction, and classifies that whole string. The decision
//! then fans back out to an ordered per-element action queue: a recognized
//! group writes its full replacement into the *first* `w:instrText` element
//! and clears the rest (this redistributes text across runs relative to
//! the source, but preserves paragraph/run/formatting structure --
//! DESIGN.md's shape guarantees don't cover exact text-to-run
//! distribution). An unrecognized group is left completely untouched and
//! produces exactly one [`crate::report::UnsupportedPayload`] for the
//! whole group, not one per fragment.
//!
//! Nested fields (a field instruction containing another field) are
//! conservatively treated as a whole-group `Unrecognized` rather than
//! classified recursively -- correctly attributing inner-field text to the
//! right logical instruction is real complexity for a construct that's
//! rare in practice, and failing closed is safe. A field instruction
//! that's never closed (`begin` with no matching `end` before
//! EOF -- a malformed document) is handled the same way.
//!
//! A `w:instrText` with no enclosing `w:fldChar begin`/`end` at all
//! (not valid per the OOXML schema, but not fatal to tolerate) degrades
//! gracefully to being classified as its own single-element group, which
//! is exactly the original per-element behavior for the common
//! non-fragmented case.
//!
//! This does not cover `w:fldSimple`, an alternate, single-attribute form
//! of field ("simple fields") where the whole instruction lives in a
//! `w:instr` XML attribute rather than `w:instrText` runs. That's a
//! different structural pattern, not a fragmentation of this one, and is a
//! separate known gap left for future work.

use std::collections::VecDeque;

use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};

use crate::Error;
use crate::report::UnsupportedPayload;
use crate::xml::rewrite::{WORDPROCESSINGML_NS, matches_namespace, resolve_general_ref, rewrite_text_elements};
use crate::xml::text::CANONICAL_HYPERLINK_TARGET;

/// Field keywords accepted as "recognized, no literal payload" as long as
/// the remainder of the instruction contains no quoted string (which would
/// indicate a switch argument we don't otherwise model, and might carry
/// user-controlled text).
const STRUCTURAL_FIELD_KEYWORDS: &[&str] = &[
    "PAGE",
    "NUMPAGES",
    "DATE",
    "TIME",
    "SAVEDATE",
    "PRINTDATE",
    "FILENAME",
    "AUTHOR",
    "SECTION",
    "SECTIONPAGES",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstrTextOutcome {
    /// The instruction matched a recognized pattern; this is its
    /// sanitized replacement (which may be identical to the input, for
    /// structural fields with no literal payload to redact).
    Recognized(String),
    /// The instruction did not match any recognized pattern and was left
    /// untouched; the caller must treat this as an unsupported payload
    /// surface, not as already-safe content.
    Unrecognized,
}

/// Classify and, if recognized, sanitize a `w:instrText` field instruction.
pub fn sanitize_instr_text(original: &str) -> InstrTextOutcome {
    if let Some((value_start, value_end)) = find_hyperlink_url_span(original) {
        let mut replaced = String::with_capacity(
            value_start + CANONICAL_HYPERLINK_TARGET.len() + (original.len() - value_end),
        );
        replaced.push_str(&original[..value_start]);
        replaced.push_str(CANONICAL_HYPERLINK_TARGET);
        replaced.push_str(&original[value_end..]);
        return InstrTextOutcome::Recognized(replaced);
    }

    if is_structural_field(original) {
        return InstrTextOutcome::Recognized(original.to_string());
    }

    InstrTextOutcome::Unrecognized
}

/// Extracts a safe-to-report keyword (the instruction's first
/// whitespace-delimited token, if any) from a field instruction, for use
/// in [`crate::report::UnsupportedPayload`] descriptions without embedding
/// the instruction's literal payload.
pub fn field_keyword(text: &str) -> Option<&str> {
    text.split_whitespace().next()
}

/// Sanitize every `w:instrText` element in `xml`, grouping fragments that
/// belong to the same field instruction (see the module docs) before
/// classifying. Returns the rewritten bytes plus one
/// [`UnsupportedPayload`] per unrecognized field-instruction group found in
/// `part_path`.
pub fn sanitize_instr_text_elements(xml: &[u8], part_path: &str) -> Result<(Vec<u8>, Vec<UnsupportedPayload>), Error> {
    let (mut actions, findings) = plan_instr_text_actions(xml, part_path)?;

    let out = rewrite_text_elements(xml, WORDPROCESSINGML_NS, b"instrText", move |text| {
        match actions.pop_front() {
            Some(InstrTextAction::ReplaceWhole(replacement)) => replacement,
            Some(InstrTextAction::Clear) => String::new(),
            Some(InstrTextAction::Unchanged) | None => text.to_string(),
        }
    })?;

    Ok((out, findings))
}

/// Per-`w:instrText`-occurrence action, in document order, produced by
/// [`plan_instr_text_actions`] and consumed by
/// [`sanitize_instr_text_elements`]'s rewrite pass.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstrTextAction {
    /// This occurrence is the first `w:instrText` in a recognized group;
    /// replace its entire content with this string.
    ReplaceWhole(String),
    /// This occurrence is a non-first `w:instrText` in a recognized group;
    /// its content is cleared (the whole group's replacement already went
    /// into the first element).
    Clear,
    /// This occurrence belongs to an unrecognized (or nested/malformed)
    /// group; leave its content exactly as it was.
    Unchanged,
}

struct FieldGroup {
    concatenated: String,
    element_count: usize,
    /// Forces an `Unrecognized` outcome regardless of `concatenated`
    /// (nested field, or a field left open at EOF) -- fail closed rather
    /// than attempt to classify a construct this model doesn't represent.
    force_unrecognized: bool,
}

/// Walks `xml` tracking `w:fldChar` field boundaries and `w:instrText`
/// occurrences, and produces an ordered action for every `w:instrText`
/// element in the document (in the same document order
/// [`rewrite_text_elements`] will visit them), plus the unsupported-payload
/// findings for any unrecognized group.
fn plan_instr_text_actions(xml: &[u8], part_path: &str) -> Result<(VecDeque<InstrTextAction>, Vec<UnsupportedPayload>), Error> {
    let mut reader = NsReader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut actions = VecDeque::new();
    let mut findings = Vec::new();

    let mut field_depth: u32 = 0;
    let mut group: Option<FieldGroup> = None;

    let mut instr_depth: usize = 0;
    let mut instr_pending = String::new();

    loop {
        match reader.read_resolved_event()? {
            (_, Event::Eof) => break,
            (resolved, Event::Empty(start)) | (resolved, Event::Start(start))
                if matches_namespace(&resolved, start.local_name().as_ref(), b"fldChar", WORDPROCESSINGML_NS) =>
            {
                match fld_char_type(&reader, &start).as_deref() {
                    Some("begin") => {
                        if field_depth == 0 {
                            group = Some(FieldGroup {
                                concatenated: String::new(),
                                element_count: 0,
                                force_unrecognized: false,
                            });
                        } else if let Some(g) = group.as_mut() {
                            // Nested field: don't attribute the inner
                            // field's text to the outer instruction.
                            g.force_unrecognized = true;
                        }
                        field_depth += 1;
                    }
                    Some("end") => {
                        field_depth = field_depth.saturating_sub(1);
                        if field_depth == 0 && let Some(g) = group.take() {
                            finalize_group(g, &mut actions, &mut findings, part_path);
                        }
                    }
                    // "separate" marks the end of the instruction phase
                    // (result content follows), but per the OOXML schema
                    // w:instrText never appears after it -- only "begin"
                    // and "end" bound a field's instrText-collecting span,
                    // so "separate" doesn't need to affect field_depth.
                    // (Treating it as a second closing marker here was a
                    // real bug: a well-formed field always emits both
                    // separate and end, and decrementing on both double-
                    // counts a single field's closure -- harmless for a
                    // flat field, since the depth reaches 0 once at
                    // separate and the second decrement at end is a no-op,
                    // but for a *nested* field it caused the inner field's
                    // "end" to erroneously close the still-open outer
                    // group early.)
                    _ => {}
                }
            }
            (resolved, Event::Empty(start))
                if matches_namespace(&resolved, start.local_name().as_ref(), b"instrText", WORDPROCESSINGML_NS) =>
            {
                record_occurrence(String::new(), &mut group, &mut actions, &mut findings, part_path);
            }
            (resolved, Event::Start(start))
                if matches_namespace(&resolved, start.local_name().as_ref(), b"instrText", WORDPROCESSINGML_NS) =>
            {
                instr_depth += 1;
                if instr_depth == 1 {
                    instr_pending.clear();
                }
            }
            (resolved, Event::End(end))
                if instr_depth == 1
                    && matches_namespace(&resolved, end.local_name().as_ref(), b"instrText", WORDPROCESSINGML_NS) =>
            {
                instr_depth -= 1;
                let text = std::mem::take(&mut instr_pending);
                record_occurrence(text, &mut group, &mut actions, &mut findings, part_path);
            }
            (resolved, Event::End(end))
                if instr_depth > 0
                    && matches_namespace(&resolved, end.local_name().as_ref(), b"instrText", WORDPROCESSINGML_NS) =>
            {
                instr_depth -= 1;
            }
            (_, Event::Text(text)) if instr_depth > 0 => {
                let decoded = text.decode().map_err(quick_xml::Error::from)?;
                instr_pending.push_str(&decoded);
            }
            (_, Event::GeneralRef(reference)) if instr_depth > 0 => {
                instr_pending.push(resolve_general_ref(&reference)?);
            }
            (_, Event::CData(cdata)) if instr_depth > 0 => {
                let decoded = cdata.decode().map_err(quick_xml::Error::from)?;
                instr_pending.push_str(&decoded);
            }
            _ => {}
        }
    }

    if let Some(mut g) = group.take() {
        // A field left open at EOF (no matching end) is
        // malformed; fail closed rather than guess.
        g.force_unrecognized = true;
        finalize_group(g, &mut actions, &mut findings, part_path);
    }

    Ok((actions, findings))
}

fn record_occurrence(
    text: String,
    group: &mut Option<FieldGroup>,
    actions: &mut VecDeque<InstrTextAction>,
    findings: &mut Vec<UnsupportedPayload>,
    part_path: &str,
) {
    match group {
        Some(g) => {
            g.concatenated.push_str(&text);
            g.element_count += 1;
        }
        None => {
            // No enclosing w:fldChar begin/end (not schema-valid, but
            // tolerated): degrade to a standalone single-element group,
            // matching the original non-fragmented behavior.
            let standalone = FieldGroup {
                concatenated: text,
                element_count: 1,
                force_unrecognized: false,
            };
            finalize_group(standalone, actions, findings, part_path);
        }
    }
}

fn finalize_group(
    group: FieldGroup,
    actions: &mut VecDeque<InstrTextAction>,
    findings: &mut Vec<UnsupportedPayload>,
    part_path: &str,
) {
    let outcome = if group.force_unrecognized {
        InstrTextOutcome::Unrecognized
    } else {
        sanitize_instr_text(&group.concatenated)
    };

    match outcome {
        InstrTextOutcome::Recognized(replaced) => {
            if group.element_count > 0 {
                actions.push_back(InstrTextAction::ReplaceWhole(replaced));
                for _ in 1..group.element_count {
                    actions.push_back(InstrTextAction::Clear);
                }
            }
        }
        InstrTextOutcome::Unrecognized => {
            let keyword = field_keyword(&group.concatenated).unwrap_or("(unknown)");
            findings.push(UnsupportedPayload {
                part: part_path.to_string(),
                description: format!("unrecognized w:instrText field instruction: {keyword}"),
            });
            for _ in 0..group.element_count {
                actions.push_back(InstrTextAction::Unchanged);
            }
        }
    }
}

fn fld_char_type(reader: &NsReader<&[u8]>, start: &BytesStart) -> Option<String> {
    for attribute in start.attributes().flatten() {
        let (resolved, local) = reader.resolve_attribute(attribute.key);
        if matches_namespace(&resolved, local.as_ref(), b"fldCharType", WORDPROCESSINGML_NS) {
            let decoded = String::from_utf8_lossy(&attribute.value);
            if let Ok(unescaped) = quick_xml::escape::unescape(&decoded) {
                return Some(unescaped.into_owned());
            }
        }
    }
    None
}

/// Recognizes exactly `HYPERLINK "<url>"` (optionally surrounded by
/// whitespace) and nothing else. Returns the byte range of `<url>` (the
/// quoted content, excluding the quotes) within `original`.
fn find_hyperlink_url_span(text: &str) -> Option<(usize, usize)> {
    let leading_ws = text.len() - text.trim_start().len();
    let rest = &text[leading_ws..];

    const KEYWORD: &str = "HYPERLINK";
    if rest.len() < KEYWORD.len() || !rest[..KEYWORD.len()].eq_ignore_ascii_case(KEYWORD) {
        return None;
    }
    let after_keyword = &rest[KEYWORD.len()..];

    let ws_len = after_keyword.len() - after_keyword.trim_start().len();
    if ws_len == 0 {
        // Keyword must be followed by whitespace, not run directly into
        // the quote (or anything else).
        return None;
    }
    let after_ws = &after_keyword[ws_len..];
    if !after_ws.starts_with('"') {
        return None;
    }

    let quote_open = leading_ws + KEYWORD.len() + ws_len;
    let inner = &after_ws[1..];
    let close_rel = inner.find('"')?;

    let value_start = quote_open + 1;
    let value_end = value_start + close_rel;

    let trailing = &text[value_end + 1..];
    if trailing.trim().is_empty() {
        Some((value_start, value_end))
    } else {
        // Anything after the closing quote (switches, another literal,
        // ...) pushes this out of the narrow recognized pattern.
        None
    }
}

fn is_structural_field(text: &str) -> bool {
    let trimmed = text.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let Some(keyword) = parts.next() else {
        return false;
    };
    if keyword.is_empty() {
        return false;
    }
    if !STRUCTURAL_FIELD_KEYWORDS.iter().any(|k| keyword.eq_ignore_ascii_case(k)) {
        return false;
    }
    let remainder = parts.next().unwrap_or("");
    !remainder.contains('"')
}

#[cfg(test)]
mod tests {
    use super::{InstrTextOutcome, field_keyword, sanitize_instr_text};

    #[test]
    fn recognizes_bare_hyperlink_field() {
        let outcome = sanitize_instr_text(r#" HYPERLINK "https://client.example.com/secret" "#);
        assert_eq!(
            outcome,
            InstrTextOutcome::Recognized(" HYPERLINK \"https://example.invalid/redacted\" ".to_string())
        );
    }

    #[test]
    fn recognizes_hyperlink_field_without_surrounding_whitespace() {
        let outcome = sanitize_instr_text(r#"HYPERLINK "https://a.example""#);
        assert_eq!(
            outcome,
            InstrTextOutcome::Recognized("HYPERLINK \"https://example.invalid/redacted\"".to_string())
        );
    }

    #[test]
    fn hyperlink_field_is_case_insensitive_on_keyword() {
        let outcome = sanitize_instr_text(r#" hyperlink "https://a.example" "#);
        assert_eq!(
            outcome,
            InstrTextOutcome::Recognized(" hyperlink \"https://example.invalid/redacted\" ".to_string())
        );
    }

    #[test]
    fn hyperlink_with_switches_is_unrecognized() {
        // \o carries an arbitrary quoted tooltip string we don't sanitize
        // here -- treating this as "recognized" would falsely imply the
        // whole field was made safe.
        let outcome = sanitize_instr_text(r#" HYPERLINK "https://a.example" \o "Click here" "#);
        assert_eq!(outcome, InstrTextOutcome::Unrecognized);
    }

    #[test]
    fn hyperlink_missing_closing_quote_is_unrecognized() {
        let outcome = sanitize_instr_text(r#" HYPERLINK "https://a.example"#);
        assert_eq!(outcome, InstrTextOutcome::Unrecognized);
    }

    #[test]
    fn hyperlink_without_quotes_is_unrecognized() {
        let outcome = sanitize_instr_text(" HYPERLINK ");
        assert_eq!(outcome, InstrTextOutcome::Unrecognized);
    }

    #[test]
    fn recognizes_structural_fields_with_no_literal_payload() {
        for field in ["PAGE", "NUMPAGES", "DATE", "TIME", "AUTHOR", "FILENAME"] {
            let input = format!(" {field} ");
            assert_eq!(
                sanitize_instr_text(&input),
                InstrTextOutcome::Recognized(input),
                "field {field} should be recognized as structural"
            );
        }
    }

    #[test]
    fn structural_field_is_case_insensitive() {
        let outcome = sanitize_instr_text(" page ");
        assert_eq!(outcome, InstrTextOutcome::Recognized(" page ".to_string()));
    }

    #[test]
    fn structural_field_with_a_quoted_switch_is_unrecognized() {
        // \@ carries a date-format pattern; conservatively treated the
        // same as any other quoted switch argument rather than assumed
        // safe.
        let outcome = sanitize_instr_text(r#" DATE \@ "MMMM yyyy" "#);
        assert_eq!(outcome, InstrTextOutcome::Unrecognized);
    }

    #[test]
    fn unknown_field_keyword_is_unrecognized() {
        assert_eq!(sanitize_instr_text(" MERGEFIELD ClientName "), InstrTextOutcome::Unrecognized);
        assert_eq!(sanitize_instr_text(" REF _Ref123456789 \\h "), InstrTextOutcome::Unrecognized);
        assert_eq!(sanitize_instr_text(" TOC \\o \"1-3\" \\h "), InstrTextOutcome::Unrecognized);
    }

    #[test]
    fn empty_instruction_is_unrecognized() {
        assert_eq!(sanitize_instr_text(""), InstrTextOutcome::Unrecognized);
        assert_eq!(sanitize_instr_text("   "), InstrTextOutcome::Unrecognized);
    }

    #[test]
    fn field_keyword_extracts_first_token_without_payload() {
        assert_eq!(field_keyword(" MERGEFIELD ClientName "), Some("MERGEFIELD"));
        assert_eq!(field_keyword("   "), None);
        assert_eq!(field_keyword(""), None);
    }

    mod field_grouping {
        use super::super::sanitize_instr_text_elements;

        const XMLNS_W: &str =
            r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

        fn sanitize(body: &str) -> (String, Vec<String>) {
            let wrapped = format!(r#"<w:root {XMLNS_W}>{body}</w:root>"#);
            let (bytes, findings) = sanitize_instr_text_elements(wrapped.as_bytes(), "word/document.xml").unwrap();
            let out = String::from_utf8(bytes).unwrap();
            let inner = out
                .strip_prefix(&format!(r#"<w:root {XMLNS_W}>"#))
                .unwrap()
                .strip_suffix("</w:root>")
                .unwrap()
                .to_string();
            (inner, findings.into_iter().map(|f| f.description).collect())
        }

        fn field(instr_runs: &[&str]) -> String {
            let mut xml = String::from(r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r>"#);
            for instr in instr_runs {
                xml.push_str(&format!(
                    r#"<w:r><w:instrText xml:space="preserve">{instr}</w:instrText></w:r>"#
                ));
            }
            xml.push_str(r#"<w:r><w:fldChar w:fldCharType="separate"/></w:r>"#);
            xml.push_str(r#"<w:r><w:fldChar w:fldCharType="end"/></w:r>"#);
            xml
        }

        #[test]
        fn rewrites_hyperlink_split_across_two_instr_text_runs() {
            // The URL is fragmented mid-string across two w:instrText
            // elements, as real Word documents commonly do.
            let input = field(&[" HYPERLINK \"https://", "internal.example.com/secret\" "]);
            let (out, findings) = sanitize(&input);

            assert!(findings.is_empty());
            assert!(!out.contains("internal.example.com"));
            assert!(!out.contains("secret"));
            assert_eq!(out.matches("<w:instrText").count(), 2, "both original elements must survive");
            // Full replacement lands in the first element; the second is
            // cleared, not merged or dropped.
            assert!(out.contains("https://example.invalid/redacted"));
            assert!(out.contains(r#"<w:instrText xml:space="preserve"></w:instrText>"#));
        }

        #[test]
        fn rewrites_hyperlink_split_across_three_instr_text_runs() {
            let input = field(&[" HYPER", "LINK \"https://a.exa", "mple/x\" "]);
            let (out, findings) = sanitize(&input);

            assert!(findings.is_empty());
            assert!(!out.contains("a.example"));
            let instr_text_count = out.matches("<w:instrText").count();
            assert_eq!(instr_text_count, 3, "all three original elements must still be present");
            assert!(out.contains("https://example.invalid/redacted"));
        }

        #[test]
        fn flags_unrecognized_split_field_once_not_per_fragment() {
            let input = field(&[" MERGE", "FIELD ClientName "]);
            let (out, findings) = sanitize(&input);

            assert_eq!(findings.len(), 1, "one finding for the whole group, not one per fragment");
            assert!(findings[0].contains("MERGEFIELD"));
            // Left completely untouched.
            assert!(out.contains(" MERGE"));
            assert!(out.contains("FIELD ClientName "));
        }

        #[test]
        fn single_element_field_behaves_as_before() {
            let input = field(&[" PAGE "]);
            let (out, findings) = sanitize(&input);

            assert!(findings.is_empty());
            assert!(out.contains(r#"<w:instrText xml:space="preserve"> PAGE </w:instrText>"#));
        }

        #[test]
        fn instr_text_without_enclosing_fld_char_is_classified_standalone() {
            // Not schema-valid, but tolerated: degrades to per-element
            // behavior instead of erroring or treating everything as one
            // giant group.
            let input =
                r#"<w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r><w:r><w:instrText xml:space="preserve"> MERGEFIELD X </w:instrText></w:r>"#;
            let (out, findings) = sanitize(input);

            assert_eq!(findings.len(), 1);
            assert!(findings[0].contains("MERGEFIELD"));
            assert!(out.contains(r#"<w:instrText xml:space="preserve"> PAGE </w:instrText>"#));
            assert!(out.contains(r#"<w:instrText xml:space="preserve"> MERGEFIELD X </w:instrText>"#));
        }

        #[test]
        fn nested_field_is_flagged_unrecognized_as_a_whole() {
            let input = format!(
                r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r>
                   <w:r><w:instrText xml:space="preserve"> IF </w:instrText></w:r>
                   {}
                   <w:r><w:instrText xml:space="preserve"> = 1 "yes" "no" </w:instrText></w:r>
                   <w:r><w:fldChar w:fldCharType="separate"/></w:r>
                   <w:r><w:fldChar w:fldCharType="end"/></w:r>"#,
                field(&[" REF x "])
            );
            let (_out, findings) = sanitize(&input);

            assert_eq!(findings.len(), 1);
        }

        #[test]
        fn unclosed_field_at_eof_fails_closed() {
            let input = r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> HYPERLINK "https://a.example" </w:instrText></w:r>"#;
            let (out, findings) = sanitize(input);

            assert_eq!(findings.len(), 1);
            assert!(out.contains("a.example"), "left untouched, not guessed at");
        }
    }
}
