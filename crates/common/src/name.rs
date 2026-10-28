//! File, folder and library names: restrictive enough that every file can be
//! downloaded on every platform (Windows sets most limits).
//!
//! Case-insensitive uniqueness is checked by the database only (`lower()` with
//! the `C.UTF-8` collation), so there is a single definition of "same name".

use std::fmt;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// Maximum name length, in bytes of UTF-8 after normalization.
pub const MAX_LEN: usize = 255;

/// A valid name, normalized to NFC.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

/// Why a name is invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Empty name.
    #[error("the name is empty")]
    Empty,
    /// `.` or `..`.
    #[error("\".\" and \"..\" are not valid names")]
    Dots,
    /// One of `< > : " / \ | ? *`.
    #[error("the character {0:?} is not allowed")]
    ForbiddenChar(char),
    /// A control character (U+0000 to U+001F).
    #[error("control characters are not allowed")]
    ControlChar,
    /// A bidirectional text control character (spoofing risk).
    #[error("text direction characters are not allowed")]
    BidiControl,
    /// Ends with a space or a dot (Windows strips them).
    #[error("a name cannot end with a space or a dot")]
    TrailingSpaceOrDot,
    /// A reserved device name on Windows, with or without extension.
    #[error("{0:?} is a reserved name on Windows")]
    WindowsReserved(String),
    /// Longer than [`MAX_LEN`] bytes of UTF-8.
    #[error("the name is {0} bytes long, the maximum is {MAX_LEN}")]
    TooLong(usize),
}

const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

impl Name {
    /// Normalizes `raw` to NFC and validates it.
    pub fn new(raw: &str) -> Result<Self, NameError> {
        let name: String = raw.nfc().collect();
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if name == "." || name == ".." {
            return Err(NameError::Dots);
        }
        for c in name.chars() {
            if FORBIDDEN.contains(&c) {
                return Err(NameError::ForbiddenChar(c));
            }
            if c < ' ' {
                return Err(NameError::ControlChar);
            }
            if matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}') {
                return Err(NameError::BidiControl);
            }
        }
        if name.ends_with([' ', '.']) {
            return Err(NameError::TrailingSpaceOrDot);
        }
        // Reserved with or without extension: "con" and "con.txt" alike.
        let base = name.split('.').next().unwrap_or_default();
        if WINDOWS_RESERVED
            .iter()
            .any(|r| r.eq_ignore_ascii_case(base))
        {
            return Err(NameError::WindowsReserved(base.to_owned()));
        }
        if name.len() > MAX_LEN {
            return Err(NameError::TooLong(name.len()));
        }
        Ok(Self(name))
    }

    /// The normalized name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A file the operating system leaves beside the user's (macOS's `._*` and
/// `.DS_Store`, Windows's `Thumbs.db`, `ehthumbs.db`, `desktop.ini`): never
/// stored by Theca's clients, nor through WebDAV. Compared without case.
pub fn is_ignored(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("._")
        || matches!(
            lower.as_str(),
            ".ds_store" | "thumbs.db" | "ehthumbs.db" | "desktop.ini"
        )
}

impl TryFrom<String> for Name {
    type Error = NameError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(raw: &str) -> NameError {
        Name::new(raw).unwrap_err()
    }

    #[test]
    fn valid_names() {
        for raw in [
            "report.pdf",
            "Photos 2026",
            ".hidden",
            "été",
            "日本語.txt",
            "a.b.c",
            "COM10",
            "console",
        ] {
            assert_eq!(Name::new(raw).unwrap().as_str(), raw);
        }
    }

    #[test]
    fn normalizes_to_nfc() {
        let nfd = "e\u{0301}t\u{0065}\u{0301}"; // "été" as macOS writes it
        assert_eq!(Name::new(nfd).unwrap().as_str(), "\u{e9}t\u{e9}");
    }

    #[test]
    fn rejected_names() {
        assert_eq!(err(""), NameError::Empty);
        assert_eq!(err("."), NameError::Dots);
        assert_eq!(err(".."), NameError::Dots);
        for c in FORBIDDEN {
            assert_eq!(err(&format!("a{c}b")), NameError::ForbiddenChar(*c));
        }
        assert_eq!(err("a\tb"), NameError::ControlChar);
        assert_eq!(err("a\u{0}b"), NameError::ControlChar);
        assert_eq!(err("photo\u{202E}gpj.exe"), NameError::BidiControl);
        assert_eq!(err("a\u{2066}b"), NameError::BidiControl);
        assert_eq!(err("name "), NameError::TrailingSpaceOrDot);
        assert_eq!(err("name."), NameError::TrailingSpaceOrDot);
    }

    #[test]
    fn windows_reserved_names() {
        for raw in [
            "CON",
            "con",
            "Con.txt",
            "nul.tar.gz",
            "COM1",
            "lpt9.log",
            "aux",
        ] {
            assert!(matches!(err(raw), NameError::WindowsReserved(_)), "{raw}");
        }
    }

    #[test]
    fn length_counts_utf8_bytes() {
        assert!(Name::new(&"a".repeat(255)).is_ok());
        assert_eq!(err(&"a".repeat(256)), NameError::TooLong(256));
        // "é" is 2 bytes: 127 fit (254 bytes), 128 don't (256 bytes).
        assert!(Name::new(&"é".repeat(127)).is_ok());
        assert_eq!(err(&"é".repeat(128)), NameError::TooLong(256));
    }

    #[test]
    fn serde_validates() {
        assert_eq!(
            serde_json::from_str::<Name>("\"a.txt\"").unwrap().as_str(),
            "a.txt"
        );
        assert!(serde_json::from_str::<Name>("\"a/b\"").is_err());
    }

    #[test]
    fn ignored_names() {
        for name in [
            "._a.txt",
            "._",
            ".DS_Store",
            ".ds_store",
            "Thumbs.db",
            "THUMBS.DB",
            "ehthumbs.db",
            "desktop.ini",
            "Desktop.ini",
        ] {
            assert!(is_ignored(name), "{name}");
        }
        for name in [
            "a._b.txt",
            "x.DS_Store",
            "Thumbs.db.txt",
            "_.a",
            ".hidden",
            "~$report.docx",
        ] {
            assert!(!is_ignored(name), "{name}");
        }
    }
}
