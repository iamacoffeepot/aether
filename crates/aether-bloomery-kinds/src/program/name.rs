//! Validated dotted names for programs, reactors, rules, and native request origins.

use alloc::string::String;
use core::error::Error as StdError;
use core::fmt;

const NAME_MAX_BYTES: usize = 128;

/// Why a dotted name was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameRule {
    Empty,
    TooLong,
    NotAscii,
    BadSegment,
}

const fn parse_dotted_name(value: &str) -> Result<(), NameRule> {
    if value.is_empty() {
        return Err(NameRule::Empty);
    }
    if value.len() > NAME_MAX_BYTES {
        return Err(NameRule::TooLong);
    }
    if !value.is_ascii() {
        return Err(NameRule::NotAscii);
    }
    let bytes = value.as_bytes();
    let mut start = 0;
    let mut index = 0;
    loop {
        if index == bytes.len() || bytes[index] == b'.' {
            if !valid_segment_bytes(bytes, start, index) {
                return Err(NameRule::BadSegment);
            }
            if index == bytes.len() {
                return Ok(());
            }
            start = index + 1;
        }
        index += 1;
    }
}

const fn valid_segment_bytes(bytes: &[u8], start: usize, end: usize) -> bool {
    if start >= end {
        return false;
    }
    if !bytes[start].is_ascii_lowercase() {
        return false;
    }
    let mut index = start + 1;
    while index < end {
        let byte = bytes[index];
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_') {
            return false;
        }
        index += 1;
    }
    true
}

macro_rules! dotted_name {
    ($Name:ident, $Error:ident) => {
        /// Why construction refused a string.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $Error {
            /// The string was empty.
            Empty,
            /// The string was longer than 128 bytes.
            TooLong,
            /// The string contained a non-ASCII byte.
            NotAscii,
            /// A segment was empty, or did not match `[a-z][a-z0-9_]*`.
            BadSegment,
        }

        impl $Error {
            const fn from_rule(rule: NameRule) -> Self {
                match rule {
                    NameRule::Empty => Self::Empty,
                    NameRule::TooLong => Self::TooLong,
                    NameRule::NotAscii => Self::NotAscii,
                    NameRule::BadSegment => Self::BadSegment,
                }
            }

            const fn reason(self) -> &'static str {
                match self {
                    Self::Empty => "empty",
                    Self::TooLong => "too-long",
                    Self::NotAscii => "not-ascii",
                    Self::BadSegment => "bad-segment",
                }
            }
        }

        impl aether_data::Invariant for $Error {
            fn reason(&self) -> &'static str {
                Self::reason(*self)
            }
        }

        impl fmt::Display for $Error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.reason())
            }
        }

        impl StdError for $Error {}

        /// Validated dotted name.
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, aether_data::Storage)]
        #[storage(validate)]
        pub struct $Name(String);

        impl $Name {
            /// Accept a dotted name of ASCII segments.
            ///
            /// # Errors
            ///
            /// The matching error names which rule failed.
            pub fn new(value: impl Into<String>) -> Result<Self, $Error> {
                let value = value.into();
                Self::check(&value)?;
                Ok(Self(value))
            }

            /// Borrow the name as a string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Whether `value` is a valid dotted name of this kind.
            #[must_use]
            pub const fn is_valid(value: &str) -> bool {
                matches!(parse_dotted_name(value), Ok(()))
            }

            fn check(value: &str) -> Result<(), $Error> {
                parse_dotted_name(value).map_err($Error::from_rule)
            }
        }
    };
}

dotted_name!(ProgramName, ProgramNameError);
dotted_name!(ReactorName, ReactorNameError);
dotted_name!(RuleName, RuleNameError);
dotted_name!(NativeOrigin, NativeOriginError);

#[cfg(test)]
mod tests {
    use super::{NAME_MAX_BYTES, ProgramName, ProgramNameError};

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        let too_long = "a".repeat(NAME_MAX_BYTES + 1);
        let max_len = "a".repeat(NAME_MAX_BYTES);
        let cases = [
            ("", ProgramNameError::Empty, "a"),
            (too_long.as_str(), ProgramNameError::TooLong, max_len.as_str()),
            ("café", ProgramNameError::NotAscii, "cafe"),
            ("a..b", ProgramNameError::BadSegment, "a.b"),
            ("A.b", ProgramNameError::BadSegment, "a.b"),
            ("1a", ProgramNameError::BadSegment, "a_1"),
        ];
        for (reject, error, accept) in cases {
            assert_eq!(ProgramName::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(ProgramName::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }
        assert_eq!(ProgramName::new("a."), Err(ProgramNameError::BadSegment));
        assert_eq!(ProgramName::new(".a"), Err(ProgramNameError::BadSegment));
        assert_eq!(ProgramName::new("_a"), Err(ProgramNameError::BadSegment));
    }
}
