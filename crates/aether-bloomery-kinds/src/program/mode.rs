//! Purity of a program: the same input digest always yields the same result, or not.

/// How executions of a program relate across executors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
pub enum Mode {
    /// Every correct executor produces the same result digest for the same input digest.
    Pure,
    /// Each execution is one observation. Never memoized.
    Sampled,
}
