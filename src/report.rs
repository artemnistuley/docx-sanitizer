//! Findings and the overall sanitization report model.
//!
//! [`UnsupportedPayload`] is the shared "here's something we couldn't
//! confidently sanitize" record, used both to decide strict-mode pass/fail
//! (Step 7's [`crate::policy`]) and as part of the [`Report`] model here
//! (Step 8). [`build_report`] assembles [`Report`] from data Steps 2-7
//! already produce (classified parts, `Scope`, collected concerns) -- it
//! does not do any sanitizing itself.

use serde::Serialize;

use crate::part::{ClassifiedPart, PartKind, SupportTier};
use crate::policy::{SanitizeMode, Scope, ScopeCategory, should_block};
use crate::xml::media::placeholder_bytes_for;

/// A payload surface that could not be confidently classified or rewritten
/// (e.g. an unrecognized `w:instrText` field instruction pattern).
///
/// `description` deliberately does not embed the original payload text --
/// only structural information safe to surface in a report shared
/// alongside the sanitized document (e.g. a field-instruction keyword, not
/// its literal arguments).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnsupportedPayload {
    /// Package-relative path of the part the finding was found in.
    pub part: String,
    /// Human-readable, payload-free description of what was skipped.
    pub description: String,
}

/// What happened to a single part during (or instead of) sanitization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartStatus {
    /// A guaranteed-tier part that was actually rewritten.
    Sanitized,
    /// Copied through byte-for-byte: either a best-effort-tier part (not
    /// sanitized in v1), or a guaranteed-tier part excluded from this run's
    /// `Scope`.
    Passthrough,
    /// An unsupported-tier part class (`CustomXml`, `Media`, `Embedding`).
    Unsupported,
    /// A `Media` part whose bytes were replaced with a fixed placeholder
    /// image, per `--strip-media` (see [`crate::xml::media`]). Distinct from
    /// `Sanitized` since this isn't payload-preserving-shape rewriting --
    /// the original image content is fully replaced, not edited in place.
    Placeholder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartReport {
    pub path: String,
    pub kind: String,
    pub tier: String,
    pub status: PartStatus,
}

/// Whether a sanitize run would produce (or did produce) output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SanitizeOutcome {
    Sanitized,
    Blocked,
}

/// The full sanitization report for a document: per-part status plus the
/// concerns (see [`UnsupportedPayload`]) that decide `outcome`.
///
/// Deliberately does not include the input file path -- that's a
/// filesystem/CLI-level detail, not something the sanitization engine
/// needs to know to describe a package it already has in memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub mode: SanitizeMode,
    pub outcome: SanitizeOutcome,
    pub parts: Vec<PartReport>,
    pub concerns: Vec<UnsupportedPayload>,
}

/// Assemble a [`Report`] from already-computed classification and concerns.
/// `strip_media` must match the value passed to sanitization for `parts`'
/// statuses to accurately reflect which `Media` parts became placeholders
/// (see [`PartStatus::Placeholder`]).
pub fn build_report(
    mode: SanitizeMode,
    scope: &Scope,
    parts: &[ClassifiedPart],
    concerns: &[UnsupportedPayload],
    strip_media: bool,
) -> Report {
    let outcome = if should_block(mode, concerns) {
        SanitizeOutcome::Blocked
    } else {
        SanitizeOutcome::Sanitized
    };

    let parts = parts
        .iter()
        .map(|part| PartReport {
            path: part.path.clone(),
            kind: part.kind.to_string(),
            tier: part.tier.to_string(),
            status: part_status(part, scope, strip_media),
        })
        .collect();

    Report {
        mode,
        outcome,
        parts,
        concerns: concerns.to_vec(),
    }
}

fn part_status(part: &ClassifiedPart, scope: &Scope, strip_media: bool) -> PartStatus {
    if part.kind == PartKind::Media && strip_media && placeholder_bytes_for(&part.path).is_some() {
        return PartStatus::Placeholder;
    }

    match part.tier {
        SupportTier::Unsupported => PartStatus::Unsupported,
        SupportTier::BestEffort => PartStatus::Passthrough,
        SupportTier::Guaranteed => match scope_category_for(&part.kind) {
            None => PartStatus::Sanitized,
            Some(category) if scope.contains(category) => PartStatus::Sanitized,
            Some(_) => PartStatus::Passthrough,
        },
    }
}

