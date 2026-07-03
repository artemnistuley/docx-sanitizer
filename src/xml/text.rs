//! Text replacement strategies for payload units such as `w:t` / `w:delText`.

use crate::Error;

/// Fixed constant string substituted for non-empty body text under
/// [`ReplacementMode::Constant`].
pub const CONSTANT_REPLACEMENT: &str = "REDACTED";

/// Which replacement strategy to apply to visible/revision text
/// (`w:t`/`w:delText`). Scoped to body text only -- revision metadata
/// (author/initials/date) and docProps values have their own fixed
/// defaults (see [`crate::xml::props`] and `sanitize.rs`) independent of
/// this setting, per DESIGN.md's Replacement Strategies section listing
/// them as separate "Recommended defaults".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplacementMode {
    /// Map each character to a placeholder of the same class, keeping
    /// length and rough shape (see [`preserve_length`]).
    #[default]
    PreserveLength,
    /// Replace all non-empty text with a fixed constant string.
    Constant,
    /// Replace all non-empty text with an empty string.
    Clear,
}

impl ReplacementMode {
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "preserve-length" => Ok(Self::PreserveLength),
            "constant" => Ok(Self::Constant),
            "clear" => Ok(Self::Clear),
            other => Err(Error::InvalidReplacementMode(other.to_string())),
        }
    }

    pub fn apply(self, text: &str) -> String {
        match self {
            Self::PreserveLength => preserve_length(text),
            Self::Constant => constant(text),
            Self::Clear => clear(text),
        }
    }
}

/// Fixed canonical timestamp substituted for revision/comment `w:date`
/// values (per DESIGN.md's Replacement Strategies: "timestamps: fixed
/// canonical timestamp").
pub const CANONICAL_TIMESTAMP: &str = "2000-01-01T00:00:00Z";

/// Fixed canonical, guaranteed-non-resolving hyperlink target (RFC 2606
/// reserved `.invalid` TLD) substituted for sanitized hyperlink targets
/// (per DESIGN.md's Replacement Strategies: "hyperlink targets: safe
/// canonical targets").
pub const CANONICAL_HYPERLINK_TARGET: &str = "https://example.invalid/redacted";

/// `preserve-length`: map each visible character to a placeholder character
/// of the same Unicode-scalar-value class, keeping char count (not byte
/// count -- UTF-8 is variable width) identical to the source.
///
/// Whitespace is preserved as-is (keeps line wrapping intact); digits map to
/// `0`; uppercase letters map to `X`; everything else maps to `x`. E.g.
/// `"John Smith"` becomes `"Xxxx Xxxxx"`.
pub fn preserve_length(text: &str) -> String {
    text.chars().map(replacement_char).collect()
}

fn replacement_char(c: char) -> char {
    if c.is_whitespace() {
        c
    } else if c.is_ascii_digit() {
        '0'
    } else if c.is_uppercase() {
        'X'
    } else {
        'x'
    }
}

/// `constant`: replace non-empty text with a fixed constant string,
/// leaving already-empty text empty (nothing to redact, and this keeps a
/// self-closing `<w:t/>` self-closing -- see
/// [`crate::xml::rewrite::rewrite_text_elements`]'s shape-preservation
/// rule).
pub fn constant(text: &str) -> String {
    if text.is_empty() { String::new() } else { CONSTANT_REPLACEMENT.to_string() }
}

/// `clear`: replace any text, empty or not, with an empty string.
pub fn clear(_text: &str) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::{ReplacementMode, clear, constant, preserve_length};

    #[test]
    fn maps_letters_by_case_and_keeps_whitespace() {
        assert_eq!(preserve_length("John Smith"), "Xxxx Xxxxx");
    }

    #[test]
    fn maps_digits_to_zero() {
        assert_eq!(preserve_length("Order #4213"), "Xxxxx x0000");
    }

    #[test]
    fn preserves_char_count_for_multibyte_input() {
        let input = "Café Münchën";
        assert_eq!(preserve_length(input).chars().count(), input.chars().count());
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(preserve_length(""), "");
    }

    #[test]
    fn constant_replaces_non_empty_text() {
        assert_eq!(constant("John Smith"), "REDACTED");
        assert_eq!(constant("x"), "REDACTED");
    }

    #[test]
    fn constant_leaves_empty_text_empty() {
        assert_eq!(constant(""), "");
    }

    #[test]
    fn clear_always_produces_empty_string() {
        assert_eq!(clear("John Smith"), "");
        assert_eq!(clear(""), "");
    }

    #[test]
    fn replacement_mode_parses_known_values() {
        assert_eq!(ReplacementMode::parse("preserve-length").unwrap(), ReplacementMode::PreserveLength);
        assert_eq!(ReplacementMode::parse("constant").unwrap(), ReplacementMode::Constant);
        assert_eq!(ReplacementMode::parse("clear").unwrap(), ReplacementMode::Clear);
    }

    #[test]
    fn replacement_mode_rejects_unknown_value() {
        assert!(ReplacementMode::parse("bogus").is_err());
    }

    #[test]
    fn replacement_mode_default_is_preserve_length() {
        assert_eq!(ReplacementMode::default(), ReplacementMode::PreserveLength);
    }

    #[test]
    fn replacement_mode_apply_dispatches_correctly() {
        assert_eq!(ReplacementMode::PreserveLength.apply("Hi"), "Xx");
        assert_eq!(ReplacementMode::Constant.apply("Hi"), "REDACTED");
        assert_eq!(ReplacementMode::Clear.apply("Hi"), "");
    }
}
