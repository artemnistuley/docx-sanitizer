#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("xml error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("xml escape error: {0}")]
    XmlEscape(#[from] quick_xml::escape::EscapeError),
    #[error("missing required part: {0}")]
    MissingPart(String),
    #[error("unsupported document feature: {0}")]
    Unsupported(&'static str),
    #[error(
        "unknown scope keyword: {0} (expected one of: headers, footers, comments, footnotes, endnotes, docprops, revisions)"
    )]
    InvalidScope(String),
    #[error("unknown replacement mode: {0} (expected one of: preserve-length, constant, clear)")]
    InvalidReplacementMode(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
