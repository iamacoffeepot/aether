//! Bringing a reactor instance live for a head, and refusing to.

use core::error::Error as StdError;
use core::fmt;

use crate::{Detail, Digest, Head, OpaqueBytes, Seq};

/// Why a live-from seq was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveFromError {
    /// The seq was zero. `Seq(0)` names no event.
    Zero,
}

impl LiveFromError {
    const fn reason(self) -> &'static str {
        match self {
            Self::Zero => "zero",
        }
    }
}

impl aether_data::Invariant for LiveFromError {
    fn reason(&self) -> &'static str {
        Self::reason(*self)
    }
}

impl fmt::Display for LiveFromError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl StdError for LiveFromError {}

/// A nonzero seq. Validation also runs when stored bytes decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
struct LiveFrom(u64);

impl LiveFrom {
    // The `#[storage(validate)]` derive always calls `check(&inner)`; the
    // signature must take a reference to match that call site.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn check(value: &u64) -> Result<(), LiveFromError> {
        if *value == 0 {
            Err(LiveFromError::Zero)
        } else {
            Ok(())
        }
    }
}

/// A reactor instance the driver brought live for a head. Written only by
/// the driver, caused by the boundary seq.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.activated")]
pub struct Activated {
    head: Head<OpaqueBytes>,
    bundle: Digest,
    live_from: LiveFrom,
}

impl Activated {
    /// Bind a head, the bundle brought live for it, and the first seq it
    /// evaluates live.
    ///
    /// # Errors
    ///
    /// [`LiveFromError::Zero`] when `live_from` is `Seq(0)`.
    pub fn new(head: Head<OpaqueBytes>, bundle: Digest, live_from: Seq) -> Result<Self, LiveFromError> {
        LiveFrom::check(&live_from.0)?;
        Ok(Self { head, bundle, live_from: LiveFrom(live_from.0) })
    }

    /// The head this activation serves.
    #[must_use]
    pub const fn head(&self) -> &Head<OpaqueBytes> {
        &self.head
    }

    /// The bundle digest brought live for `head`.
    #[must_use]
    pub const fn bundle(&self) -> Digest {
        self.bundle
    }

    /// The first seq this instance evaluates live.
    #[must_use]
    pub const fn live_from(&self) -> Seq {
        Seq(self.live_from.0)
    }
}

/// A driver activation attempt that failed. Written only by the driver,
/// caused by the boundary seq. The rejected interval is owed to the next
/// successful activation of `head`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.activation_rejected")]
pub struct ActivationRejected {
    pub head: Head<OpaqueBytes>,
    pub bundle: Digest,
    pub reason: Detail,
}
