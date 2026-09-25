//! The closed set of decode policies, and the limits every policy carries.

use std::error::Error;
use std::fmt;

/// How much one [`crate::decode()`] call may make. Both budgets are at least 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    entries: u32,
    bytes: u64,
}

impl Limits {
    /// A budget of `entries` tree entries, implicit parent directories
    /// included, and `bytes` of file content across the whole archive.
    ///
    /// # Errors
    ///
    /// [`LimitsError`] names the budget that is zero.
    pub fn new(entries: u32, bytes: u64) -> Result<Self, LimitsError> {
        if entries == 0 {
            return Err(LimitsError::ZeroEntries);
        }
        if bytes == 0 {
            return Err(LimitsError::ZeroBytes);
        }
        Ok(Self { entries, bytes })
    }

    pub(super) fn entries(self) -> u32 {
        self.entries
    }

    pub(super) fn bytes(self) -> u64 {
        self.bytes
    }
}

/// Why [`Limits::new`] refused its budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsError {
    /// The entry budget was zero.
    ZeroEntries,
    /// The byte budget was zero.
    ZeroBytes,
}

impl fmt::Display for LimitsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroEntries => f.write_str("the entry limit is zero"),
            Self::ZeroBytes => f.write_str("the byte limit is zero"),
        }
    }
}

impl Error for LimitsError {}

/// The policy one [`crate::decode()`] call decodes under. There are exactly
/// two, one per kind of archive a caller reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    limits: Limits,
    userland: bool,
}

impl Rules {
    /// The tree rules and nothing else: every absolute symlink target and
    /// every device node is refused. For archives a run produced.
    #[must_use]
    pub fn canonical(limits: Limits) -> Self {
        Self { limits, userland: false }
    }

    /// The canonical rules, plus the two an imported userland needs: an
    /// absolute symlink target is rewritten to the relative target that
    /// resolves the same inside the tree, and a device node under `dev/` is
    /// dropped.
    #[must_use]
    pub fn userland(limits: Limits) -> Self {
        Self { limits, userland: true }
    }

    pub(super) fn limits(&self) -> Limits {
        self.limits
    }

    pub(super) fn is_userland(&self) -> bool {
        self.userland
    }
}
