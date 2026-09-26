//! An attempt that produced no execution.
//!
//! 1. A fault is an attempt that ended without an `Execution<P>`. If `finish`
//!    ran, it is a result; if not, a fault.
//! 2. Whatever the wrapped thing did (tests failed, compiler errored) is a
//!    result, expressed in the result kind.
//! 3. Faults are about the attempt, never the subject. The reason set is closed.
//! 4. A program may return only `Refused`, `InputMissing`, `InputDecode`. The
//!    driver assigns the rest from outside. Programs never write events.
//! 5. A fault carries no blobs. Bounded inline detail only; text is a
//!    [`Detail`]. Truncation happens in [`Detail::new`]; decode of a stored
//!    blob past the cap refuses.
//! 6. One fault per attempt. A retry is a new event.
//! 7. Faults are never memoized and say nothing about purity.
//! 8. If a fault seems to need structure, the declaration's result kind is
//!    wrong. Faults are never widened.

use alloc::string::String;
use core::error::Error as StdError;
use core::fmt;

use crate::Digest;
use crate::program::reference::ProgramRef;

/// Why [`Detail`] decode refused a stored blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailError {
    /// The stored text was longer than [`Detail::MAX_BYTES`].
    TooLong,
}

impl aether_data::Invariant for DetailError {
    fn reason(&self) -> &'static str {
        "too-long"
    }
}

impl fmt::Display for DetailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(aether_data::Invariant::reason(self))
    }
}

impl StdError for DetailError {}

/// Bounded inline fault text. At most 4096 bytes; a constructor input past
/// the cap is cut at the last char boundary at or before it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[storage(validate)]
pub struct Detail(String);

impl Detail {
    pub const MAX_BYTES: usize = 4096;

    /// The one constructor. Never refuses: fault text is bounded, not rejected.
    #[must_use]
    pub fn new(text: impl AsRef<str>) -> Self {
        let text = text.as_ref();
        if text.len() <= Self::MAX_BYTES {
            return Self(String::from(text));
        }
        let mut end = Self::MAX_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        Self(String::from(&text[..end]))
    }

    /// Borrow the bounded text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn check(text: &str) -> Result<(), DetailError> {
        if text.len() > Self::MAX_BYTES {
            Err(DetailError::TooLong)
        } else {
            Ok(())
        }
    }
}

/// An attempt that ended without an execution. Written only by the driver.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.fault")]
pub struct Fault {
    pub program: ProgramRef,
    pub input: Digest,
    pub reason: FaultReason,
}

/// Closed set of reasons an attempt produced no execution.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum FaultReason {
    /// The input digest names nothing in the store.
    InputMissing,
    /// The input blob had the declared kind but did not decode.
    InputDecode,
    /// The program declined to attempt. Bounded reason.
    Refused { reason: Detail },
    /// The program panicked. Caught by the driver.
    Panicked { message: Detail },
    /// An executor's time allotment ran out. The deadline is the executor's and is not recorded.
    TimedOut,
    /// Reserved: no out-of-process program exists in this brick.
    Crashed { stderr_tail: Detail },
    /// The input's transitive closure exceeded the driver's byte cap. Nothing was loaded.
    ClosureTooLarge { limit_bytes: u64 },
    /// A failure before `Invoke`: a load error, an unreadable section, an unknown program name, or the wrong input kind.
    BundleUnavailable { reason: Detail },
    /// A failure after `Invoke`: `Invoked::Rejected`, or a result whose prefix doesn't match the declaration.
    ProtocolViolation { reason: Detail },
    /// The request was outstanding when the engine stopped.
    Interrupted,
    /// An executor's memory allotment ran out.
    ResourceExhausted,
    /// An executor failed during the attempt for a reason outside the request.
    ExecutorFailed { reason: Detail },
}

#[cfg(test)]
mod tests {
    use super::Detail;

    #[test]
    fn new_cuts_inside_a_multibyte_char_at_the_cap() {
        let mut text = "a".repeat(Detail::MAX_BYTES - 2);
        text.push('\u{4e2d}');
        assert_eq!(text.len(), Detail::MAX_BYTES + 1);
        let detail = Detail::new(&text);
        assert!(detail.as_str().len() <= Detail::MAX_BYTES);
        assert!(detail.as_str().is_char_boundary(detail.as_str().len()));
        assert_eq!(detail.as_str(), "a".repeat(Detail::MAX_BYTES - 2));
    }
}
