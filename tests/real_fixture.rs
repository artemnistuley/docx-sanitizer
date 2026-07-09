//! Integration tests against a real Word-produced `.docx` fixture, as
//! opposed to the hand-crafted XML snippets used by the unit tests. Real
//! documents exercise serialization details (attribute order, self-closing
//! tags, namespace prefix choices, entity encoding) that synthetic
//! fixtures can miss.

use std::io::Cursor;
use std::path::PathBuf;

use docx_sanitizer::policy::{SanitizeMode, Scope};
use docx_sanitizer::sanitize::{SanitizeResult, sanitize};
use docx_sanitizer::xml::text::{CANONICAL_HYPERLINK_TARGET, CANONICAL_TIMESTAMP, ReplacementMode};
use docx_sanitizer::zip::unpack_docx;

const JPEG_MAGIC: &[u8] = &[0xFF, 0xD8, 0xFF];

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn unpack_fixture(name: &str) -> docx_sanitizer::zip::FileRegistry {
    let bytes = std::fs::read(fixture_path(name)).expect("fixture must be readable");
    unpack_docx(Cursor::new(bytes)).expect("fixture must be a valid docx package")
}

#[test]
fn strict_mode_blocks_on_customxml() {
    let files = unpack_fixture("test-doc.docx");

    let result = sanitize(
        &files,
        SanitizeMode::Strict,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        false,
        false,
    )
    .unwrap();

    let concerns = match result {
        SanitizeResult::Blocked { concerns } => concerns,
        SanitizeResult::Produced(_) => panic!("expected strict mode to block on customXml/*"),
    };
    assert!(concerns.iter().any(|c| c.part.starts_with("customXml/")));
}

