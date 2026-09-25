//! A path inside a tree that cannot leave it. Valid by construction.

use alloc::string::String;

use aether_bloomery_kinds::{Name, NameError};

const TREE_PATH_MAX_BYTES: usize = 1024;

/// Why [`TreePath::new`] or decode refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreePathError {
    /// The string was empty.
    Empty,
    /// The string was longer than 1024 bytes.
    TooLong,
    /// The string began with `/`.
    Absolute,
    /// The string contained an empty segment (`//` or a trailing `/`).
    EmptySegment,
    /// A segment failed a [`Name`] rule, which refuses `.` and `..`.
    Segment(NameError),
}

impl TreePathError {
    fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too-long",
            Self::Absolute => "absolute",
            Self::EmptySegment => "empty-segment",
            Self::Segment(error) => aether_data::Invariant::reason(&error),
        }
    }
}

invariant_errors!(TreePathError);

/// A relative path of one or more `/`-separated segments, each a [`Name`].
///
/// Unlike a symlink target ([`aether_bloomery_kinds::Path`]) it has no `.` or
/// `..` segment, so a mount point, a scratch path, or a tool path always names
/// a place inside its tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, aether_data::Storage)]
#[storage(validate)]
pub struct TreePath(String);

impl TreePath {
    /// Accept a relative path whose every segment is a valid [`Name`].
    ///
    /// # Errors
    ///
    /// [`TreePathError`] names which rule failed.
    pub fn new(value: impl Into<String>) -> Result<Self, TreePathError> {
        let value = value.into();
        Self::check(&value)?;
        Ok(Self(value))
    }

    /// Borrow the path as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(value: &str) -> Result<(), TreePathError> {
        if value.is_empty() {
            return Err(TreePathError::Empty);
        }
        if value.len() > TREE_PATH_MAX_BYTES {
            return Err(TreePathError::TooLong);
        }
        if value.starts_with('/') {
            return Err(TreePathError::Absolute);
        }
        if value.ends_with('/') || value.contains("//") {
            return Err(TreePathError::EmptySegment);
        }
        value.split('/').try_for_each(|segment| Name::new(segment).map(drop).map_err(TreePathError::Segment))
    }
}

/// Whether `inner` is `outer` or lies under it, segment by segment: `a`
/// covers `a` and `a/b` but not `ab`.
pub fn covers(outer: &str, inner: &str) -> bool {
    inner.strip_prefix(outer).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use aether_bloomery_kinds::NameError;

    use super::{TREE_PATH_MAX_BYTES, TreePath, TreePathError};
    use crate::kinds::test_support::assert_rule;

    fn path_of_len(len: usize) -> String {
        let mut path = "aa/".repeat(len / 3);
        path.push_str(&"a".repeat(len % 3));
        path
    }

    #[test]
    fn each_rule_refuses_and_accepts_its_neighbour() {
        let too_long = path_of_len(TREE_PATH_MAX_BYTES + 1);
        let max_len = path_of_len(TREE_PATH_MAX_BYTES);
        let cases = [
            ("", TreePathError::Empty, "a"),
            (too_long.as_str(), TreePathError::TooLong, max_len.as_str()),
            ("/usr/bin", TreePathError::Absolute, "usr/bin"),
            ("usr//bin", TreePathError::EmptySegment, "usr/bin"),
            ("usr/", TreePathError::EmptySegment, "usr"),
            ("../etc", TreePathError::Segment(NameError::Dot), "..a/etc"),
            ("usr/./bin", TreePathError::Segment(NameError::Dot), "usr/.a/bin"),
            ("a/.git", TreePathError::Segment(NameError::Git), "a/.gitignore"),
        ];
        for (reject, error, accept) in cases {
            assert_rule(TreePath::new, String::from(reject), error, String::from(accept));
        }
    }
}
