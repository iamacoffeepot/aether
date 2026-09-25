//! The name of one entry in a directory. Valid by construction.

use alloc::string::String;
use core::error::Error as StdError;
use core::fmt;

use unicode_normalization::is_nfc;

const NAME_MAX_BYTES: usize = 255;

/// Why [`Name::new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// The string was empty.
    Empty,
    /// The string was longer than 255 bytes.
    TooLong,
    /// The string contained `/` or `\`.
    Separator,
    /// The string contained NUL (U+0000).
    Nul,
    /// The string contained a C0, DEL, C1, or line-separator control.
    Control,
    /// The string was `.` or `..`.
    Dot,
    /// The string had a trailing `.`, or leading or trailing Unicode whitespace.
    TrailingDotOrSpace,
    /// The string contained `<`, `>`, `:`, `"`, `|`, `?`, or `*`.
    Reserved,
    /// The stem is a Windows device name, with or without an extension.
    Device,
    /// The whole name is `.git` case-insensitively.
    Git,
    /// The string contained an invisible or bidi format character.
    Format,
    /// The string was not Unicode NFC.
    NotNfc,
}

impl NameError {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::Separator => "separator",
            Self::Nul => "nul",
            Self::Control => "control",
            Self::Dot => "dot",
            Self::TrailingDotOrSpace => "trailing-dot-or-space",
            Self::Reserved => "reserved",
            Self::Device => "device",
            Self::Git => "git",
            Self::Format => "format",
            Self::NotNfc => "not-nfc",
        }
    }
}

impl aether_data::Invariant for NameError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for NameError {}

/// One directory entry name. These are our rules, not Unix's: a tree is
/// something models and humans read, so a name that is not text is refused
/// at the boundary.
///
/// NFC gives each run of canonically equivalent text one spelling, so a
/// tree's byte-exact uniqueness also refuses `é` precomposed beside `e` plus
/// a combining acute.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, aether_data::Storage)]
#[storage(validate)]
pub struct Name(String);

impl Name {
    /// Accept a name that is a lossless directory entry on every host we run on.
    ///
    /// # Errors
    ///
    /// [`NameError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, NameError> {
        let value = value.into();
        Self::check(&value)?;
        Ok(Self(value))
    }

    /// Borrow the name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(value: &str) -> Result<(), NameError> {
        if value.is_empty() {
            return Err(NameError::Empty);
        }
        if value.len() > NAME_MAX_BYTES {
            return Err(NameError::TooLong);
        }
        if value == "." || value == ".." {
            return Err(NameError::Dot);
        }
        if has_trailing_dot_or_edge_whitespace(value) {
            return Err(NameError::TrailingDotOrSpace);
        }
        if let Some(error) = value.chars().find_map(char_error) {
            return Err(error);
        }
        if is_device_name(value) {
            return Err(NameError::Device);
        }
        if value.eq_ignore_ascii_case(".git") {
            return Err(NameError::Git);
        }
        if !is_nfc(value) {
            return Err(NameError::NotNfc);
        }
        Ok(())
    }
}

fn has_trailing_dot_or_edge_whitespace(value: &str) -> bool {
    value.ends_with('.')
        || value.chars().next().is_some_and(char::is_whitespace)
        || value.chars().next_back().is_some_and(char::is_whitespace)
}

fn char_error(ch: char) -> Option<NameError> {
    match ch {
        '/' | '\\' => Some(NameError::Separator),
        '\0' => Some(NameError::Nul),
        '\u{01}'..='\u{1F}' | '\u{7F}' | '\u{80}'..='\u{9F}' | '\u{2028}' | '\u{2029}' => Some(NameError::Control),
        '<' | '>' | ':' | '"' | '|' | '?' | '*' => Some(NameError::Reserved),
        '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}'
        | '\u{FEFF}'
        | '\u{00AD}'
        | '\u{061C}'
        | '\u{180E}' => Some(NameError::Format),
        _ => None,
    }
}

fn is_device_name(value: &str) -> bool {
    let stem = value.split_once('.').map_or(value, |(stem, _)| stem);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::{NAME_MAX_BYTES, Name, NameError};

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        let too_long = "a".repeat(NAME_MAX_BYTES + 1);
        let max_len = "a".repeat(NAME_MAX_BYTES);
        let cases = [
            ("", NameError::Empty, "a"),
            (too_long.as_str(), NameError::TooLong, max_len.as_str()),
            ("a/b", NameError::Separator, "a-b"),
            ("a\0b", NameError::Nul, "ab"),
            ("a\nb", NameError::Control, "ab"),
            (".", NameError::Dot, ".a"),
            ("foo.", NameError::TrailingDotOrSpace, "foo.rs"),
            ("a<b", NameError::Reserved, "ab"),
            ("nul.txt", NameError::Device, "null.txt"),
            (".git", NameError::Git, ".gitignore"),
            ("a\u{200B}b", NameError::Format, "ab"),
            ("e\u{0301}", NameError::NotNfc, "\u{00E9}"),
        ];
        for (reject, error, accept) in cases {
            assert_eq!(Name::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(Name::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }
        assert_eq!(Name::new("a\\b"), Err(NameError::Separator));
        assert_eq!(Name::new(".."), Err(NameError::Dot));
        assert_eq!(Name::new(" foo"), Err(NameError::TrailingDotOrSpace));
        assert_eq!(Name::new("foo "), Err(NameError::TrailingDotOrSpace));
    }
}
