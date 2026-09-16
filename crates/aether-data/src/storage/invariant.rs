//! Error type a validated newtype's `check` returns.

/// Implemented by the error type a validated newtype's check returns.
pub trait Invariant {
    /// Stable, static reason for the refusal, e.g. `"too-long"`.
    fn reason(&self) -> &'static str;
}
