//! Untyped validated text that names a head.

use alloc::string::String;
use core::error::Error as StdError;
use core::fmt;

/// Why [`Symbol::new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolError {
    /// The string was empty.
    Empty,
    /// The string was longer than [`Symbol::MAX_BYTES`].
    TooLong,
    /// The string contained a character for which [`char::is_whitespace`] is true.
    Whitespace,
    /// The string contained a character for which [`char::is_control`] is true.
    Control,
}

impl SymbolError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::Whitespace => "whitespace",
            Self::Control => "control",
        }
    }
}

impl aether_data::Invariant for SymbolError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for SymbolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for SymbolError {}

/// Validated head name. One to 128 UTF-8 bytes, no whitespace or control.
///
/// Bytes are preserved exactly. Equality, hashing, and ordering are
/// case-sensitive and normalization-free. Punctuation has no special
/// semantics. A symbol is untyped: it is not a [`crate::ProgramName`],
/// and it is independent of any label inside the target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, aether_data::Storage)]
#[storage(validate)]
pub struct Symbol(String);

impl Symbol {
    /// Maximum accepted UTF-8 byte length, inclusive.
    pub const MAX_BYTES: usize = 128;

    /// Accept a symbol of 1–128 UTF-8 bytes with no whitespace or control.
    ///
    /// # Errors
    ///
    /// [`SymbolError`] names which rule failed. Length is checked before
    /// characters. A character that is both control and whitespace is
    /// [`SymbolError::Control`].
    pub fn new(value: impl Into<String>) -> Result<Self, SymbolError> {
        let value = value.into();
        Self::check(&value)?;
        Ok(Self(value))
    }

    /// Borrow the symbol as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(value: &str) -> Result<(), SymbolError> {
        if value.is_empty() {
            return Err(SymbolError::Empty);
        }
        if value.len() > Self::MAX_BYTES {
            return Err(SymbolError::TooLong);
        }
        for ch in value.chars() {
            if ch.is_control() {
                return Err(SymbolError::Control);
            }
            if ch.is_whitespace() {
                return Err(SymbolError::Whitespace);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Symbol, SymbolError};

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        // Catches a check that swapped reasons, skipped a predicate, or
        // accepted the empty string.
        let too_long = "a".repeat(Symbol::MAX_BYTES + 1);
        let max_len = "a".repeat(Symbol::MAX_BYTES);
        let cases = [
            ("", SymbolError::Empty, "a"),
            (too_long.as_str(), SymbolError::TooLong, max_len.as_str()),
            (" main", SymbolError::Whitespace, "main"),
            ("main ", SymbolError::Whitespace, "main"),
            ("a\u{00A0}b", SymbolError::Whitespace, "ab"),
            ("a\u{3000}b", SymbolError::Whitespace, "ab"),
            ("a\u{2028}b", SymbolError::Whitespace, "ab"),
            ("a\nb", SymbolError::Control, "ab"),
            ("a\0b", SymbolError::Control, "ab"),
            ("a\u{007F}b", SymbolError::Control, "ab"),
            ("a\u{0080}b", SymbolError::Control, "ab"),
            ("a\u{009F}b", SymbolError::Control, "ab"),
        ];
        for (reject, error, accept) in cases {
            assert_eq!(Symbol::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(Symbol::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }
    }

    #[test]
    fn the_byte_cap_is_not_a_character_cap() {
        // Catches a check that counted characters, so 64 × 'é' (128 bytes)
        // would be treated like 64 ASCII bytes, and 64 × 'é' plus one more
        // byte would sneak under a 128-character cap.
        let max = "é".repeat(64);
        assert_eq!(max.len(), Symbol::MAX_BYTES);
        assert_eq!(max.chars().count(), 64);
        assert_eq!(Symbol::new(max.as_str()).expect("128 bytes").as_str(), max);

        let mut over = max;
        over.push('a');
        assert_eq!(over.len(), Symbol::MAX_BYTES + 1);
        assert_eq!(Symbol::new(over.as_str()), Err(SymbolError::TooLong));
    }

    #[test]
    fn length_is_checked_before_characters() {
        // Catches a scan that reported whitespace or control on an
        // over-long input instead of too-long.
        let spaces = " ".repeat(Symbol::MAX_BYTES + 1);
        assert_eq!(Symbol::new(spaces.as_str()), Err(SymbolError::TooLong));
        let tabs = "\t".repeat(Symbol::MAX_BYTES + 1);
        assert_eq!(Symbol::new(tabs.as_str()), Err(SymbolError::TooLong));
    }

    #[test]
    fn a_character_that_is_both_control_and_whitespace_is_control() {
        // Catches a check that tested whitespace first and reported tab
        // or newline as whitespace.
        assert_eq!(Symbol::new("\t"), Err(SymbolError::Control));
        assert_eq!(Symbol::new("\n"), Err(SymbolError::Control));
        assert_eq!(Symbol::new("\u{0085}"), Err(SymbolError::Control));
        assert_eq!(Symbol::new(" "), Err(SymbolError::Whitespace));
    }

    #[test]
    fn punctuation_case_and_decomposition_are_preserved() {
        // Catches a wrapper that folded case, NFC-normalized, or treated
        // punctuation as a path or filesystem restriction.
        for accept in ["main/head", "a.b", "foo-bar", "foo:bar", "foo_bar", "foo*bar", "Main"] {
            assert_eq!(Symbol::new(accept).expect("punctuation and case are allowed").as_str(), accept);
        }
        let upper = Symbol::new("Main").expect("case is allowed");
        let lower = Symbol::new("main").expect("case is allowed");
        assert_ne!(upper, lower);
        assert!(upper < lower);

        let composed = Symbol::new("\u{00E9}").expect("composed spelling");
        let decomposed = Symbol::new("e\u{0301}").expect("decomposed spelling");
        assert_ne!(composed, decomposed);
        assert_eq!(composed.as_str(), "\u{00E9}");
        assert_eq!(decomposed.as_str(), "e\u{0301}");
    }
}
