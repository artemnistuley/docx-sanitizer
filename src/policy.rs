//! Strict (`fail-on-unsupported`) vs `best-effort` sanitization policy.
//!
//! Per DESIGN.md's "Default Safety Mode": strict mode fails on either kind
//! of unsupported content -- an unsupported *part class* (`CustomXml`,
//! `Media`, `Embedding`, per [`crate::part::SupportTier::Unsupported`]) or
//! an unsupported *payload surface* inside an otherwise-supported part
//! (e.g. an unrecognized `w:instrText` pattern, surfaced as
//! [`crate::report::UnsupportedPayload`] by the sanitize pass). Both kinds
//! are collected into one flat list of concerns; only the sanitization
//! *mode* decides whether that list blocks output.
//!
//! ## `--include`/`--exclude` does not narrow unsupported part-class checks
//!
//! [`Scope`] only toggles which *guaranteed* categories get sanitized
//! (headers, footers, comments, footnotes, endnotes, docProps, revision
//! metadata) -- it has no keyword for `CustomXml`/`Media`/`Embedding` at
//! all, so there is no well-defined way for those part classes to be "in
//! scope" or "out of scope". [`collect_concerns`] therefore always reports
//! unsupported part classes present in the package, regardless of `Scope`:
//! a narrow `--include comments` run still blocks strict mode if the
//! package contains `word/media/*`.
//!
//! This was a real design fork, not an oversight: the alternative --
//! letting any `--include`/`--exclude` use suppress part-class checks --
//! would make an unrelated scope narrowing silently turn off protection
//! against a structurally different risk (arbitrary binary/business-data
//! parts we can't inspect at all), for a document that happens to also
//! contain one. `--best-effort` is the intended, explicit way to proceed
//! despite unsupported part classes.
//!
//! Unsupported *payload-surface* concerns (unrecognized `w:instrText`) are
//! naturally scoped already, with no special-casing needed: they can only
//! be found in parts [`crate::sanitize::sanitize_document_text`] actually
//! processes, which is already gated by `Scope` (except
//! `word/document.xml`, which -- like part-class checks -- is never
//! excludable).

use std::collections::HashSet;

use crate::Error;
use crate::part::{ClassifiedPart, PartKind, SupportTier};
use crate::report::UnsupportedPayload;
use crate::xml::media::placeholder_bytes_for;

/// A togglable sanitization category selectable via `--include`/`--exclude`.
///
/// Notably absent: the main document body (`word/document.xml`'s `w:t`/
/// `w:delText`) is always in scope and cannot be excluded -- `--include`/
/// `--exclude` only narrow the *other* guaranteed parts, plus whether
/// revision/comment metadata attributes are rewritten within whatever parts
/// remain in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeCategory {
    Headers,
    Footers,
    Comments,
    Footnotes,
    Endnotes,
    /// `docProps/core.xml`, `docProps/app.xml`, and `docProps/custom.xml`
    /// together -- the plan's fixed scope vocabulary has one `docprops`
    /// keyword, not one per docProps part.
    DocProps,
    /// `w:author`/`w:initials`/`w:date` attribute rewriting specifically,
    /// not revision *text* (`w:delText`, or visible text inside
    /// `w:ins`/`w:del`) -- that's covered by the ordinary `w:t`/`w:delText`
    /// rewrite, which is always in scope. Excluding `revisions` leaves
    /// author names, initials, and dates on tracked changes/comments
    /// untouched while still sanitizing the text itself.
    Revisions,
}

const ALL_SCOPE_CATEGORIES: &[ScopeCategory] = &[
    ScopeCategory::Headers,
    ScopeCategory::Footers,
    ScopeCategory::Comments,
    ScopeCategory::Footnotes,
    ScopeCategory::Endnotes,
    ScopeCategory::DocProps,
    ScopeCategory::Revisions,
];

