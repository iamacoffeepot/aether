//! Inline symlink target. Valid by construction.

use alloc::string::String;
use core::error::Error as StdError;
use core::fmt;

use crate::tree::name::{Name, NameError};

const PATH_MAX_BYTES: usize = 1024;

/// Why [`Path::new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The string was empty.
    Empty,
    /// The string was longer than 1024 bytes.
    TooLong,
    /// The string began with `/`.
    Absolute,
    /// The string contained an empty segment (`//` or a trailing `/`).
    EmptySegment,
    /// A segment failed a [`Name`] rule. `.` and `..` are allowed.
    Segment(NameError),
}

impl PathError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::Absolute => "absolute",
            Self::EmptySegment => "empty-segment",
            Self::Segment(error) => error.reason(),
        }
    }
}

impl aether_data::Invariant for PathError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Segment(error) => write!(f, "segment: {error}"),
            other => f.write_str(other.reason()),
        }
    }
}

impl StdError for PathError {}

/// A symlink target stored inline in the tree, not as a separate blob.
/// Relative, `/`-separated, and each segment is `.`, `..`, or a [`Name`].
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Path(String);

impl Path {
    /// Accept a relative target whose segments are `.`, `..`, or a valid [`Name`].
    ///
    /// # Errors
    ///
    /// [`PathError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, PathError> {
        let value = value.into();
        Self::check(&value)?;
        Ok(Self(value))
    }

    /// Borrow the target as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(value: &str) -> Result<(), PathError> {
        if value.is_empty() {
            return Err(PathError::Empty);
        }
        if value.len() > PATH_MAX_BYTES {
            return Err(PathError::TooLong);
        }
        if value.starts_with('/') {
            return Err(PathError::Absolute);
        }
        if value.ends_with('/') || value.contains("//") {
            return Err(PathError::EmptySegment);
        }
        for segment in value.split('/') {
            if segment == "." || segment == ".." {
                continue;
            }
            if let Err(error) = Name::new(segment) {
                return Err(PathError::Segment(error));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::{PATH_MAX_BYTES, Path, PathError};
    use crate::tree::name::NameError;

    fn path_of_len(len: usize) -> String {
        let mut path = "aa/".repeat(len / 3);
        path.push_str(&"a".repeat(len % 3));
        debug_assert_eq!(path.len(), len);
        path
    }

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        let too_long = path_of_len(PATH_MAX_BYTES + 1);
        let max_len = path_of_len(PATH_MAX_BYTES);
        let cases = [
            ("", PathError::Empty, "a"),
            (too_long.as_str(), PathError::TooLong, max_len.as_str()),
            ("/bin/run", PathError::Absolute, "../bin/run"),
            ("foo/", PathError::EmptySegment, "foo"),
            ("a/.git/config", PathError::Segment(NameError::Git), "a/.github/config"),
        ];
        for (reject, error, accept) in cases {
            assert_eq!(Path::new(reject), Err(error), "reject {reject:?}");
            assert_eq!(Path::new(accept).expect("accepted neighbour").as_str(), accept, "accept {accept:?}");
        }
    }
}
