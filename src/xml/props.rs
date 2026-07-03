//! Sanitization for `docProps/core.xml`, `docProps/app.xml`, and
//! `docProps/custom.xml`.
//!
//! These parts aren't WordprocessingML-namespaced body XML, so they don't
//! go through [`crate::sanitize::sanitize_body_text_xml`] -- each uses its
//! own OOXML schema/namespace (Dublin Core + core-properties for
//! `core.xml`; extended-properties for `app.xml`; custom-properties + the
//! shared variant-types namespace for `custom.xml`). Replacement follows
//! DESIGN.md's Recommended defaults: document property values get a fixed
//! canonical placeholder (not preserve-length -- unlike body text, a
//! property's *length* is not treated as structurally meaningful), and
//! timestamp-valued properties get the same fixed canonical timestamp used
//! for revision metadata.

use crate::Error;
use crate::xml::rewrite::rewrite_text_elements;
use crate::xml::text::CANONICAL_TIMESTAMP;

/// Fixed canonical placeholder substituted for sanitized document property
/// values (per DESIGN.md's Replacement Strategies: "document properties:
/// canonical placeholders").
pub const CANONICAL_PLACEHOLDER: &str = "Redacted";

const CORE_PROPERTIES_NS: &[u8] =
    b"http://schemas.openxmlformats.org/package/2006/metadata/core-properties";
const DC_NS: &[u8] = b"http://purl.org/dc/elements/1.1/";
const DCTERMS_NS: &[u8] = b"http://purl.org/dc/terms/";
const EXTENDED_PROPERTIES_NS: &[u8] =
    b"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties";
const VT_NS: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes";

/// Sanitize `docProps/core.xml`: Dublin Core / core-properties text fields
/// (`dc:title`, `dc:subject`, `dc:creator`, `cp:keywords`, `dc:description`,
/// `cp:lastModifiedBy`, `cp:category`, `cp:contentStatus`) get the canonical
/// placeholder; `dcterms:created`/`dcterms:modified` get the canonical
/// timestamp. `cp:revision` (a plain revision counter, not user-controlled
/// payload) is left untouched.
pub fn sanitize_core_props_xml(xml: &[u8]) -> Result<Vec<u8>, Error> {
    let xml = rewrite_text_elements(xml, DC_NS, b"title", placeholder)?;
    let xml = rewrite_text_elements(&xml, DC_NS, b"subject", placeholder)?;
    let xml = rewrite_text_elements(&xml, DC_NS, b"creator", placeholder)?;
    let xml = rewrite_text_elements(&xml, DC_NS, b"description", placeholder)?;
    let xml = rewrite_text_elements(&xml, CORE_PROPERTIES_NS, b"keywords", placeholder)?;
    let xml = rewrite_text_elements(&xml, CORE_PROPERTIES_NS, b"lastModifiedBy", placeholder)?;
    let xml = rewrite_text_elements(&xml, CORE_PROPERTIES_NS, b"category", placeholder)?;
    let xml = rewrite_text_elements(&xml, CORE_PROPERTIES_NS, b"contentStatus", placeholder)?;
    let xml = rewrite_text_elements(&xml, DCTERMS_NS, b"created", canonical_timestamp)?;
    rewrite_text_elements(&xml, DCTERMS_NS, b"modified", canonical_timestamp)
}

/// Sanitize `docProps/app.xml`: the extended-properties fields most likely
/// to carry confidential business data (`Company`, `Manager`,
/// `HyperlinkBase`) get the canonical placeholder. Other extended-properties
/// fields (page/word/character counts, application name, template) are
/// software/document statistics, not user-controlled payload, and are left
/// untouched.
pub fn sanitize_app_props_xml(xml: &[u8]) -> Result<Vec<u8>, Error> {
    let xml = rewrite_text_elements(xml, EXTENDED_PROPERTIES_NS, b"Company", placeholder)?;
    let xml = rewrite_text_elements(&xml, EXTENDED_PROPERTIES_NS, b"Manager", placeholder)?;
    rewrite_text_elements(&xml, EXTENDED_PROPERTIES_NS, b"HyperlinkBase", placeholder)
}

/// Sanitize `docProps/custom.xml`: string-typed variant values
/// (`vt:lpwstr`, `vt:lpstr`, `vt:bstr`) get the canonical placeholder;
/// timestamp-typed variant values (`vt:filetime`, `vt:date`) get the
/// canonical timestamp -- custom properties can store a date as freely as a
/// string (e.g. a "ContractSignedDate" property), and DESIGN.md's threat
/// model treats timestamps as sensitive payload regardless of which part
/// carries them. Property names (the `name` attribute on each `property`
/// element) are labels, not payload, and are left untouched -- consistent
/// with how revision metadata rewriting leaves attribute *names* alone and
/// only replaces values.
pub fn sanitize_custom_props_xml(xml: &[u8]) -> Result<Vec<u8>, Error> {
    let xml = rewrite_text_elements(xml, VT_NS, b"lpwstr", placeholder)?;
    let xml = rewrite_text_elements(&xml, VT_NS, b"lpstr", placeholder)?;
    let xml = rewrite_text_elements(&xml, VT_NS, b"bstr", placeholder)?;
    let xml = rewrite_text_elements(&xml, VT_NS, b"filetime", canonical_timestamp)?;
    rewrite_text_elements(&xml, VT_NS, b"date", canonical_timestamp)
}

fn placeholder(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        CANONICAL_PLACEHOLDER.to_string()
    }
}