/// Maps a guaranteed-tier [`PartKind`] to the [`ScopeCategory`] that
/// toggles it, if any. `None` means the part is always sanitized
/// regardless of `Scope` (currently only `word/document.xml`).
fn scope_category_for(kind: &PartKind) -> Option<ScopeCategory> {
    match kind {
        PartKind::Header(_) => Some(ScopeCategory::Headers),
        PartKind::Footer(_) => Some(ScopeCategory::Footers),
        PartKind::Comments => Some(ScopeCategory::Comments),
        PartKind::Footnotes => Some(ScopeCategory::Footnotes),
        PartKind::Endnotes => Some(ScopeCategory::Endnotes),
        PartKind::CoreProps | PartKind::AppProps | PartKind::CustomProps => Some(ScopeCategory::DocProps),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{PartStatus, SanitizeOutcome, UnsupportedPayload, build_report};
    use crate::part::{ClassifiedPart, PartKind, SupportTier};
    use crate::policy::{SanitizeMode, Scope};

    fn part(path: &str, kind: PartKind, tier: SupportTier) -> ClassifiedPart {
        ClassifiedPart {
            path: path.to_string(),
            kind,
            tier,
        }
    }

    #[test]
    fn main_document_is_always_sanitized() {
        let parts = vec![part("word/document.xml", PartKind::MainDocument, SupportTier::Guaranteed)];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &parts, &[], false);
        assert_eq!(report.parts[0].status, PartStatus::Sanitized);
    }

    #[test]
    fn excluded_guaranteed_part_is_passthrough() {
        let parts = vec![part("word/header1.xml", PartKind::Header(1), SupportTier::Guaranteed)];
        let scope = Scope::parse_exclude("headers").unwrap();
        let report = build_report(SanitizeMode::Strict, &scope, &parts, &[], false);
        assert_eq!(report.parts[0].status, PartStatus::Passthrough);
    }

    #[test]
    fn included_guaranteed_part_is_sanitized() {
        let parts = vec![part("word/header1.xml", PartKind::Header(1), SupportTier::Guaranteed)];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &parts, &[], false);
        assert_eq!(report.parts[0].status, PartStatus::Sanitized);
    }

    #[test]
    fn best_effort_tier_part_is_passthrough() {
        let parts = vec![part("word/styles.xml", PartKind::Other, SupportTier::BestEffort)];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &parts, &[], false);
        assert_eq!(report.parts[0].status, PartStatus::Passthrough);
    }

    #[test]
    fn unsupported_tier_part_is_unsupported() {
        let parts = vec![part("customXml/item1.xml", PartKind::CustomXml, SupportTier::Unsupported)];
        let report = build_report(SanitizeMode::BestEffort, &Scope::all(), &parts, &[], false);
        assert_eq!(report.parts[0].status, PartStatus::Unsupported);
    }

    #[test]
    fn media_with_strip_media_and_supported_extension_is_placeholder() {
        let parts = vec![part("word/media/image1.png", PartKind::Media, SupportTier::Unsupported)];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &parts, &[], true);
        assert_eq!(report.parts[0].status, PartStatus::Placeholder);
    }

    #[test]
    fn media_without_strip_media_stays_unsupported() {
        let parts = vec![part("word/media/image1.png", PartKind::Media, SupportTier::Unsupported)];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &parts, &[], false);
        assert_eq!(report.parts[0].status, PartStatus::Unsupported);
    }

    #[test]
    fn media_with_strip_media_but_unsupported_extension_stays_unsupported() {
        let parts = vec![part("word/media/image1.emf", PartKind::Media, SupportTier::Unsupported)];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &parts, &[], true);
        assert_eq!(report.parts[0].status, PartStatus::Unsupported);
    }

    #[test]
    fn outcome_is_blocked_in_strict_mode_with_concerns() {
        let concerns = vec![UnsupportedPayload {
            part: "customXml/item1.xml".to_string(),
            description: "unsupported part class: CustomXml".to_string(),
        }];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &[], &concerns, false);
        assert_eq!(report.outcome, SanitizeOutcome::Blocked);
        assert_eq!(report.concerns.len(), 1);
    }

    #[test]
    fn outcome_is_sanitized_in_best_effort_mode_with_concerns() {
        let concerns = vec![UnsupportedPayload {
            part: "customXml/item1.xml".to_string(),
            description: "unsupported part class: CustomXml".to_string(),
        }];
        let report = build_report(SanitizeMode::BestEffort, &Scope::all(), &[], &concerns, false);
        assert_eq!(report.outcome, SanitizeOutcome::Sanitized);
    }

    #[test]
    fn outcome_is_sanitized_with_no_concerns() {
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &[], &[], false);
        assert_eq!(report.outcome, SanitizeOutcome::Sanitized);
    }

    #[test]
    fn serializes_to_expected_json_shape() {
        let parts = vec![part("word/document.xml", PartKind::MainDocument, SupportTier::Guaranteed)];
        let report = build_report(SanitizeMode::Strict, &Scope::all(), &parts, &[], false);
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(json["mode"], "strict");
        assert_eq!(json["outcome"], "sanitized");
        assert_eq!(json["parts"][0]["path"], "word/document.xml");
        assert_eq!(json["parts"][0]["status"], "sanitized");
        assert_eq!(json["concerns"].as_array().unwrap().len(), 0);
    }
}
