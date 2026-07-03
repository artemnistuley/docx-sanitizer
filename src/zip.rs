use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, Write};

use zip::write::SimpleFileOptions;

use crate::Error;

pub const MAIN_DOCUMENT_PART: &str = "word/document.xml";

pub type FileRegistry = HashMap<String, Vec<u8>>;

pub fn unpack_docx(input: impl Read + Seek) -> Result<FileRegistry, Error> {
    let mut archive = zip::ZipArchive::new(input)?;
    let mut files = HashMap::with_capacity(archive.len());

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;

        if entry.is_dir() {
            continue;
        }

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        files.insert(entry.name().to_string(), bytes);
    }

    Ok(files)
}

pub fn get_part<'a>(files: &'a FileRegistry, path: &str) -> Option<&'a [u8]> {
    files.get(path).map(Vec::as_slice)
}

pub fn require_part<'a>(files: &'a FileRegistry, path: &str) -> Result<&'a [u8], Error> {
    get_part(files, path).ok_or_else(|| Error::MissingPart(path.to_string()))
}

/// Repack `files` into a new zip, replacing the bytes of any path present in
/// `overrides` and copying everything else through unchanged. Only paths
/// already present in `files` are written; `overrides` entries for unknown
/// paths are ignored.
///
/// Entries are written in sorted path order so that identical input and
/// overrides always produce byte-identical output.
///
/// This targets byte-identical *part payload* for untouched parts, not a
/// byte-identical whole zip container (compression parameters, local header
/// ordering, and other ZIP metadata are free to differ from the source).
pub fn repack_docx(files: &FileRegistry, overrides: &FileRegistry) -> Result<Vec<u8>, Error> {
    let mut buffer = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buffer);
    let options = SimpleFileOptions::default();

    let mut paths: Vec<&String> = files.keys().collect();
    paths.sort();

    for path in paths {
        let bytes = overrides.get(path).unwrap_or(&files[path]);
        writer.start_file(path, options)?;
        writer.write_all(bytes)?;
    }

    writer.finish()?;
    Ok(buffer.into_inner())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    use std::collections::HashMap;

    use super::{MAIN_DOCUMENT_PART, get_part, repack_docx, require_part, unpack_docx};

    #[test]
    fn unpack_docx_reads_all_files() {
        let bytes = build_zip(&[
            (
                MAIN_DOCUMENT_PART,
                br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#,
            ),
            ("docProps/core.xml", br#"<cp:coreProperties/>"#),
        ]);

        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        assert_eq!(files.len(), 2);
        assert!(get_part(&files, MAIN_DOCUMENT_PART).is_some());
        assert!(get_part(&files, "docProps/core.xml").is_some());
    }

    #[test]
    fn require_part_returns_missing_part_error() {
        let bytes = build_zip(&[("docProps/core.xml", br#"<cp:coreProperties/>"#)]);
        let files = unpack_docx(Cursor::new(bytes)).unwrap();

        let error = require_part(&files, MAIN_DOCUMENT_PART).unwrap_err();

        assert!(matches!(error, crate::Error::MissingPart(path) if path == MAIN_DOCUMENT_PART));
    }

    #[test]
    fn repack_with_no_overrides_preserves_part_payload() {
        let bytes = build_zip(&[
            (MAIN_DOCUMENT_PART, br#"<w:document/>"#),
            ("docProps/core.xml", br#"<cp:coreProperties/>"#),
            ("word/media/image1.png", b"\x89PNG\x00\x01\x02"),
        ]);
        let original = unpack_docx(Cursor::new(bytes)).unwrap();

        let repacked_bytes = repack_docx(&original, &HashMap::new()).unwrap();
        let repacked = unpack_docx(Cursor::new(repacked_bytes)).unwrap();

        assert_eq!(repacked, original);
    }

    #[test]
    fn repack_produces_deterministic_entry_order() {
        let bytes = build_zip(&[
            ("word/document.xml", br#"<w:document/>"#),
            ("docProps/core.xml", br#"<cp:coreProperties/>"#),
            ("word/media/image1.png", b"\x89PNG"),
            ("word/comments.xml", br#"<w:comments/>"#),
        ]);
        let original = unpack_docx(Cursor::new(bytes)).unwrap();

        let first = repack_docx(&original, &HashMap::new()).unwrap();
        let second = repack_docx(&original, &HashMap::new()).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn repack_overrides_only_the_specified_part() {
        let bytes = build_zip(&[
            (MAIN_DOCUMENT_PART, br#"<w:document/>"#),
            ("docProps/core.xml", br#"<cp:coreProperties/>"#),
        ]);
        let original = unpack_docx(Cursor::new(bytes)).unwrap();

        let mut overrides = HashMap::new();
        overrides.insert(MAIN_DOCUMENT_PART.to_string(), b"<w:document>sanitized</w:document>".to_vec());
        // Override for a path that isn't in `files` must be ignored.
        overrides.insert("word/unknown.xml".to_string(), b"ignored".to_vec());

        let repacked_bytes = repack_docx(&original, &overrides).unwrap();
        let repacked = unpack_docx(Cursor::new(repacked_bytes)).unwrap();

        assert_eq!(repacked.len(), 2);
        assert_eq!(
            get_part(&repacked, MAIN_DOCUMENT_PART).unwrap(),
            b"<w:document>sanitized</w:document>".as_slice()
        );
        assert_eq!(
            get_part(&repacked, "docProps/core.xml").unwrap(),
            get_part(&original, "docProps/core.xml").unwrap()
        );
    }

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
}
