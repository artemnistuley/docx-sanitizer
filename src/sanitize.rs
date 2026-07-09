//! Top-level sanitization orchestration.
//!
//! Step 5/6 scope so far: `word/document.xml`, `word/header*.xml`,
//! `word/footer*.xml`, `word/footnotes.xml`, `word/endnotes.xml`, and
//! `word/comments.xml` have their `w:t` (visible text) and `w:delText`
//! (tracked-deletion text) rewritten per the caller's chosen
//! [`ReplacementMode`] (`preserve-length` by default; Step 9), their revision/
//! comment metadata attributes (`w:author`, `w:initials`: always preserve-length;
//! `w:date`: fixed canonical timestamp) rewritten wherever they appear
//! (`w:ins`, `w:del`, `w:comment`, `w:rPrChange`, ...), and their
//! `w:instrText` field instructions conservatively rewritten for a narrow
//! recognized set (see [`crate::xml::instr_text`]) -- unrecognized field
//! instructions are left untouched but recorded in
//! [`SanitizeOutput::unsupported`]. `docProps/core.xml`, `docProps/app.xml`,
//! and `docProps/custom.xml` have their sensitive property values (see
//! [`crate::xml::props`]) replaced with a fixed canonical placeholder or
//! timestamp. Every other part is passed through byte-for-byte via
//! [`crate::zip::repack_docx`].
//!
//! Step 7 adds policy enforcement on top of that: [`sanitize`] combines
//! [`SanitizeOutput::unsupported`] with any unsupported *part classes*
//! present (`word/customXml/`, `word/media/`, `word/embeddings/`, per
//! [`crate::part::SupportTier::Unsupported`]) into one list of concerns
//! (see [`crate::policy`]), and -- in the default strict mode -- refuses to
//! return output at all if that list is non-empty.
//!
//! Step 10 adds one best-effort/optional extra (DESIGN.md): external
//! hyperlink targets in every `.rels` part found in the package (see
//! [`crate::xml::rels`]) are rewritten unconditionally -- not gated by
//! `scope` or counted toward strict-mode concerns, since this is additive
//! cleanup on top of the guaranteed contract, not part of it.

use std::collections::HashMap;

use crate::Error;
use crate::part::inspect_parts;
use crate::policy::{SanitizeMode, Scope, ScopeCategory, collect_concerns, should_block};
use crate::relationships::Relationships;
use crate::report::{Report, UnsupportedPayload, build_report};
use crate::xml::instr_text::sanitize_instr_text_elements;
use crate::xml::media::placeholder_bytes_for;
use crate::xml::props::{sanitize_app_props_xml, sanitize_core_props_xml, sanitize_custom_props_xml};
use crate::xml::rels::sanitize_hyperlink_targets;
use crate::xml::rewrite::{WORDPROCESSINGML_NS, rewrite_attribute_values, rewrite_text_elements};
use crate::xml::text::{CANONICAL_TIMESTAMP, ReplacementMode, preserve_length};
use crate::xml::track_changes::remove_track_changes as strip_track_changes;
use crate::zip::{FileRegistry, MAIN_DOCUMENT_PART, get_part, repack_docx, require_part};

const CORE_PROPS_PATH: &str = "docProps/core.xml";
const APP_PROPS_PATH: &str = "docProps/app.xml";
const CUSTOM_PROPS_PATH: &str = "docProps/custom.xml";

/// Result of a sanitization run: the repacked `.docx` bytes plus any
/// payload surfaces that could not be confidently sanitized. This is the
/// raw output of the rewrite passes, before [`sanitize`] applies strict/
/// best-effort policy on top -- `bytes` here is always produced regardless
/// of whether `unsupported` is non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeOutput {
    pub bytes: Vec<u8>,
    pub unsupported: Vec<UnsupportedPayload>,
}

/// Outcome of [`sanitize`]: either the sanitized output, or a refusal to
/// produce output (strict mode, with concerns present).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SanitizeResult {
    Produced(SanitizeOutput),
    Blocked { concerns: Vec<UnsupportedPayload> },
}

