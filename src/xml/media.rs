//! `--strip-media` placeholder bytes for `word/media/*` parts.
//!
//! Deliberately narrow: only raster formats where a trivially valid,
//! minimal (1x1 pixel) file is well-defined. Vector/metafile formats
//! (`.emf`, `.wmf`, `.svg`) and anything else are not covered -- those
//! parts remain unsupported (`--strip-media` does not change their
//! strict-mode blocking), rather than risk emitting a placeholder that
//! doesn't parse as the claimed format.
//!
//! This only replaces payload *bytes* for a part that continues to exist
//! at the same path with the same relationship/content-type -- unlike
//! removing the part entirely, no `.rels`/`[Content_Types].xml` cleanup is
//! needed. See DESIGN.md's "Images and Embeddings" for why placeholder
//! (payload replacement) was chosen over removal (a structural change).

const PLACEHOLDER_PNG: &[u8] = include_bytes!("../../assets/placeholder/1x1.png");
const PLACEHOLDER_JPEG: &[u8] = include_bytes!("../../assets/placeholder/1x1.jpg");
const PLACEHOLDER_GIF: &[u8] = include_bytes!("../../assets/placeholder/1x1.gif");
const PLACEHOLDER_BMP: &[u8] = include_bytes!("../../assets/placeholder/1x1.bmp");

/// The placeholder bytes to substitute for `path`'s media content, based on
/// its file extension. `None` means no placeholder is available for this
/// extension -- the part remains unsupported.
pub fn placeholder_bytes_for(path: &str) -> Option<&'static [u8]> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(PLACEHOLDER_PNG),
        "jpg" | "jpeg" => Some(PLACEHOLDER_JPEG),
        "gif" => Some(PLACEHOLDER_GIF),
        "bmp" => Some(PLACEHOLDER_BMP),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{PLACEHOLDER_BMP, PLACEHOLDER_GIF, PLACEHOLDER_JPEG, PLACEHOLDER_PNG, placeholder_bytes_for};

    #[test]
    fn returns_matching_placeholder_per_extension() {
        assert_eq!(placeholder_bytes_for("word/media/image1.png"), Some(PLACEHOLDER_PNG));
        assert_eq!(placeholder_bytes_for("word/media/image1.jpg"), Some(PLACEHOLDER_JPEG));
        assert_eq!(placeholder_bytes_for("word/media/image1.jpeg"), Some(PLACEHOLDER_JPEG));
        assert_eq!(placeholder_bytes_for("word/media/image1.gif"), Some(PLACEHOLDER_GIF));
        assert_eq!(placeholder_bytes_for("word/media/image1.bmp"), Some(PLACEHOLDER_BMP));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(placeholder_bytes_for("word/media/image1.PNG"), Some(PLACEHOLDER_PNG));
        assert_eq!(placeholder_bytes_for("word/media/image1.JPG"), Some(PLACEHOLDER_JPEG));
    }

    #[test]
    fn unsupported_extensions_return_none() {
        assert_eq!(placeholder_bytes_for("word/media/image1.emf"), None);
        assert_eq!(placeholder_bytes_for("word/media/image1.wmf"), None);
        assert_eq!(placeholder_bytes_for("word/media/image1.tiff"), None);
        assert_eq!(placeholder_bytes_for("word/media/image1.svg"), None);
    }

    #[test]
    fn path_without_extension_returns_none() {
        assert_eq!(placeholder_bytes_for("word/media/image1"), None);
    }

    #[test]
    fn placeholder_bytes_have_correct_magic_numbers() {
        assert_eq!(&PLACEHOLDER_PNG[..8], &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&PLACEHOLDER_JPEG[..3], &[0xFF, 0xD8, 0xFF]);
        assert_eq!(&PLACEHOLDER_GIF[..3], b"GIF");
        assert_eq!(&PLACEHOLDER_BMP[..2], b"BM");
    }
}
