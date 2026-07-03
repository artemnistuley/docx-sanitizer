use quick_xml::Reader;
use quick_xml::events::Event;

use crate::Error;
use crate::zip::{FileRegistry, MAIN_DOCUMENT_PART, get_part};

const PACKAGE_RELS_PATH: &str = "_rels/.rels";

pub const REL_TYPE_COMMENTS: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
pub const REL_TYPE_COMMENTS_EXTENDED: &str =
    "http://schemas.microsoft.com/office/2011/relationships/commentsExtended";
pub const REL_TYPE_COMMENTS_IDS: &str =
    "http://schemas.microsoft.com/office/2016/09/relationships/commentsIds";
pub const REL_TYPE_FOOTNOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
pub const REL_TYPE_ENDNOTES: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes";
pub const REL_TYPE_HEADER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/header";
pub const REL_TYPE_FOOTER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer";
pub const REL_TYPE_HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
pub const REL_TYPE_OFFICE_DOCUMENT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relationship {
    pub id: String,
    pub rel_type: String,
    pub target: String,
    pub target_mode: Option<String>,
    pub resolved_path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Relationships {
    package: Vec<Relationship>,
    document: Vec<Relationship>,
}

impl Relationships {
    pub fn from_files(files: &FileRegistry) -> Result<Self, Error> {
        let package = parse_relationship_part(get_part(files, PACKAGE_RELS_PATH), PACKAGE_RELS_PATH)?;
        let main_document_path = package
            .iter()
            .find(|rel| rel.rel_type == REL_TYPE_OFFICE_DOCUMENT)
            .map(|rel| rel.resolved_path.as_str())
            .unwrap_or(MAIN_DOCUMENT_PART);
        let document_rels_path = relationship_part_path(main_document_path);
        let document =
            parse_relationship_part(get_part(files, &document_rels_path), &document_rels_path)?;

        Ok(Self { package, document })
    }

    pub fn for_part(files: &FileRegistry, part_path: &str) -> Result<Self, Error> {
        let rels_path = relationship_part_path(part_path);
        let document = parse_relationship_part(get_part(files, &rels_path), &rels_path)?;

        Ok(Self {
            package: Vec::new(),
            document,
        })
    }

    pub fn package(&self) -> &[Relationship] {
        &self.package
    }

    pub fn document(&self) -> &[Relationship] {
        &self.document
    }

    pub fn find_package_by_type(&self, rel_type: &str) -> Vec<&Relationship> {
        self.package
            .iter()
            .filter(|rel| rel.rel_type == rel_type)
            .collect()
    }

    pub fn find_document_by_id(&self, id: &str) -> Option<&Relationship> {
        self.document.iter().find(|rel| rel.id == id)
    }

    pub fn find_document_by_type(&self, rel_type: &str) -> Vec<&Relationship> {
        self.document
            .iter()
            .filter(|rel| rel.rel_type == rel_type)
            .collect()
    }

    pub fn main_document_path(&self) -> Option<&str> {
        self.package
            .iter()
            .find(|rel| rel.rel_type == REL_TYPE_OFFICE_DOCUMENT)
            .map(|rel| rel.resolved_path.as_str())
    }

    pub fn find_comments_part(&self) -> Option<&str> {
        find_single_path(self.find_document_by_type(REL_TYPE_COMMENTS))
    }

    pub fn find_comments_extended_part(&self) -> Option<&str> {
        find_single_path(self.find_document_by_type(REL_TYPE_COMMENTS_EXTENDED))
    }

    pub fn find_comments_ids_part(&self) -> Option<&str> {
        find_single_path(self.find_document_by_type(REL_TYPE_COMMENTS_IDS))
    }

    pub fn find_footnotes_part(&self) -> Option<&str> {
        find_single_path(self.find_document_by_type(REL_TYPE_FOOTNOTES))
    }

    pub fn find_endnotes_part(&self) -> Option<&str> {
        find_single_path(self.find_document_by_type(REL_TYPE_ENDNOTES))
    }

    pub fn find_header_parts(&self) -> Vec<&str> {
        self.find_document_by_type(REL_TYPE_HEADER)
            .into_iter()
            .map(|rel| rel.resolved_path.as_str())
            .collect()
    }

    pub fn find_footer_parts(&self) -> Vec<&str> {
        self.find_document_by_type(REL_TYPE_FOOTER)
            .into_iter()
            .map(|rel| rel.resolved_path.as_str())
            .collect()
    }

    pub fn hyperlink_target(&self, id: &str) -> Option<&str> {
        self.find_document_by_id(id)
            .filter(|rel| rel.rel_type == REL_TYPE_HYPERLINK)
            .map(|rel| {
                if rel.target_mode.as_deref() == Some("External") {
                    rel.target.as_str()
                } else {
                    rel.resolved_path.as_str()
                }
            })
    }
}

fn find_single_path(relationships: Vec<&Relationship>) -> Option<&str> {
    relationships
        .into_iter()
        .next()
        .map(|rel| rel.resolved_path.as_str())
}

fn relationship_part_path(part_path: &str) -> String {
    if let Some((dir, file_name)) = part_path.rsplit_once('/') {
        format!("{dir}/_rels/{file_name}.rels")
    } else {
        format!("_rels/{part_path}.rels")
    }
}

fn parse_relationship_part(part: Option<&[u8]>, path: &str) -> Result<Vec<Relationship>, Error> {
    let Some(part) = part else {
        return Ok(Vec::new());
    };

    parse_relationships_xml(part, path)
}

fn parse_relationships_xml(xml: &[u8], source_path: &str) -> Result<Vec<Relationship>, Error> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut relationships = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(ref event) | Event::Empty(ref event)
                if event.local_name().as_ref() == b"Relationship" =>
            {
                let mut id = None;
                let mut rel_type = None;
                let mut target = None;
                let mut target_mode = None;

                for attribute in event.attributes().flatten() {
                    match attribute.key.local_name().as_ref() {
                        b"Id" => id = Some(decode_attr(&attribute.value)?),
                        b"Type" => rel_type = Some(decode_attr(&attribute.value)?),
                        b"Target" => target = Some(decode_attr(&attribute.value)?),
                        b"TargetMode" => target_mode = Some(decode_attr(&attribute.value)?),
                        _ => {}
                    }
                }

                if let (Some(id), Some(rel_type), Some(target)) = (id, rel_type, target) {
                    let resolved_path = resolve_relationship_target(source_path, &target);
                    relationships.push(Relationship {
                        id,
                        rel_type,
                        target,
                        target_mode,
                        resolved_path,
                    });
                }
            }
            Event::Eof => break,
            _ => {}
        }

        buf.clear();
    }

    Ok(relationships)
}