impl ScopeCategory {
    fn parse_keyword(keyword: &str) -> Result<Self, Error> {
        match keyword {
            "headers" => Ok(Self::Headers),
            "footers" => Ok(Self::Footers),
            "comments" => Ok(Self::Comments),
            "footnotes" => Ok(Self::Footnotes),
            "endnotes" => Ok(Self::Endnotes),
            "docprops" => Ok(Self::DocProps),
            "revisions" => Ok(Self::Revisions),
            other => Err(Error::InvalidScope(other.to_string())),
        }
    }
}

/// Which togglable categories (see [`ScopeCategory`]) are in scope for a
/// sanitize run, per `--include`/`--exclude <scope>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    included: HashSet<ScopeCategory>,
}

impl Scope {
    /// Every togglable category in scope -- the default when neither
    /// `--include` nor `--exclude` is given.
    pub fn all() -> Self {
        Self {
            included: ALL_SCOPE_CATEGORIES.iter().copied().collect(),
        }
    }

    /// Parse a `--include <scope>` value: only the listed categories are
    /// in scope.
    pub fn parse_include(spec: &str) -> Result<Self, Error> {
        Ok(Self {
            included: parse_keywords(spec)?,
        })
    }

    /// Parse an `--exclude <scope>` value: every category except the
    /// listed ones is in scope.
    pub fn parse_exclude(spec: &str) -> Result<Self, Error> {
        let excluded = parse_keywords(spec)?;
        Ok(Self {
            included: ALL_SCOPE_CATEGORIES
                .iter()
                .copied()
                .filter(|category| !excluded.contains(category))
                .collect(),
        })
    }

    pub fn contains(&self, category: ScopeCategory) -> bool {
        self.included.contains(&category)
    }
}

fn parse_keywords(spec: &str) -> Result<HashSet<ScopeCategory>, Error> {
    spec.split(',')
        .map(str::trim)
        .filter(|keyword| !keyword.is_empty())
        .map(ScopeCategory::parse_keyword)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SanitizeMode {
    /// Refuse to produce output if any concern is present. The default,
    /// per DESIGN.md, to avoid producing a document that looks sanitized
    /// while still containing unsupported confidential payload.
    #[default]
    Strict,
    /// Produce output regardless of concerns found.
    BestEffort,
}

/// Structural/content sanitization toggles beyond `mode`/`scope`/
/// `replacement_mode`, grouped into one struct rather than several adjacent
/// same-type (`bool`) positional parameters -- which becomes a real
/// call-site correctness risk (silently swapping two `bool`s compiles) once
/// there's more than a couple. See DESIGN.md Decision Log #11.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SanitizePolicy {
    /// Collapse tracked changes to their accepted state before sanitizing.
    /// See [`crate::xml::track_changes`].
    pub remove_track_changes: bool,
    /// Replace `word/media/*` images with a fixed placeholder for parts
    /// with a supported extension. See [`crate::xml::media`].
    pub strip_media: bool,
    /// Replace text-node payload in `word/customXml/*` parts in place,
    /// regardless of schema. See
    /// [`crate::xml::rewrite::rewrite_all_text_nodes`].
    pub sanitize_customxml: bool,
}

/// Collect every unsupported-content concern for a sanitize run: unsupported
/// part classes present in the package, plus unsupported payload surfaces
/// found while sanitizing supported parts (e.g. unrecognized `w:instrText`).
///
/// `policy.strip_media`: when set, a `Media` part with a supported
/// placeholder extension (see [`crate::xml::media`]) is not reported as a
/// concern -- its bytes are replaced with a placeholder rather than left as
/// unsupported content. A `Media` part whose extension has no placeholder
/// (e.g. `.emf`) is still a concern even with `strip_media` set.
///
/// `policy.sanitize_customxml`: when set, a `CustomXml` *part class* concern
/// is not reported -- unlike media, this always applies regardless of the
/// part's content, since any well-formed XML can be walked by
/// [`crate::xml::rewrite::rewrite_all_text_nodes`]. This does not mean the
/// part is unconditionally fully sanitized, though: that rewrite pass
/// deliberately leaves mixed-content elements' own direct text untouched
/// (see its docs), and when it does, the caller is expected to have already
/// turned that into a `payload_findings` entry (fail-closed, like an
/// unrecognized `w:instrText` pattern) -- which still ends up in `concerns`
/// via the `payload_findings` extension below, independent of this
/// part-class filter.
pub fn collect_concerns(
    parts: &[ClassifiedPart],
    payload_findings: &[UnsupportedPayload],
    policy: SanitizePolicy,
) -> Vec<UnsupportedPayload> {
    let mut concerns: Vec<UnsupportedPayload> = parts
        .iter()
        .filter(|part| part.tier == SupportTier::Unsupported)
        .filter(|part| {
            !(policy.strip_media
                && part.kind == PartKind::Media
                && placeholder_bytes_for(&part.path).is_some())
        })
        .filter(|part| !(policy.sanitize_customxml && part.kind == PartKind::CustomXml))
        .map(|part| UnsupportedPayload {
            part: part.path.clone(),
            description: format!("unsupported part class: {}", part.kind),
        })
        .collect();
    concerns.extend(payload_findings.iter().cloned());
    concerns
}

