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

fn parse_dotted_name(value: &str) -> Result<(), NameRule> {
    if value.is_empty() {
        return Err(NameRule::Empty);
    }
    if value.len() > NAME_MAX_BYTES {
        return Err(NameRule::TooLong);
    }
    if !value.is_ascii() {
        return Err(NameRule::NotAscii);
    }
    if !value.split('.').all(valid_segment) {
        return Err(NameRule::BadSegment);
    }
    Ok(())
}

fn valid_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase() && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
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
