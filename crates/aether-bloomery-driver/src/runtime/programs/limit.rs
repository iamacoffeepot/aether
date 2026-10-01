//! The invocation limit: how many program requests one bundle runs at once.

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

/// How many requests one bundle digest has active at once, each in its own
/// invocation (ADR-0226 decision 3). The rest wait in FIFO order.
///
/// Never zero: a limit of zero would leave every request waiting forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationLimit(NonZeroUsize);

impl InvocationLimit {
    /// The limit a driver runs under unless configured otherwise.
    pub const DEFAULT: Self = Self(NonZeroUsize::new(16).expect("16 is non-zero"));

    /// A limit of `count` invocations per bundle, refusing zero.
    pub fn new(count: usize) -> Result<Self, InvocationLimitError> {
        NonZeroUsize::new(count).map(Self).ok_or(InvocationLimitError)
    }

    /// The number of requests one bundle may have active at once.
    #[must_use]
    pub fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for InvocationLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A zero invocation limit, which would never start a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvocationLimitError;

impl fmt::Display for InvocationLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the invocation limit must be at least 1")
    }
}

impl Error for InvocationLimitError {}