/// Sanitize `files` and apply strict/best-effort policy: combines
/// unsupported part classes (`word/customXml/`, `word/media/`,
/// `word/embeddings/`) with unsupported payload surfaces found while
/// sanitizing (e.g. unrecognized `w:instrText`) into one list of concerns.
/// In [`SanitizeMode::Strict`] (the default), any concern blocks output
/// entirely; in [`SanitizeMode::BestEffort`], output is always produced and
/// concerns are only informational.
pub fn sanitize(
    files: &FileRegistry,
    mode: SanitizeMode,
    scope: &Scope,
    replacement_mode: ReplacementMode,
    remove_track_changes: bool,
    strip_media: bool,
) -> Result<SanitizeResult, Error> {
    let output = sanitize_document_text(files, scope, replacement_mode, remove_track_changes, strip_media)?;
    let parts = inspect_parts(files)?;
    let concerns = collect_concerns(&parts, &output.unsupported, strip_media);

    if should_block(mode, &concerns) {
        Ok(SanitizeResult::Blocked { concerns })
    } else {
        Ok(SanitizeResult::Produced(output))
    }
}

/// Build a [`Report`] for `files` without writing any output: a dry run of
/// the same classification and sanitize pass `sanitize` performs, used by
/// the `report` command and by `sanitize --report-json`.
///
/// Note: this repeats `sanitize`'s work (rewriting XML, repacking) rather
/// than sharing it, since `sanitize`'s `SanitizeResult` doesn't currently
/// carry the classified parts this needs. Calling both `sanitize` and
/// `report` for the same input does the sanitize pass twice; simplicity
/// over avoiding that duplication for v1.
pub fn report(
    files: &FileRegistry,
    mode: SanitizeMode,
    scope: &Scope,
    replacement_mode: ReplacementMode,
    remove_track_changes: bool,
    strip_media: bool,
) -> Result<Report, Error> {
    let output = sanitize_document_text(files, scope, replacement_mode, remove_track_changes, strip_media)?;
    let parts = inspect_parts(files)?;
    let concerns = collect_concerns(&parts, &output.unsupported, strip_media);
    Ok(build_report(mode, scope, &parts, &concerns, strip_media))
}

/// Sanitize the visible text and revision/comment metadata of
/// `word/document.xml`, headers, footers, footnotes, endnotes, and comments:
/// body/revision text (`w:t`/`w:delText`) uses `replacement_mode`; author and
/// initials always use preserve-length; dates always use a fixed canonical
/// timestamp (see [`sanitize_body_text_xml`]'s docs for why those two are
/// not configurable). Every other part is left byte-for-byte unchanged.
/// `word/document.xml` itself is always sanitized; `scope` controls whether
/// the other guaranteed parts are sanitized (left untouched if excluded) and
/// whether revision/comment metadata attributes are rewritten.
pub fn sanitize_document_text(
    files: &FileRegistry,
    scope: &Scope,
    replacement_mode: ReplacementMode,
    remove_track_changes: bool,
    strip_media: bool,
) -> Result<SanitizeOutput, Error> {
    let relationships = Relationships::from_files(files)?;
    let main_document_path = relationships
        .main_document_path()
        .unwrap_or(MAIN_DOCUMENT_PART)
        .to_string();

    let mut overrides = HashMap::new();
    let mut unsupported = Vec::new();

    let document_xml = require_part(files, &main_document_path)?;
    let (bytes, findings) =
        sanitize_body_text_xml(&main_document_path, document_xml, scope, replacement_mode, remove_track_changes)?;
    overrides.insert(main_document_path, bytes);
    unsupported.extend(findings);

    if scope.contains(ScopeCategory::Headers) {
        for header_path in relationships.find_header_parts() {
            let xml = require_part(files, header_path)?;
            let (bytes, findings) = sanitize_body_text_xml(header_path, xml, scope, replacement_mode, remove_track_changes)?;
            overrides.insert(header_path.to_string(), bytes);
            unsupported.extend(findings);
        }
    }

    if scope.contains(ScopeCategory::Footers) {
        for footer_path in relationships.find_footer_parts() {
            let xml = require_part(files, footer_path)?;
            let (bytes, findings) = sanitize_body_text_xml(footer_path, xml, scope, replacement_mode, remove_track_changes)?;
            overrides.insert(footer_path.to_string(), bytes);
            unsupported.extend(findings);
        }
    }

    if scope.contains(ScopeCategory::Footnotes)
        && let Some(footnotes_path) = relationships.find_footnotes_part() {
            let xml = require_part(files, footnotes_path)?;
            let (bytes, findings) = sanitize_body_text_xml(footnotes_path, xml, scope, replacement_mode, remove_track_changes)?;
            overrides.insert(footnotes_path.to_string(), bytes);
            unsupported.extend(findings);
        }

    if scope.contains(ScopeCategory::Endnotes)
        && let Some(endnotes_path) = relationships.find_endnotes_part() {
            let xml = require_part(files, endnotes_path)?;
            let (bytes, findings) = sanitize_body_text_xml(endnotes_path, xml, scope, replacement_mode, remove_track_changes)?;
            overrides.insert(endnotes_path.to_string(), bytes);
            unsupported.extend(findings);
        }

    if scope.contains(ScopeCategory::Comments)
        && let Some(comments_path) = relationships.find_comments_part() {
            let xml = require_part(files, comments_path)?;
            let (bytes, findings) = sanitize_body_text_xml(comments_path, xml, scope, replacement_mode, remove_track_changes)?;
            overrides.insert(comments_path.to_string(), bytes);
            unsupported.extend(findings);
        }

    if scope.contains(ScopeCategory::DocProps) {
        if let Some(xml) = get_part(files, CORE_PROPS_PATH) {
            overrides.insert(CORE_PROPS_PATH.to_string(), sanitize_core_props_xml(xml)?);
        }

        if let Some(xml) = get_part(files, APP_PROPS_PATH) {
            overrides.insert(APP_PROPS_PATH.to_string(), sanitize_app_props_xml(xml)?);
        }

        if let Some(xml) = get_part(files, CUSTOM_PROPS_PATH) {
            overrides.insert(CUSTOM_PROPS_PATH.to_string(), sanitize_custom_props_xml(xml)?);
        }
    }

    // Best-effort/optional (DESIGN.md): external hyperlink targets in any
    // `.rels` part, unconditional on `scope` -- this isn't part of the
    // guaranteed --include/--exclude vocabulary or the strict-mode
    // pass/fail contract, just additive cleanup.
    for (path, xml) in files {
        if path.ends_with(".rels") {
            overrides.insert(path.clone(), sanitize_hyperlink_targets(xml)?);
        }
    }

    // `--strip-media`: replace `word/media/*` bytes with a fixed placeholder
    // image for supported extensions. The part stays at the same path with
    // the same relationship/content-type entry -- only its payload changes
    // -- so no `.rels`/`[Content_Types].xml` cleanup is needed (see
    // DESIGN.md's "Images and Embeddings" for why this is placeholder
    // replacement, not part removal).
    if strip_media {
        for path in files.keys() {
            if path.starts_with("word/media/")
                && let Some(placeholder) = placeholder_bytes_for(path)
            {
                overrides.insert(path.clone(), placeholder.to_vec());
            }
        }
    }

    let bytes = repack_docx(files, &overrides)?;
    Ok(SanitizeOutput { bytes, unsupported })
}

