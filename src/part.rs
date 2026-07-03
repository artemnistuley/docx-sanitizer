use std::fmt;

use crate::Error;
use crate::relationships::Relationships;
use crate::zip::{FileRegistry, MAIN_DOCUMENT_PART};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartKind {
    MainDocument,
    Header(u32),
    Footer(u32),
    Comments,
    Footnotes,
    Endnotes,
    CoreProps,
    AppProps,
    CustomProps,
    CustomXml,
    Media,
    Embedding,
    Other,
}

impl fmt::Display for PartKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PartKind::MainDocument => write!(f, "MainDocument"),
            PartKind::Header(n) => write!(f, "Header({n})"),
            PartKind::Footer(n) => write!(f, "Footer({n})"),
            PartKind::Comments => write!(f, "Comments"),
            PartKind::Footnotes => write!(f, "Footnotes"),
            PartKind::Endnotes => write!(f, "Endnotes"),
            PartKind::CoreProps => write!(f, "CoreProps"),
            PartKind::AppProps => write!(f, "AppProps"),
            PartKind::CustomProps => write!(f, "CustomProps"),
            PartKind::CustomXml => write!(f, "CustomXml"),
            PartKind::Media => write!(f, "Media"),
            PartKind::Embedding => write!(f, "Embedding"),
            PartKind::Other => write!(f, "Other"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportTier {
    Guaranteed,
    BestEffort,
    Unsupported,
}

impl fmt::Display for SupportTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SupportTier::Guaranteed => write!(f, "guaranteed"),
            SupportTier::BestEffort => write!(f, "best-effort"),
            SupportTier::Unsupported => write!(f, "unsupported"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedPart {
    pub path: String,
    pub kind: PartKind,
    pub tier: SupportTier,
}

pub fn inspect_parts(files: &FileRegistry) -> Result<Vec<ClassifiedPart>, Error> {
    let relationships = Relationships::from_files(files)?;
    let main_document_path = relationships
        .main_document_path()
        .unwrap_or(MAIN_DOCUMENT_PART)
        .to_string();

    let header_index = index_by_path(relationships.find_header_parts());
    let footer_index = index_by_path(relationships.find_footer_parts());
    let comments_path = relationships.find_comments_part().map(str::to_string);
    let footnotes_path = relationships.find_footnotes_part().map(str::to_string);
    let endnotes_path = relationships.find_endnotes_part().map(str::to_string);

    let mut parts: Vec<ClassifiedPart> = files
        .keys()
        .map(|path| {
            let kind = classify_path(
                path,
                &main_document_path,
                &header_index,
                &footer_index,
                comments_path.as_deref(),
                footnotes_path.as_deref(),
                endnotes_path.as_deref(),
            );
            let tier = support_tier(&kind);
            ClassifiedPart {
                path: path.clone(),
                kind,
                tier,
            }
        })
        .collect();

    parts.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(parts)
}

fn index_by_path(paths: Vec<&str>) -> Vec<String> {
    paths.into_iter().map(str::to_string).collect()
}

#[allow(clippy::too_many_arguments)]
fn classify_path(
    path: &str,
    main_document_path: &str,
    header_paths: &[String],
    footer_paths: &[String],
    comments_path: Option<&str>,
    footnotes_path: Option<&str>,
    endnotes_path: Option<&str>,
) -> PartKind {
    if path == main_document_path {
        return PartKind::MainDocument;
    }
    if let Some(index) = header_paths.iter().position(|p| p == path) {
        return PartKind::Header(index as u32 + 1);
    }
    if let Some(index) = footer_paths.iter().position(|p| p == path) {
        return PartKind::Footer(index as u32 + 1);
    }
    if Some(path) == comments_path {
        return PartKind::Comments;
    }
    if Some(path) == footnotes_path {
        return PartKind::Footnotes;
    }
    if Some(path) == endnotes_path {
        return PartKind::Endnotes;
    }
    if path == "docProps/core.xml" {
        return PartKind::CoreProps;
    }
    if path == "docProps/app.xml" {
        return PartKind::AppProps;
    }
    if path == "docProps/custom.xml" {
        return PartKind::CustomProps;
    }
    if path.starts_with("word/customXml/") || path.starts_with("customXml/") {
        return PartKind::CustomXml;
    }
    if path.starts_with("word/media/") {
        return PartKind::Media;
    }
    if path.starts_with("word/embeddings/") {
        return PartKind::Embedding;
    }

    PartKind::Other
}

fn support_tier(kind: &PartKind) -> SupportTier {
    match kind {
        PartKind::MainDocument
        | PartKind::Header(_)
        | PartKind::Footer(_)
        | PartKind::Comments
        | PartKind::Footnotes
        | PartKind::Endnotes
        | PartKind::CoreProps
        | PartKind::AppProps
        | PartKind::CustomProps => SupportTier::Guaranteed,
        PartKind::CustomXml | PartKind::Media | PartKind::Embedding => SupportTier::Unsupported,
        PartKind::Other => SupportTier::BestEffort,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    use super::{PartKind, SupportTier, inspect_parts};
    use crate::zip::unpack_docx;

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

    #[test]
    fn classifies_plain_document() {
        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", b"<w:document/>"),
            ("docProps/core.xml", b"<cp:coreProperties/>"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();
        let parts = inspect_parts(&files).unwrap();

        let document = parts.iter().find(|p| p.path == "word/document.xml").unwrap();
        assert_eq!(document.kind, PartKind::MainDocument);
        assert_eq!(document.tier, SupportTier::Guaranteed);

        let core_props = parts.iter().find(|p| p.path == "docProps/core.xml").unwrap();
        assert_eq!(core_props.kind, PartKind::CoreProps);
        assert_eq!(core_props.tier, SupportTier::Guaranteed);
    }

    #[test]
    fn classifies_headers_and_footers() {
        let document_rels = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
              <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
            </Relationships>"#;
        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", b"<w:document/>"),
            ("word/header1.xml", b"<w:hdr/>"),
            ("word/footer1.xml", b"<w:ftr/>"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();
        let parts = inspect_parts(&files).unwrap();

        let header = parts.iter().find(|p| p.path == "word/header1.xml").unwrap();
        assert_eq!(header.kind, PartKind::Header(1));
        assert_eq!(header.tier, SupportTier::Guaranteed);

        let footer = parts.iter().find(|p| p.path == "word/footer1.xml").unwrap();
        assert_eq!(footer.kind, PartKind::Footer(1));
        assert_eq!(footer.tier, SupportTier::Guaranteed);
    }

    #[test]
    fn classifies_comments() {
        let document_rels = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
            </Relationships>"#;
        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", b"<w:document/>"),
            ("word/comments.xml", b"<w:comments/>"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();
        let parts = inspect_parts(&files).unwrap();

        let comments = parts.iter().find(|p| p.path == "word/comments.xml").unwrap();
        assert_eq!(comments.kind, PartKind::Comments);
        assert_eq!(comments.tier, SupportTier::Guaranteed);
    }

    #[test]
    fn classifies_footnotes_and_endnotes() {
        let document_rels = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>
              <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes" Target="endnotes.xml"/>
            </Relationships>"#;
        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/_rels/document.xml.rels", document_rels),
            ("word/document.xml", b"<w:document/>"),
            ("word/footnotes.xml", b"<w:footnotes/>"),
            ("word/endnotes.xml", b"<w:endnotes/>"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();
        let parts = inspect_parts(&files).unwrap();

        let footnotes = parts.iter().find(|p| p.path == "word/footnotes.xml").unwrap();
        assert_eq!(footnotes.kind, PartKind::Footnotes);
        assert_eq!(footnotes.tier, SupportTier::Guaranteed);

        let endnotes = parts.iter().find(|p| p.path == "word/endnotes.xml").unwrap();
        assert_eq!(endnotes.kind, PartKind::Endnotes);
        assert_eq!(endnotes.tier, SupportTier::Guaranteed);
    }

    #[test]
    fn classifies_custom_xml_and_media_as_unsupported() {
        let bytes = build_zip(&[
            ("_rels/.rels", PACKAGE_RELS),
            ("word/document.xml", b"<w:document/>"),
            ("word/customXml/item1.xml", b"<root/>"),
            ("word/media/image1.png", b"\x89PNG"),
            ("word/embeddings/oleObject1.bin", b"\x00\x01"),
        ]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();
        let parts = inspect_parts(&files).unwrap();

        let custom_xml = parts
            .iter()
            .find(|p| p.path == "word/customXml/item1.xml")
            .unwrap();
        assert_eq!(custom_xml.kind, PartKind::CustomXml);
        assert_eq!(custom_xml.tier, SupportTier::Unsupported);

        let media = parts.iter().find(|p| p.path == "word/media/image1.png").unwrap();
        assert_eq!(media.kind, PartKind::Media);
        assert_eq!(media.tier, SupportTier::Unsupported);

        let embedding = parts
            .iter()
            .find(|p| p.path == "word/embeddings/oleObject1.bin")
            .unwrap();
        assert_eq!(embedding.kind, PartKind::Embedding);
        assert_eq!(embedding.tier, SupportTier::Unsupported);
    }
}