fn decode_attr(value: &[u8]) -> Result<String, Error> {
    let lossy = String::from_utf8_lossy(value);
    let unescaped = quick_xml::escape::unescape(&lossy)?;
    Ok(unescaped.into_owned())
}

fn resolve_relationship_target(source_path: &str, target: &str) -> String {
    if target.starts_with('/') {
        return target.trim_start_matches('/').to_string();
    }

    let base_dir = relationship_source_directory(source_path);
    normalize_path(&format!("{base_dir}/{target}"))
}

fn relationship_source_directory(source_path: &str) -> &str {
    if let Some(parent) = source_path.rsplit_once('/') {
        let dir = parent.0;
        if dir == "_rels" {
            return "";
        }
        if let Some(stripped) = dir.strip_suffix("/_rels") {
            return stripped;
        }
        return dir;
    }

    ""
}

fn normalize_path(path: &str) -> String {
    let mut normalized = Vec::new();

    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                normalized.pop();
            }
            _ => normalized.push(segment),
        }
    }

    normalized.join("/")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        REL_TYPE_COMMENTS, REL_TYPE_COMMENTS_EXTENDED, REL_TYPE_COMMENTS_IDS, REL_TYPE_FOOTER,
        REL_TYPE_HEADER, REL_TYPE_HYPERLINK, REL_TYPE_OFFICE_DOCUMENT, Relationships,
        normalize_path, parse_relationships_xml, relationship_part_path,
        resolve_relationship_target,
    };

    #[test]
    fn resolves_document_relationship_targets() {
        assert_eq!(
            resolve_relationship_target("word/_rels/document.xml.rels", "comments.xml"),
            "word/comments.xml"
        );
        assert_eq!(
            resolve_relationship_target("word/_rels/document.xml.rels", "../custom/comments.xml"),
            "custom/comments.xml"
        );
        assert_eq!(
            resolve_relationship_target("word/_rels/document.xml.rels", "/word/header1.xml"),
            "word/header1.xml"
        );
        assert_eq!(
            relationship_part_path("word/header1.xml"),
            "word/_rels/header1.xml.rels"
        );
        assert_eq!(
            relationship_part_path("word/footer2.xml"),
            "word/_rels/footer2.xml.rels"
        );
    }

    #[test]
    fn parses_and_finds_relationship_parts() {
        let mut files = HashMap::new();
        files.insert(
            "_rels/.rels".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rPkg1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
            </Relationships>"#
                .to_vec(),
        );
        files.insert(
            "word/_rels/document.xml.rels".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
              <Relationship Id="rId2" Type="http://schemas.microsoft.com/office/2011/relationships/commentsExtended" Target="commentsExtended.xml"/>
              <Relationship Id="rId3" Type="http://schemas.microsoft.com/office/2016/09/relationships/commentsIds" Target="commentsIds.xml"/>
              <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
              <Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header2.xml"/>
              <Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/>
              <Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/>
            </Relationships>"#
                .to_vec(),
        );

        let relationships = Relationships::from_files(&files).unwrap();

        assert_eq!(relationships.main_document_path(), Some("word/document.xml"));
        assert_eq!(relationships.find_comments_part(), Some("word/comments.xml"));
        assert_eq!(
            relationships.find_comments_extended_part(),
            Some("word/commentsExtended.xml")
        );
        assert_eq!(
            relationships.find_comments_ids_part(),
            Some("word/commentsIds.xml")
        );
        assert_eq!(
            relationships.find_header_parts(),
            vec!["word/header1.xml", "word/header2.xml"]
        );
        assert_eq!(relationships.find_footer_parts(), vec!["word/footer1.xml"]);
        assert_eq!(relationships.hyperlink_target("rId7"), Some("https://example.com"));
        assert_eq!(
            relationships.find_document_by_id("rId4").unwrap().resolved_path,
            "word/header1.xml"
        );
    }

    #[test]
    fn parses_multiple_relationships_of_same_type() {
        let rels = parse_relationships_xml(
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
              <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header2.xml"/>
            </Relationships>"#,
            "word/_rels/document.xml.rels",
        )
        .unwrap();

        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0].resolved_path, "word/header1.xml");
        assert_eq!(rels[1].resolved_path, "word/header2.xml");
    }

    #[test]
    fn resolves_document_rels_from_nonstandard_main_document_path() {
        let mut files = HashMap::new();
        files.insert(
            "_rels/.rels".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rPkg1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="content/main.xml"/>
            </Relationships>"#
                .to_vec(),
        );
        files.insert(
            "content/_rels/main.xml.rels".to_string(),
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
            </Relationships>"#
                .to_vec(),
        );

        let relationships = Relationships::from_files(&files).unwrap();

        assert_eq!(relationships.main_document_path(), Some("content/main.xml"));
        assert_eq!(relationships.find_comments_part(), Some("content/comments.xml"));
    }

    #[test]
    fn unescapes_xml_entities_in_relationship_target() {
        let rels = parse_relationships_xml(
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com?a=1&amp;b=2" TargetMode="External"/>
            </Relationships>"#,
            "word/_rels/document.xml.rels",
        )
        .unwrap();

        assert_eq!(rels[0].target, "https://example.com?a=1&b=2");
    }

    #[test]
    fn tolerates_non_utf8_bytes_in_relationship_attributes() {
        let mut xml = br#"<?xml version="1.0" encoding="UTF-8"?>
            <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
              <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target=""#
            .to_vec();
        xml.extend_from_slice(b"bad-\xFF-target");
        xml.extend_from_slice(br#"" TargetMode="External"/></Relationships>"#);

        let rels = parse_relationships_xml(&xml, "word/_rels/document.xml.rels").unwrap();

        assert_eq!(rels.len(), 1);
        assert!(rels[0].target.starts_with("bad-"));
    }

    #[test]
    fn normalizes_relative_paths() {
        assert_eq!(normalize_path("word/./comments.xml"), "word/comments.xml");
        assert_eq!(normalize_path("word/../docProps/core.xml"), "docProps/core.xml");
    }

    #[test]
    fn relationship_type_constants_cover_expected_lookups() {
        let known = [
            REL_TYPE_COMMENTS,
            REL_TYPE_COMMENTS_EXTENDED,
            REL_TYPE_COMMENTS_IDS,
            REL_TYPE_FOOTER,
            REL_TYPE_HEADER,
            REL_TYPE_HYPERLINK,
            REL_TYPE_OFFICE_DOCUMENT,
        ];

        assert!(known.iter().all(|value| !value.is_empty()));
    }
}