#[test]
fn best_effort_sanitizes_known_sensitive_values() {
    let files = unpack_fixture("test-doc.docx");

    let output = match sanitize(
        &files,
        SanitizeMode::BestEffort,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        false,
        false,
    )
    .unwrap()
    {
        SanitizeResult::Produced(output) => output,
        SanitizeResult::Blocked { concerns } => {
            panic!("best-effort mode should always produce output, got concerns: {concerns:?}")
        }
    };

    let sanitized = unpack_docx(Cursor::new(output.bytes)).unwrap();

    let core_props = std::str::from_utf8(&sanitized["docProps/core.xml"]).unwrap();
    assert!(
        !core_props.contains("Chris Crawford"),
        "real author name must not survive into sanitized core.xml"
    );
    assert!(!core_props.contains("2026-07-03T14:31:00Z"));
    assert!(core_props.contains(CANONICAL_TIMESTAMP));

    let custom_props = std::str::from_utf8(&sanitized["docProps/custom.xml"]).unwrap();
    assert!(!custom_props.contains("1.43.2"));
    assert!(!custom_props.contains("70DFD301-AC0C-9F40-89AC-A947D38E1AE2"));

    let document = std::str::from_utf8(&sanitized["word/document.xml"]).unwrap();
    assert!(
        !document.contains("Mutual Agreement for Document Excellence"),
        "original body text must not survive sanitization"
    );

    let footer = std::str::from_utf8(&sanitized["word/footer1.xml"]).unwrap();
    assert!(!footer.contains("Page "));

    let rels = std::str::from_utf8(&sanitized["word/_rels/document.xml.rels"]).unwrap();
    assert!(!rels.contains("docs.superdoc.dev"));
    assert!(rels.contains(CANONICAL_HYPERLINK_TARGET));
    // Non-hyperlink relationships (theme, styles, ...) must stay untouched.
    assert!(rels.contains(r#"Target="theme/theme1.xml""#));
    assert!(rels.contains(r#"Target="styles.xml""#));

    // customXml is unsupported content; best-effort mode passes it through
    // unchanged rather than sanitizing or dropping it.
    assert_eq!(
        sanitized["customXml/item1.xml"],
        files["customXml/item1.xml"]
    );
}

#[test]
fn best_effort_sanitized_part_contents_are_deterministic_across_runs() {
    // Per DESIGN.md, identical ZIP bytes are not guaranteed (e.g. entry
    // timestamps), but the sanitized content of each part must be.
    let files = unpack_fixture("test-doc.docx");

    let run = || {
        match sanitize(
            &files,
            SanitizeMode::BestEffort,
            &Scope::all(),
            ReplacementMode::PreserveLength,
            false,
            false,
        )
        .unwrap()
        {
            SanitizeResult::Produced(output) => unpack_docx(Cursor::new(output.bytes)).unwrap(),
            SanitizeResult::Blocked { .. } => panic!("expected best-effort output"),
        }
    };

    assert_eq!(run(), run());
}

#[test]
fn narrow_include_scope_still_blocks_strict_mode_on_customxml() {
    let files = unpack_fixture("test-doc.docx");

    // Excluding every togglable category still leaves customXml/* as an
    // always-on strict-mode blocker (see policy.rs module docs).
    let scope = Scope::parse_exclude(
        "headers,footers,comments,footnotes,endnotes,docprops,revisions",
    )
    .unwrap();

    let result = sanitize(
        &files,
        SanitizeMode::Strict,
        &scope,
        ReplacementMode::PreserveLength,
        false,
        false,
    )
    .unwrap();

    assert!(matches!(result, SanitizeResult::Blocked { .. }));
}

#[test]
fn strict_mode_blocks_on_media() {
    let files = unpack_fixture("test-doc-2.docx");

    let result = sanitize(
        &files,
        SanitizeMode::Strict,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        false,
        false,
    )
    .unwrap();

    let concerns = match result {
        SanitizeResult::Blocked { concerns } => concerns,
        SanitizeResult::Produced(_) => panic!("expected strict mode to block on word/media/*"),
    };
    assert!(concerns.iter().any(|c| c.part.starts_with("word/media/")));
}

#[test]
fn best_effort_sanitizes_metadata_and_passes_media_through_unchanged() {
    let files = unpack_fixture("test-doc-2.docx");

    let output = match sanitize(
        &files,
        SanitizeMode::BestEffort,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        false,
        false,
    )
    .unwrap()
    {
        SanitizeResult::Produced(output) => output,
        SanitizeResult::Blocked { concerns } => {
            panic!("best-effort mode should always produce output, got concerns: {concerns:?}")
        }
    };

    let sanitized = unpack_docx(Cursor::new(output.bytes)).unwrap();

    let core_props = std::str::from_utf8(&sanitized["docProps/core.xml"]).unwrap();
    assert!(!core_props.contains("Theresa Whitworth"));
    assert!(!core_props.contains("Microsoft Word - Agenda - 5-9-2022"));
    assert!(!core_props.contains("2025-06-17T14:29:00Z"));
    assert!(core_props.contains(CANONICAL_TIMESTAMP));

    let document = std::str::from_utf8(&sanitized["word/document.xml"]).unwrap();
    assert!(!document.contains("Prattsville Board Agenda"));

    // Binary media is unsupported content; best-effort mode passes it
    // through byte-for-byte rather than sanitizing or dropping it.
    assert_eq!(
        sanitized["word/media/image1.jpeg"],
        files["word/media/image1.jpeg"]
    );
}

#[test]
fn strict_mode_succeeds_on_a_document_with_only_guaranteed_scope_parts() {
    // No customXml, media, or embeddings here -- strict mode must produce
    // output rather than block, unlike the customXml/media fixtures above.
    let files = unpack_fixture("test-doc-3.docx");

    let output = match sanitize(
        &files,
        SanitizeMode::Strict,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        false,
        false,
    )
    .unwrap()
    {
        SanitizeResult::Produced(output) => output,
        SanitizeResult::Blocked { concerns } => {
            panic!("expected strict mode to succeed, got concerns: {concerns:?}")
        }
    };

    let sanitized = unpack_docx(Cursor::new(output.bytes)).unwrap();

    let core_props = std::str::from_utf8(&sanitized["docProps/core.xml"]).unwrap();
    assert!(!core_props.contains("Harper, Michael"));
    assert!(!core_props.contains("2017-04-12T04:31:00Z"));
    assert!(core_props.contains(CANONICAL_TIMESTAMP));

    let document = std::str::from_utf8(&sanitized["word/document.xml"]).unwrap();
    assert!(!document.contains("COMMONWEALTH BANKS."));

    let header = std::str::from_utf8(&sanitized["word/header1.xml"]).unwrap();
    assert!(!header.contains("Commonwealth Banks."));
}

#[test]
fn strict_mode_sanitizes_comments_and_tracked_changes_while_preserving_structure() {
    // Default policy: track-changes structure (w:ins/w:del) is preserved,
    // not collapsed, per DESIGN.md.
    let files = unpack_fixture("test-doc-4.docx");

    let output = match sanitize(
        &files,
        SanitizeMode::Strict,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        false,
        false,
    )
    .unwrap()
    {
        SanitizeResult::Produced(output) => output,
        SanitizeResult::Blocked { concerns } => {
            panic!("expected strict mode to succeed, got concerns: {concerns:?}")
        }
    };

    let sanitized = unpack_docx(Cursor::new(output.bytes)).unwrap();

    let document = std::str::from_utf8(&sanitized["word/document.xml"]).unwrap();
    assert!(document.contains("<w:ins"), "tracked-insert wrapper must survive");
    assert!(document.contains("<w:del"), "tracked-delete wrapper must survive");
    assert!(!document.contains("scrambled"));
    assert!(!document.contains("Insert text."));
    assert!(!document.contains("Artem Nistuley"), "revision author must be sanitized");
    assert!(!document.contains("2026-07-09T14:32:00Z"), "revision date must be sanitized");
    assert!(document.contains(CANONICAL_TIMESTAMP));

    let comments = std::str::from_utf8(&sanitized["word/comments.xml"]).unwrap();
    assert!(!comments.contains("Add some comment."));
    assert!(!comments.contains("Comment 2."));
    assert!(!comments.contains("Artem Nistuley"));
    assert!(!comments.contains("2026-07-09T14:32:00Z"));
    assert!(comments.contains(CANONICAL_TIMESTAMP));

    let core_props = std::str::from_utf8(&sanitized["docProps/core.xml"]).unwrap();
    assert!(!core_props.contains("Artem Nistuley"));

    // word/people.xml is not in guaranteed scope; best-effort/strict does
    // not touch it, so the fixture's own email stays untouched here.
    let people = std::str::from_utf8(&sanitized["word/people.xml"]).unwrap();
    assert!(people.contains("artem.nistuley@gmail.com"));
}

#[test]
fn remove_track_changes_collapses_real_document_to_accepted_state() {
    let files = unpack_fixture("test-doc-4.docx");

    let output = match sanitize(
        &files,
        SanitizeMode::Strict,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        true,
        false,
    )
    .unwrap()
    {
        SanitizeResult::Produced(output) => output,
        SanitizeResult::Blocked { concerns } => {
            panic!("expected strict mode to succeed, got concerns: {concerns:?}")
        }
    };

    let sanitized = unpack_docx(Cursor::new(output.bytes)).unwrap();
    let document = std::str::from_utf8(&sanitized["word/document.xml"]).unwrap();

    assert!(!document.contains("<w:ins"), "insert wrapper must be unwrapped");
    assert!(!document.contains("<w:del"), "delete wrapper must be removed entirely");
    assert!(!document.contains("scrambled"), "deleted text must not survive collapse");
}

#[test]
fn strip_media_lets_strict_mode_succeed_and_replaces_the_real_image() {
    // test-doc-2.docx's word/media/image1.jpeg is exactly the case
    // --strip-media targets: without it, strict mode blocks (see
    // strict_mode_blocks_on_media above).
    let files = unpack_fixture("test-doc-2.docx");

    let output = match sanitize(
        &files,
        SanitizeMode::Strict,
        &Scope::all(),
        ReplacementMode::PreserveLength,
        false,
        true,
    )
    .unwrap()
    {
        SanitizeResult::Produced(output) => output,
        SanitizeResult::Blocked { concerns } => {
            panic!("expected --strip-media to let strict mode succeed, got concerns: {concerns:?}")
        }
    };

    let sanitized = unpack_docx(Cursor::new(output.bytes)).unwrap();
    let placeholder = &sanitized["word/media/image1.jpeg"];

    assert_ne!(placeholder, &files["word/media/image1.jpeg"]);
    assert_eq!(&placeholder[..3], JPEG_MAGIC, "placeholder must still be a valid JPEG");
}