/// Rewrite `w:t`/`w:delText` payload, `w:author`/`w:initials`/`w:date`
/// revision metadata, and `w:instrText` field instructions in a body-story
/// XML part (`word/document.xml`, headers/footers/footnotes/endnotes, and
/// `word/comments.xml`, which all share the same body content model and
/// revision-metadata attributes). Returns the rewritten bytes plus any
/// unrecognized `w:instrText` field instructions found in this part.
///
/// `w:author`/`w:initials` use `preserve_length`, the same shape-masking
/// function as visible text, rather than an identity-preserving hash
/// pseudonym (e.g. `Person_XXXX`) -- a known, explicitly chosen v1
/// simplification. DESIGN.md's Recommended Defaults call out author fields
/// as "deterministic pseudonyms" separately from body text's
/// "preserve-length", and unlike a hash pseudonym, preserve-length is not
/// injective: two different authors whose names are the same length (e.g.
/// "Alice" and "Grace") collapse to the same placeholder, so author
/// distinctness within a sanitized document's tracked changes/comments is
/// not guaranteed to survive sanitization. Revisit with a hash-based
/// pseudonym scheme if that distinctness turns out to matter in practice.
///
/// If `remove_track_changes` is set, tracked changes are collapsed to their
/// accepted state (see [`crate::xml::track_changes`]) *before* any of the
/// above -- deleted text is gone by the time payload rewriting runs, so it
/// never needs sanitizing in the first place.
fn sanitize_body_text_xml(
    part_path: &str,
    xml: &[u8],
    scope: &Scope,
    replacement_mode: ReplacementMode,
    remove_track_changes: bool,
) -> Result<(Vec<u8>, Vec<UnsupportedPayload>), Error> {
    let xml = if remove_track_changes {
        strip_track_changes(xml)?
    } else {
        xml.to_vec()
    };

    let xml = rewrite_text_elements(&xml, WORDPROCESSINGML_NS, b"t", move |text| replacement_mode.apply(text))?;
    let xml = rewrite_text_elements(&xml, WORDPROCESSINGML_NS, b"delText", move |text| {
        replacement_mode.apply(text)
    })?;

    let xml = if scope.contains(ScopeCategory::Revisions) {
        let xml = rewrite_attribute_values(&xml, WORDPROCESSINGML_NS, b"author", preserve_length)?;
        let xml = rewrite_attribute_values(&xml, WORDPROCESSINGML_NS, b"initials", preserve_length)?;
        rewrite_attribute_values(&xml, WORDPROCESSINGML_NS, b"date", |_| CANONICAL_TIMESTAMP.to_string())?
    } else {
        xml
    };

    sanitize_instr_text_elements(&xml, part_path)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use quick_xml::Reader;
    use quick_xml::events::Event;
    use zip::write::SimpleFileOptions;

    use super::{SanitizeResult, sanitize, sanitize_document_text};
    use crate::policy::{SanitizeMode, Scope};
    use crate::xml::text::ReplacementMode;
    use crate::zip::{get_part, unpack_docx};

    fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = SimpleFileOptions::default();

        for (path, contents) in files {
            writer.start_file(*path, options).unwrap();
            writer.write_all(contents).unwrap();
        }

        writer.finish().unwrap();
        buffer.into_inner()
    }

    const PACKAGE_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
        <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
          <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
        </Relationships>"#;

    fn assert_well_formed(xml: &[u8]) {
        let mut reader = Reader::from_reader(xml);
        let mut buf = Vec::new();
        loop {
            if reader.read_event_into(&mut buf).unwrap() == Event::Eof {
                break;
            }
            buf.clear();
        }
    }

    #[test]
    fn sanitizes_document_text_and_preserves_other_parts() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:t xml:space="preserve">Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>
              </w:body>
            </w:document>"#;
        let core_props: &[u8] = br#"<cp:coreProperties/>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
            ("docProps/core.xml", core_props),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();

        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        assert_well_formed(sanitized_document);
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();
        assert!(sanitized_str.contains(r#"<w:t xml:space="preserve">Xxxxx </w:t>"#));
        assert!(sanitized_str.contains(r#"<w:t>xxxxx</w:t>"#));
        assert!(!sanitized_str.contains("Hello"));
        assert!(!sanitized_str.contains("world"));

        assert_eq!(
            get_part(&sanitized_files, "docProps/core.xml").unwrap(),
            core_props
        );
    }

    #[test]
    fn sanitizes_tracked_deletion_text_alongside_visible_text() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p>
                  <w:del w:id="1" w:author="Alice" w:date="2026-05-29T10:00:00Z">
                    <w:r><w:delText xml:space="preserve">Secret Old</w:delText></w:r>
                  </w:del>
                  <w:ins w:id="2" w:author="Alice" w:date="2026-05-29T10:00:00Z">
                    <w:r><w:t>New</w:t></w:r>
                  </w:ins>
                </w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();

        assert_well_formed(sanitized_document);
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();
        assert!(sanitized_str.contains(r#"<w:delText xml:space="preserve">Xxxxxx Xxx</w:delText>"#));
        assert!(sanitized_str.contains(r#"<w:t>Xxx</w:t>"#));
        assert!(!sanitized_str.contains("Secret"));
        assert!(!sanitized_str.contains("Old"));
        // Revision containers are preserved; author/date metadata is
        // rewritten (preserve-length author, canonical timestamp date).
        assert!(!sanitized_str.contains("Alice"));
        assert!(sanitized_str.contains(r#"w:author="Xxxxx""#));
        assert!(sanitized_str.contains(r#"w:date="2000-01-01T00:00:00Z""#));
        assert!(sanitized_str.contains(r#"<w:del w:id="1""#));
    }

    #[test]
    fn preserves_self_closing_empty_del_text() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:del><w:r><w:delText/></w:r></w:del></w:p></w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();

        assert_well_formed(sanitized_document);
        assert!(
            std::str::from_utf8(sanitized_document)
                .unwrap()
                .contains("<w:delText/>")
        );
    }

    #[test]
    fn sanitizes_headers_and_footers() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let header_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:p><w:r><w:t>Acme Corp Confidential</w:t></w:r></w:p>
            </w:hdr>"#;
        let footer_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:ftr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:p><w:r><w:t>Page secrets</w:t></w:r></w:p>
            </w:ftr>"#;
        let document_rels: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
              <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
            </Relationships>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", document_xml),
            ("word/header1.xml", header_xml),
            ("word/footer1.xml", footer_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();

        let sanitized_header = get_part(&sanitized_files, "word/header1.xml").unwrap();
        assert_well_formed(sanitized_header);
        let sanitized_header_str = std::str::from_utf8(sanitized_header).unwrap();
        assert!(!sanitized_header_str.contains("Acme"));
        assert!(sanitized_header_str.contains(r#"<w:t>Xxxx Xxxx Xxxxxxxxxxxx</w:t>"#));

        let sanitized_footer = get_part(&sanitized_files, "word/footer1.xml").unwrap();
        assert_well_formed(sanitized_footer);
        let sanitized_footer_str = std::str::from_utf8(sanitized_footer).unwrap();
        assert!(!sanitized_footer_str.contains("secrets"));
        assert!(sanitized_footer_str.contains(r#"<w:t>Xxxx xxxxxxx</w:t>"#));
    }

    #[test]
    fn sanitizes_footnotes_and_endnotes() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let footnotes_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:footnote w:id="1"><w:p><w:r><w:t>Client budget leak</w:t></w:r></w:p></w:footnote>
            </w:footnotes>"#;
        let endnotes_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:endnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:endnote w:id="1"><w:p><w:r><w:t>Internal source</w:t></w:r></w:p></w:endnote>
            </w:endnotes>"#;
        let document_rels: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>
              <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes" Target="endnotes.xml"/>
            </Relationships>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", document_xml),
            ("word/footnotes.xml", footnotes_xml),
            ("word/endnotes.xml", endnotes_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();

        let sanitized_footnotes = get_part(&sanitized_files, "word/footnotes.xml").unwrap();
        assert_well_formed(sanitized_footnotes);
        let sanitized_footnotes_str = std::str::from_utf8(sanitized_footnotes).unwrap();
        assert!(!sanitized_footnotes_str.contains("budget"));
        assert!(sanitized_footnotes_str.contains(r#"<w:t>Xxxxxx xxxxxx xxxx</w:t>"#));

        let sanitized_endnotes = get_part(&sanitized_files, "word/endnotes.xml").unwrap();
        assert_well_formed(sanitized_endnotes);
        let sanitized_endnotes_str = std::str::from_utf8(sanitized_endnotes).unwrap();
        assert!(!sanitized_endnotes_str.contains("Internal"));
        assert!(sanitized_endnotes_str.contains(r#"<w:t>Xxxxxxxx xxxxxx</w:t>"#));
    }

    #[test]
    fn sanitizes_comments() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let comments_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:comment w:id="1" w:author="Reviewer">
                <w:p><w:r><w:t>Reject this pricing</w:t></w:r></w:p>
              </w:comment>
            </w:comments>"#;
        let document_rels: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
            </Relationships>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", document_xml),
            ("word/comments.xml", comments_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();

        let sanitized_comments = get_part(&sanitized_files, "word/comments.xml").unwrap();
        assert_well_formed(sanitized_comments);
        let sanitized_comments_str = std::str::from_utf8(sanitized_comments).unwrap();
        assert!(!sanitized_comments_str.contains("Reject"));
        assert!(!sanitized_comments_str.contains("pricing"));
        assert!(sanitized_comments_str.contains(r#"<w:t>Xxxxxx xxxx xxxxxxx</w:t>"#));
        assert!(!sanitized_comments_str.contains("Reviewer"));
        assert!(sanitized_comments_str.contains(r#"w:author="Xxxxxxxx""#));
    }

    #[test]
    fn sanitizes_comment_initials_alongside_author_and_date() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let comments_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:comment w:id="1" w:author="Reviewer" w:initials="RV" w:date="2026-05-29T10:00:00Z">
                <w:p><w:r><w:t>Note</w:t></w:r></w:p>
              </w:comment>
            </w:comments>"#;
        let document_rels: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
            </Relationships>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", document_xml),
            ("word/comments.xml", comments_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_comments = get_part(&sanitized_files, "word/comments.xml").unwrap();

        assert_well_formed(sanitized_comments);
        let sanitized_str = std::str::from_utf8(sanitized_comments).unwrap();
        assert!(!sanitized_str.contains("RV"));
        assert!(sanitized_str.contains(r#"w:initials="XX""#));
        assert!(sanitized_str.contains(r#"w:date="2000-01-01T00:00:00Z""#));
    }

    #[test]
    fn sanitizes_doc_props_alongside_body_text() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let core_props: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <cp:coreProperties
                xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                xmlns:dc="http://purl.org/dc/elements/1.1/">
              <dc:creator>Jane Doe</dc:creator>
            </cp:coreProperties>"#;
        let app_props: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties">
              <Company>Acme Corp</Company>
            </Properties>"#;
        let custom_props: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties"
                xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">
              <property fmtid="{D5CDD505-2E9C-101B-9397-08002B2CF9AE}" pid="2" name="Client">
                <vt:lpwstr>Acme</vt:lpwstr>
              </property>
            </Properties>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
            ("docProps/core.xml", core_props),
            ("docProps/app.xml", app_props),
            ("docProps/custom.xml", custom_props),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();

        let sanitized_core = get_part(&sanitized_files, "docProps/core.xml").unwrap();
        assert_well_formed(sanitized_core);
        assert!(!std::str::from_utf8(sanitized_core).unwrap().contains("Jane Doe"));

        let sanitized_app = get_part(&sanitized_files, "docProps/app.xml").unwrap();
        assert_well_formed(sanitized_app);
        assert!(!std::str::from_utf8(sanitized_app).unwrap().contains("Acme Corp"));

        let sanitized_custom = get_part(&sanitized_files, "docProps/custom.xml").unwrap();
        assert_well_formed(sanitized_custom);
        let sanitized_custom_str = std::str::from_utf8(sanitized_custom).unwrap();
        assert!(!sanitized_custom_str.contains(">Acme<"));
        assert!(sanitized_custom_str.contains(r#"name="Client""#));
    }

    #[test]
    fn sanitizes_recognized_hyperlink_field_instruction() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r>
                  <w:r><w:instrText xml:space="preserve"> HYPERLINK "https://internal.example.com/secret-plan" </w:instrText></w:r>
                  <w:r><w:fldChar w:fldCharType="end"/></w:r>
                </w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        assert!(output.unsupported.is_empty());

        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();

        assert_well_formed(sanitized_document);
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();
        assert!(!sanitized_str.contains("internal.example.com"));
        assert!(!sanitized_str.contains("secret-plan"));
        // quick-xml's re-escaping of the replaced text writes `"` as the
        // `&quot;` entity in text content (valid XML, though not strictly
        // required there, since quick-xml's `escape()` is shared with
        // attribute-value escaping).
        assert!(sanitized_str.contains(
            r#"<w:instrText xml:space="preserve"> HYPERLINK &quot;https://example.invalid/redacted&quot; </w:instrText>"#
        ));
    }

    #[test]
    fn flags_unrecognized_field_instruction_without_stripping_it() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:instrText xml:space="preserve"> MERGEFIELD ClientName </w:instrText></w:r></w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();

        assert_eq!(output.unsupported.len(), 1);
        assert_eq!(output.unsupported[0].part, "word/document.xml");
        assert!(output.unsupported[0].description.contains("MERGEFIELD"));
        // Not silently "sanitized" as an empty/placeholder value -- the
        // original is left in place until Step 7 decides strict-mode
        // pass/fail for it.
        assert!(!output.unsupported[0].description.contains("ClientName"));

        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        assert!(std::str::from_utf8(sanitized_document).unwrap().contains("MERGEFIELD ClientName"));
    }

    #[test]
    fn recognizes_structural_field_instructions_as_no_op() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r></w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        assert!(output.unsupported.is_empty());

        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        assert!(
            std::str::from_utf8(sanitized_document)
                .unwrap()
                .contains(r#"<w:instrText xml:space="preserve"> PAGE </w:instrText>"#)
        );
    }

    #[test]
    fn preserves_self_closing_empty_runs() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t/></w:r></w:p></w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();

        assert_well_formed(sanitized_document);
        assert!(std::str::from_utf8(sanitized_document).unwrap().contains("<w:t/>"));
    }

    #[test]
    fn strict_mode_blocks_output_when_custom_xml_is_present() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Secret</w:t></w:r></w:p></w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
            ("customXml/item1.xml", b"<root/>"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let result = sanitize(&files, SanitizeMode::Strict, &Scope::all(), ReplacementMode::default(), false, false).unwrap();

        match result {
            SanitizeResult::Blocked { concerns } => {
                assert_eq!(concerns.len(), 1);
                assert_eq!(concerns[0].part, "customXml/item1.xml");
            }
            SanitizeResult::Produced(_) => panic!("expected strict mode to block output"),
        }
    }

    #[test]
    fn narrow_include_scope_still_blocks_strict_mode_on_custom_xml() {
        // Unsupported part classes are a separate axis from `--include`/
        // `--exclude`, which only toggles guaranteed-scope categories (see
        // policy.rs's module docs) -- there is no `customxml` scope
        // keyword, so a narrow --include must not silently suppress this
        // check.
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Secret</w:t></w:r></w:p></w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
            ("customXml/item1.xml", b"<root/>"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let scope = Scope::parse_include("comments").unwrap();
        let result = sanitize(&files, SanitizeMode::Strict, &scope, ReplacementMode::default(), false, false).unwrap();

        match result {
            SanitizeResult::Blocked { concerns } => {
                assert_eq!(concerns.len(), 1);
                assert_eq!(concerns[0].part, "customXml/item1.xml");
            }
            SanitizeResult::Produced(_) => panic!("expected strict mode to block output regardless of scope"),
        }
    }

    #[test]
    fn best_effort_mode_produces_output_when_custom_xml_is_present() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Secret</w:t></w:r></w:p></w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
            ("customXml/item1.xml", b"<root/>"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let result = sanitize(&files, SanitizeMode::BestEffort, &Scope::all(), ReplacementMode::default(), false, false).unwrap();

        match result {
            SanitizeResult::Produced(output) => {
                let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
                let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
                assert!(!std::str::from_utf8(sanitized_document).unwrap().contains("Secret"));
            }
            SanitizeResult::Blocked { .. } => panic!("expected best-effort mode to produce output"),
        }
    }

    #[test]
    fn strict_mode_blocks_output_on_unrecognized_instr_text() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:instrText xml:space="preserve"> MERGEFIELD ClientName </w:instrText></w:r></w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let result = sanitize(&files, SanitizeMode::Strict, &Scope::all(), ReplacementMode::default(), false, false).unwrap();

        match result {
            SanitizeResult::Blocked { concerns } => {
                assert_eq!(concerns.len(), 1);
                assert!(concerns[0].description.contains("MERGEFIELD"));
            }
            SanitizeResult::Produced(_) => panic!("expected strict mode to block output"),
        }
    }

    #[test]
    fn strict_mode_produces_output_when_everything_is_supported() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p></w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let result = sanitize(&files, SanitizeMode::Strict, &Scope::all(), ReplacementMode::default(), false, false).unwrap();

        assert!(matches!(result, SanitizeResult::Produced(_)));
    }

    #[test]
    fn excluding_headers_leaves_the_header_part_untouched() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let header_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:p><w:r><w:t>Acme Corp Confidential</w:t></w:r></w:p>
            </w:hdr>"#;
        let document_rels: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
            </Relationships>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", document_xml),
            ("word/header1.xml", header_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let scope = Scope::parse_exclude("headers").unwrap();
        let output = sanitize_document_text(&files, &scope, ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();

        // Header untouched (excluded)...
        assert_eq!(
            get_part(&sanitized_files, "word/header1.xml").unwrap(),
            header_xml
        );
        // ...but body text still sanitized.
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        assert!(!std::str::from_utf8(sanitized_document).unwrap().contains("Body"));
    }

    #[test]
    fn include_scope_only_sanitizes_the_listed_categories() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let comments_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:comment w:id="1" w:author="Reviewer"><w:p><w:r><w:t>Note</w:t></w:r></w:p></w:comment>
            </w:comments>"#;
        let core_props: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                xmlns:dc="http://purl.org/dc/elements/1.1/">
              <dc:creator>Jane Doe</dc:creator>
            </cp:coreProperties>"#;
        let document_rels: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
            </Relationships>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", document_xml),
            ("word/comments.xml", comments_xml),
            ("docProps/core.xml", core_props),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        // Only comments in scope -- docprops must stay untouched even
        // though its part is present.
        let scope = Scope::parse_include("comments").unwrap();
        let output = sanitize_document_text(&files, &scope, ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();

        let sanitized_comments = get_part(&sanitized_files, "word/comments.xml").unwrap();
        assert!(!std::str::from_utf8(sanitized_comments).unwrap().contains("Note"));

        assert_eq!(
            get_part(&sanitized_files, "docProps/core.xml").unwrap(),
            core_props
        );
    }

    #[test]
    fn excluding_revisions_leaves_author_untouched_but_still_sanitizes_text() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p>
                  <w:del w:id="1" w:author="Alice" w:date="2026-05-29T10:00:00Z">
                    <w:r><w:delText>Secret</w:delText></w:r>
                  </w:del>
                </w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let scope = Scope::parse_exclude("revisions").unwrap();
        let output = sanitize_document_text(&files, &scope, ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();

        assert!(!sanitized_str.contains("Secret"));
        assert!(sanitized_str.contains(r#"w:author="Alice""#));
        assert!(sanitized_str.contains(r#"w:date="2026-05-29T10:00:00Z""#));
    }

    #[test]
    fn constant_mode_replaces_body_text_with_fixed_string() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Hello world</w:t></w:r></w:p></w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output =
            sanitize_document_text(&files, &Scope::all(), ReplacementMode::Constant, false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();

        assert!(!sanitized_str.contains("Hello world"));
        assert!(sanitized_str.contains("<w:t>REDACTED</w:t>"));
    }

    #[test]
    fn clear_mode_empties_body_text() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p>
                  <w:r><w:t>Hello world</w:t></w:r>
                  <w:del w:author="Alice"><w:r><w:delText>Secret</w:delText></w:r></w:del>
                </w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::Clear, false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();

        assert!(!sanitized_str.contains("Hello world"));
        assert!(!sanitized_str.contains("Secret"));
        assert!(sanitized_str.contains("<w:t></w:t>"));
        assert!(sanitized_str.contains("<w:delText></w:delText>"));
        // Revision metadata is unaffected by replacement_mode -- it's a
        // separate, always-preserve-length setting.
        assert!(sanitized_str.contains(r#"w:author="Xxxxx""#));
    }

    #[test]
    fn replacement_mode_does_not_affect_doc_props() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let core_props: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                xmlns:dc="http://purl.org/dc/elements/1.1/">
              <dc:creator>Jane Doe</dc:creator>
            </cp:coreProperties>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
            ("docProps/core.xml", core_props),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output = sanitize_document_text(&files, &Scope::all(), ReplacementMode::Clear, false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_core = get_part(&sanitized_files, "docProps/core.xml").unwrap();

        // Still the fixed canonical placeholder, not emptied by Clear mode.
        assert!(std::str::from_utf8(sanitized_core).unwrap().contains("<dc:creator>Redacted</dc:creator>"));
    }

    #[test]
    fn remove_track_changes_collapses_to_accepted_state_before_sanitizing() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p>
                  <w:del w:id="1" w:author="Alice" w:date="2026-05-29T10:00:00Z">
                    <w:r><w:delText>Old secret</w:delText></w:r>
                  </w:del>
                  <w:ins w:id="2" w:author="Alice" w:date="2026-05-29T10:00:00Z">
                    <w:r><w:t>New text</w:t></w:r>
                  </w:ins>
                </w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output =
            sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), true, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();

        assert_well_formed(sanitized_document);
        // The deleted text is fully gone, not sanitized-in-place.
        assert!(!sanitized_str.contains("Old"));
        assert!(!sanitized_str.contains("secret"));
        // No tracked-changes wrappers or their author/date metadata remain.
        assert!(!sanitized_str.contains("w:del"));
        assert!(!sanitized_str.contains("w:ins"));
        assert!(!sanitized_str.contains("Alice"));
        // The inserted text survives, unwrapped, and still gets sanitized
        // like ordinary body text.
        assert!(sanitized_str.contains(r#"<w:t>Xxx xxxx</w:t>"#));
    }

    #[test]
    fn without_the_flag_track_changes_structure_is_preserved() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p>
                  <w:del w:id="1" w:author="Alice"><w:r><w:delText>Old</w:delText></w:r></w:del>
                </w:p>
              </w:body>
            </w:document>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let output =
            sanitize_document_text(&files, &Scope::all(), ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_document = get_part(&sanitized_files, "word/document.xml").unwrap();
        let sanitized_str = std::str::from_utf8(sanitized_document).unwrap();

        // w:del container survives (default is preservation, not removal);
        // its payload is still sanitized in place.
        assert!(sanitized_str.contains("<w:del "));
        assert!(sanitized_str.contains("<w:delText>Xxx</w:delText>"));
    }

    #[test]
    fn sanitizes_external_hyperlink_targets_in_rels_unconditionally() {
        let document_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body><w:p><w:r><w:t>Body</w:t></w:r></w:p></w:body>
            </w:document>"#;
        let document_rels: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://internal.example.com/secret-plan" TargetMode="External"/>
              <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
            </Relationships>"#;

        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", document_xml),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        // Excluding everything togglable must not exempt .rels sanitizing
        // -- it isn't part of the scope vocabulary at all.
        let scope = Scope::parse_exclude("headers,footers,comments,footnotes,endnotes,docprops,revisions")
            .unwrap();
        let output = sanitize_document_text(&files, &scope, ReplacementMode::default(), false, false).unwrap();
        let sanitized_files = unpack_docx(Cursor::new(output.bytes)).unwrap();
        let sanitized_rels = get_part(&sanitized_files, "word/_rels/document.xml.rels").unwrap();
        let sanitized_str = std::str::from_utf8(sanitized_rels).unwrap();

        assert!(!sanitized_str.contains("internal.example.com"));
        assert!(sanitized_str.contains(r#"Target="https://example.invalid/redacted""#));
        // Non-hyperlink relationship untouched.
        assert!(sanitized_str.contains(r#"Target="styles.xml""#));
    }
}
