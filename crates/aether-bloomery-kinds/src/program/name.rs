//! Validated dotted names for programs and executors.

use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_data::storage::{RecordReader, RecordWriter, StorageElement, StorageError};
use aether_data::wire::{Error as WireError, WireDecode, WireEncode};
use aether_data::{Citations, Cites, LabelNode, Schema, SchemaType, StorageLeaves};

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
    ($Name:ident, $Error:ident, $kind:literal) => {
        /// Why [`$Name::new`] refused a string.
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

        impl fmt::Display for $Error {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.reason())
            }
        }

        impl StdError for $Error {}

        /// Validated dotted name. Two types so a program name and an executor
        /// name cannot be swapped in a record.
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $Name(String);

        impl $Name {
            /// Accept a dotted name of ASCII segments.
            ///
            /// # Errors
            ///
            /// [`$Error`] names which rule failed.
            pub fn new(value: impl Into<String>) -> Result<Self, $Error> {
                let value = value.into();
                parse_dotted_name(&value).map_err($Error::from_rule)?;
                Ok(Self(value))
            }

            /// Borrow the name as a string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Schema for $Name {
            const SCHEMA: SchemaType = <String as Schema>::SCHEMA;
            const LABEL: Option<&'static str> = Some(concat!(module_path!(), "::", stringify!($Name)));
            const LABEL_NODE: LabelNode = <String as Schema>::LABEL_NODE;
        }

        impl StorageLeaves for $Name {
            fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
                self.0.contribute(carry, depth, sink)
            }

            fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
                Self::new(<String as StorageLeaves>::assemble(carry, depth, source)?)
                    .map_err(|error| StorageError::Invariant { kind: $kind, reason: error.reason() })
            }

            fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
                <String as StorageLeaves>::is_absent(carry, depth, source)
            }
        }

        impl WireEncode for $Name {
            fn encode(&self, out: &mut Vec<u8>) -> Result<(), WireError> {
                self.0.encode(out)
            }
        }

        impl<'de> WireDecode<'de> for $Name {
            fn decode(cursor: &mut &'de [u8]) -> Result<Self, WireError> {
                Self::new(String::decode(cursor)?).map_err(|error| WireError::Message(error.reason().into()))
            }
        }

        impl StorageElement for $Name {
            const TAGGED: bool = <String as StorageElement>::TAGGED;

            fn contribute_element(&self, depth: u32, out: &mut Vec<u8>) -> Result<(), StorageError> {
                self.0.contribute_element(depth, out)
            }

            fn assemble_element(depth: u32, cursor: &mut &[u8]) -> Result<Self, StorageError> {
                Self::new(String::assemble_element(depth, cursor)?)
                    .map_err(|error| StorageError::Invariant { kind: $kind, reason: error.reason() })
            }
        }

        impl Cites for $Name {
            fn cites(&self, _sink: &mut Citations) {}
        }
    };
}

dotted_name!(ProgramName, ProgramNameError, "ProgramName");
dotted_name!(ExecutorName, ExecutorNameError, "ExecutorName");

#[cfg(test)]
mod tests {
    use super::{ExecutorName, NAME_MAX_BYTES, ProgramName, ProgramNameError};

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
        assert!(ExecutorName::new("a.b").is_ok());
        assert!(ExecutorName::new("a..b").is_err());
    }
}