fn canonical_timestamp(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        CANONICAL_TIMESTAMP.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_app_props_xml, sanitize_core_props_xml, sanitize_custom_props_xml};

    #[test]
    fn sanitizes_core_properties_text_fields_and_timestamps() {
        let input = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
            <cp:coreProperties
                xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                xmlns:dc="http://purl.org/dc/elements/1.1/"
                xmlns:dcterms="http://purl.org/dc/terms/"
                xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
              <dc:title>Q3 Acquisition Plan</dc:title>
              <dc:creator>Jane Doe</dc:creator>
              <cp:lastModifiedBy>Jane Doe</cp:lastModifiedBy>
              <cp:keywords>merger,confidential</cp:keywords>
              <dcterms:created xsi:type="dcterms:W3CDTF">2026-06-11T09:15:00Z</dcterms:created>
              <dcterms:modified xsi:type="dcterms:W3CDTF">2026-06-11T10:30:00Z</dcterms:modified>
              <cp:revision>9</cp:revision>
            </cp:coreProperties>"#;

        let output = sanitize_core_props_xml(input).unwrap();
        let output_str = std::str::from_utf8(&output).unwrap();

        assert!(!output_str.contains("Q3 Acquisition"));
        assert!(!output_str.contains("Jane Doe"));
        assert!(!output_str.contains("merger"));
        assert!(output_str.contains("<dc:title>Redacted</dc:title>"));
        assert!(output_str.contains("<dc:creator>Redacted</dc:creator>"));
        assert!(output_str.contains("<cp:lastModifiedBy>Redacted</cp:lastModifiedBy>"));
        assert!(output_str.contains("<cp:keywords>Redacted</cp:keywords>"));
        assert!(output_str.contains(
            r#"<dcterms:created xsi:type="dcterms:W3CDTF">2000-01-01T00:00:00Z</dcterms:created>"#
        ));
        assert!(output_str.contains(
            r#"<dcterms:modified xsi:type="dcterms:W3CDTF">2000-01-01T00:00:00Z</dcterms:modified>"#
        ));
        // Revision counter is not payload -- left untouched.
        assert!(output_str.contains("<cp:revision>9</cp:revision>"));
    }

    #[test]
    fn leaves_empty_core_property_elements_empty() {
        let input = br#"<?xml version="1.0" encoding="UTF-8"?>
            <cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
                xmlns:dc="http://purl.org/dc/elements/1.1/">
              <dc:title></dc:title>
            </cp:coreProperties>"#;

        let output = sanitize_core_props_xml(input).unwrap();
        assert!(std::str::from_utf8(&output).unwrap().contains("<dc:title></dc:title>"));
    }

    #[test]
    fn sanitizes_app_properties_company_manager_and_hyperlink_base() {
        let input = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"
                xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">
              <Application>Microsoft Office Word</Application>
              <Company>Acme Corp</Company>
              <Manager>Jane Doe</Manager>
              <HyperlinkBase>https://internal.acme.example/wiki</HyperlinkBase>
              <Pages>3</Pages>
            </Properties>"#;

        let output = sanitize_app_props_xml(input).unwrap();
        let output_str = std::str::from_utf8(&output).unwrap();

        assert!(!output_str.contains("Acme Corp"));
        assert!(!output_str.contains("Jane Doe"));
        assert!(!output_str.contains("internal.acme.example"));
        assert!(output_str.contains("<Company>Redacted</Company>"));
        assert!(output_str.contains("<Manager>Redacted</Manager>"));
        assert!(output_str.contains("<HyperlinkBase>Redacted</HyperlinkBase>"));
        // Non-sensitive document statistics are left untouched.
        assert!(output_str.contains("<Application>Microsoft Office Word</Application>"));
        assert!(output_str.contains("<Pages>3</Pages>"));
    }

    #[test]
    fn sanitizes_custom_property_string_values_but_not_names() {
        let input = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties"
                xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">
              <property fmtid="{D5CDD505-2E9C-101B-9397-08002B2CF9AE}" pid="2" name="ClientAccountNumber">
                <vt:lpwstr>ACC-90210</vt:lpwstr>
              </property>
              <property fmtid="{D5CDD505-2E9C-101B-9397-08002B2CF9AE}" pid="3" name="Reviewed">
                <vt:bool>1</vt:bool>
              </property>
            </Properties>"#;

        let output = sanitize_custom_props_xml(input).unwrap();
        let output_str = std::str::from_utf8(&output).unwrap();

        assert!(!output_str.contains("ACC-90210"));
        assert!(output_str.contains("<vt:lpwstr>Redacted</vt:lpwstr>"));
        // Property name (a label, not payload) and non-string-typed values
        // are left untouched.
        assert!(output_str.contains(r#"name="ClientAccountNumber""#));
        assert!(output_str.contains("<vt:bool>1</vt:bool>"));
    }

    #[test]
    fn sanitizes_custom_property_filetime_and_date_values() {
        let input = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties"
                xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">
              <property fmtid="{D5CDD505-2E9C-101B-9397-08002B2CF9AE}" pid="2" name="ContractSignedDate">
                <vt:filetime>2026-03-14T00:00:00Z</vt:filetime>
              </property>
              <property fmtid="{D5CDD505-2E9C-101B-9397-08002B2CF9AE}" pid="3" name="ReviewDate">
                <vt:date>2026-04-01T00:00:00Z</vt:date>
              </property>
            </Properties>"#;

        let output = sanitize_custom_props_xml(input).unwrap();
        let output_str = std::str::from_utf8(&output).unwrap();

        assert!(!output_str.contains("2026-03-14"));
        assert!(!output_str.contains("2026-04-01"));
        assert!(output_str.contains("<vt:filetime>2000-01-01T00:00:00Z</vt:filetime>"));
        assert!(output_str.contains("<vt:date>2000-01-01T00:00:00Z</vt:date>"));
        assert!(output_str.contains(r#"name="ContractSignedDate""#));
    }
}