/// Whether `mode` should block output given `concerns`.
pub fn should_block(mode: SanitizeMode, concerns: &[UnsupportedPayload]) -> bool {
    mode == SanitizeMode::Strict && !concerns.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{SanitizeMode, SanitizePolicy, Scope, ScopeCategory, collect_concerns, should_block};
    use crate::Error;
    use crate::part::{ClassifiedPart, PartKind, SupportTier};
    use crate::report::UnsupportedPayload;

    #[test]
    fn all_scope_includes_every_category() {
        let scope = Scope::all();
        for category in [
            ScopeCategory::Headers,
            ScopeCategory::Footers,
            ScopeCategory::Comments,
            ScopeCategory::Footnotes,
            ScopeCategory::Endnotes,
            ScopeCategory::DocProps,
            ScopeCategory::Revisions,
        ] {
            assert!(scope.contains(category));
        }
    }

    #[test]
    fn include_scope_is_an_allowlist() {
        let scope = Scope::parse_include("headers,comments").unwrap();
        assert!(scope.contains(ScopeCategory::Headers));
        assert!(scope.contains(ScopeCategory::Comments));
        assert!(!scope.contains(ScopeCategory::Footers));
        assert!(!scope.contains(ScopeCategory::Revisions));
    }

    #[test]
    fn exclude_scope_is_a_denylist() {
        let scope = Scope::parse_exclude("revisions,docprops").unwrap();
        assert!(!scope.contains(ScopeCategory::Revisions));
        assert!(!scope.contains(ScopeCategory::DocProps));
        assert!(scope.contains(ScopeCategory::Headers));
        assert!(scope.contains(ScopeCategory::Comments));
    }

    #[test]
    fn parse_tolerates_whitespace_around_keywords() {
        let scope = Scope::parse_include(" headers , footers ").unwrap();
        assert!(scope.contains(ScopeCategory::Headers));
        assert!(scope.contains(ScopeCategory::Footers));
        assert!(!scope.contains(ScopeCategory::Comments));
    }

    #[test]
    fn unknown_keyword_is_an_error() {
        let err = Scope::parse_include("headers,bogus").unwrap_err();
        assert!(matches!(err, Error::InvalidScope(keyword) if keyword == "bogus"));
    }

    #[test]
    fn empty_spec_includes_nothing() {
        let scope = Scope::parse_include("").unwrap();
        assert!(!scope.contains(ScopeCategory::Headers));
    }

    fn part(path: &str, kind: PartKind, tier: SupportTier) -> ClassifiedPart {
        ClassifiedPart {
            path: path.to_string(),
            kind,
            tier,
        }
    }

    #[test]
    fn collects_unsupported_part_classes() {
        let parts = vec![
            part("word/document.xml", PartKind::MainDocument, SupportTier::Guaranteed),
            part("customXml/item1.xml", PartKind::CustomXml, SupportTier::Unsupported),
            part("word/media/image1.png", PartKind::Media, SupportTier::Unsupported),
        ];

        let concerns = collect_concerns(&parts, &[], SanitizePolicy::default());

        assert_eq!(concerns.len(), 2);
        assert!(concerns.iter().any(|c| c.part == "customXml/item1.xml"));
        assert!(concerns.iter().any(|c| c.part == "word/media/image1.png"));
    }

    #[test]
    fn collects_unsupported_payload_findings() {
        let parts = vec![part(
            "word/document.xml",
            PartKind::MainDocument,
            SupportTier::Guaranteed,
        )];
        let findings = vec![UnsupportedPayload {
            part: "word/document.xml".to_string(),
            description: "unrecognized w:instrText field instruction: MERGEFIELD".to_string(),
        }];

        let concerns = collect_concerns(&parts, &findings, SanitizePolicy::default());

        assert_eq!(concerns.len(), 1);
        assert_eq!(concerns[0].part, "word/document.xml");
    }

    #[test]
    fn no_concerns_when_everything_supported() {
        let parts = vec![part(
            "word/document.xml",
            PartKind::MainDocument,
            SupportTier::Guaranteed,
        )];

        assert!(collect_concerns(&parts, &[], SanitizePolicy::default()).is_empty());
    }

    #[test]
    fn strip_media_excludes_media_with_supported_extension_from_concerns() {
        let parts = vec![
            part("customXml/item1.xml", PartKind::CustomXml, SupportTier::Unsupported),
            part("word/media/image1.png", PartKind::Media, SupportTier::Unsupported),
        ];

        let concerns = collect_concerns(&parts, &[], SanitizePolicy { strip_media: true, ..Default::default() });

        assert_eq!(concerns.len(), 1);
        assert_eq!(concerns[0].part, "customXml/item1.xml");
    }

    #[test]
    fn strip_media_still_flags_media_with_unsupported_extension() {
        let parts = vec![part("word/media/image1.emf", PartKind::Media, SupportTier::Unsupported)];

        let concerns = collect_concerns(&parts, &[], SanitizePolicy { strip_media: true, ..Default::default() });

        assert_eq!(concerns.len(), 1);
        assert_eq!(concerns[0].part, "word/media/image1.emf");
    }

    #[test]
    fn sanitize_customxml_excludes_all_customxml_from_concerns() {
        let parts = vec![
            part("customXml/item1.xml", PartKind::CustomXml, SupportTier::Unsupported),
            part("word/media/image1.png", PartKind::Media, SupportTier::Unsupported),
        ];

        let concerns = collect_concerns(
            &parts,
            &[],
            SanitizePolicy { sanitize_customxml: true, ..Default::default() },
        );

        assert_eq!(concerns.len(), 1);
        assert_eq!(concerns[0].part, "word/media/image1.png");
    }

    #[test]
    fn strip_media_and_sanitize_customxml_combine() {
        let parts = vec![
            part("customXml/item1.xml", PartKind::CustomXml, SupportTier::Unsupported),
            part("word/media/image1.png", PartKind::Media, SupportTier::Unsupported),
        ];

        let concerns = collect_concerns(
            &parts,
            &[],
            SanitizePolicy { strip_media: true, sanitize_customxml: true, ..Default::default() },
        );

        assert!(concerns.is_empty());
    }

    #[test]
    fn strict_mode_blocks_on_any_concern() {
        let concerns = vec![UnsupportedPayload {
            part: "customXml/item1.xml".to_string(),
            description: "unsupported part class: CustomXml".to_string(),
        }];

        assert!(should_block(SanitizeMode::Strict, &concerns));
        assert!(!should_block(SanitizeMode::Strict, &[]));
    }

    #[test]
    fn best_effort_never_blocks() {
        let concerns = vec![UnsupportedPayload {
            part: "customXml/item1.xml".to_string(),
            description: "unsupported part class: CustomXml".to_string(),
        }];

        assert!(!should_block(SanitizeMode::BestEffort, &concerns));
        assert!(!should_block(SanitizeMode::BestEffort, &[]));
    }

    #[test]
    fn default_mode_is_strict() {
        assert_eq!(SanitizeMode::default(), SanitizeMode::Strict);
    }
}
