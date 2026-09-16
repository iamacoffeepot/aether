//! An attempt that produced no execution.
//!
//! 1. A fault is an attempt that ended without an `Execution<P>`. If `finish`
//!    ran, it is a result; if not, a fault.
//! 2. Whatever the wrapped thing did (tests failed, compiler errored) is a
//!    result, expressed in the result kind.
//! 3. Faults are about the attempt, never the subject. The reason set is closed.
//! 4. An executor may return only `Refused`, `InputMissing`, `InputDecode`. The
//!    driver assigns the rest from outside. Executors never write events.
//! 5. A fault carries no blobs. Bounded inline detail only; `String` fields are
//!    capped at 4096 bytes by a validated constructor on `FaultReason`
//!    (`FaultReason::refused(text)` truncates, never refuses).
//! 6. One fault per attempt. A retry is a new event.
//! 7. Faults are never memoized and say nothing about purity.
//! 8. If a fault seems to need structure, the declaration's result kind is
//!    wrong. Faults are never widened.

use alloc::borrow::Cow;
use alloc::string::String;

use aether_data::storage::{
    RecordReader, RecordWriter, StorageError, UNIT_SCHEMA, VARIANT_LEAF, fold_path_segment, variant_hash,
};
use aether_data::{Citations, Cites, NamedField, Schema, SchemaType, StorageLeaves};

use crate::program::Program;
use crate::program::name::ExecutorName;
use crate::{Digest, Ref};

const DETAIL_MAX_BYTES: usize = 4096;

/// An attempt that ended without an execution. Written only by the driver.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "bloomery.fault")]
pub struct Fault {
    pub program: Ref<Program>,
    pub input: Digest,
    pub executor: ExecutorName,
    pub reason: FaultReason,
}

/// Closed set of reasons an attempt produced no execution.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub enum FaultReason {
    /// The input digest names nothing in the store.
    InputMissing,
    /// The input blob had the declared kind but did not decode.
    InputDecode,
    /// The executor declined to attempt. Bounded reason.
    Refused { reason: String },
    /// The executor panicked. Caught by the driver.
    Panicked { message: String },
    /// Reserved: the driver enforces no timeout in this brick.
    TimedOut { after_millis: u64 },
    /// Reserved: no out-of-process executor exists in this brick.
    Crashed { stderr_tail: String },
}

impl FaultReason {
    /// Decline with a bounded reason. Truncates at 4096 bytes; never refuses.
    #[must_use]
    pub fn refused(text: impl AsRef<str>) -> Self {
        Self::Refused { reason: cap_detail(text.as_ref()) }
    }

    /// A panic caught by the driver. Truncates at 4096 bytes.
    #[must_use]
    pub fn panicked(message: impl AsRef<str>) -> Self {
        Self::Panicked { message: cap_detail(message.as_ref()) }
    }

    /// Reserved timeout reason.
    #[must_use]
    pub fn timed_out(after_millis: u64) -> Self {
        Self::TimedOut { after_millis }
    }

    /// Reserved crash reason. Truncates at 4096 bytes.
    #[must_use]
    pub fn crashed(stderr_tail: impl AsRef<str>) -> Self {
        Self::Crashed { stderr_tail: cap_detail(stderr_tail.as_ref()) }
    }

    fn capped(self) -> Self {
        match self {
            Self::Refused { reason } => Self::Refused { reason: cap_detail(&reason) },
            Self::Panicked { message } => Self::Panicked { message: cap_detail(&message) },
            Self::Crashed { stderr_tail } => Self::Crashed { stderr_tail: cap_detail(&stderr_tail) },
            other => other,
        }
    }
}

fn cap_detail(text: &str) -> String {
    if text.len() <= DETAIL_MAX_BYTES {
        return String::from(text);
    }
    let mut end = DETAIL_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    String::from(&text[..end])
}

impl Cites for FaultReason {
    fn cites(&self, _sink: &mut Citations) {}
}

const REFUSED_FIELDS: &[NamedField] = &[NamedField { name: Cow::Borrowed("reason"), ty: <String as Schema>::SCHEMA }];
const PANICKED_FIELDS: &[NamedField] = &[NamedField { name: Cow::Borrowed("message"), ty: <String as Schema>::SCHEMA }];
const TIMED_OUT_FIELDS: &[NamedField] =
    &[NamedField { name: Cow::Borrowed("after_millis"), ty: <u64 as Schema>::SCHEMA }];
const CRASHED_FIELDS: &[NamedField] =
    &[NamedField { name: Cow::Borrowed("stderr_tail"), ty: <String as Schema>::SCHEMA }];

const REFUSED_SCHEMA: SchemaType = SchemaType::Struct { fields: Cow::Borrowed(REFUSED_FIELDS), repr_c: false };
const PANICKED_SCHEMA: SchemaType = SchemaType::Struct { fields: Cow::Borrowed(PANICKED_FIELDS), repr_c: false };
const TIMED_OUT_SCHEMA: SchemaType = SchemaType::Struct { fields: Cow::Borrowed(TIMED_OUT_FIELDS), repr_c: false };
const CRASHED_SCHEMA: SchemaType = SchemaType::Struct { fields: Cow::Borrowed(CRASHED_FIELDS), repr_c: false };

impl StorageLeaves for FaultReason {
    fn contribute(&self, carry: u64, depth: u32, sink: &mut RecordWriter) -> Result<(), StorageError> {
        let disc = match self {
            Self::InputMissing => variant_hash("InputMissing", &UNIT_SCHEMA),
            Self::InputDecode => variant_hash("InputDecode", &UNIT_SCHEMA),
            Self::Refused { .. } => variant_hash("Refused", &REFUSED_SCHEMA),
            Self::Panicked { .. } => variant_hash("Panicked", &PANICKED_SCHEMA),
            Self::TimedOut { .. } => variant_hash("TimedOut", &TIMED_OUT_SCHEMA),
            Self::Crashed { .. } => variant_hash("Crashed", &CRASHED_SCHEMA),
        };
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        u64::contribute(&disc, var_carry, depth + 1, sink)?;
        match self {
            Self::InputMissing | Self::InputDecode => Ok(()),
            Self::Refused { reason } => {
                let body = fold_path_segment(carry, b"Refused", depth);
                let field = fold_path_segment(body, b"reason", depth + 1);
                String::contribute(&cap_detail(reason), field, depth + 2, sink)
            }
            Self::Panicked { message } => {
                let body = fold_path_segment(carry, b"Panicked", depth);
                let field = fold_path_segment(body, b"message", depth + 1);
                String::contribute(&cap_detail(message), field, depth + 2, sink)
            }
            Self::TimedOut { after_millis } => {
                let body = fold_path_segment(carry, b"TimedOut", depth);
                let field = fold_path_segment(body, b"after_millis", depth + 1);
                u64::contribute(after_millis, field, depth + 2, sink)
            }
            Self::Crashed { stderr_tail } => {
                let body = fold_path_segment(carry, b"Crashed", depth);
                let field = fold_path_segment(body, b"stderr_tail", depth + 1);
                String::contribute(&cap_detail(stderr_tail), field, depth + 2, sink)
            }
        }
    }

    fn assemble(carry: u64, depth: u32, source: &mut RecordReader) -> Result<Self, StorageError> {
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        let disc = u64::assemble(var_carry, depth + 1, source)?;
        let value = if disc == variant_hash("InputMissing", &UNIT_SCHEMA) {
            Self::InputMissing
        } else if disc == variant_hash("InputDecode", &UNIT_SCHEMA) {
            Self::InputDecode
        } else if disc == variant_hash("Refused", &REFUSED_SCHEMA) {
            let body = fold_path_segment(carry, b"Refused", depth);
            let field = fold_path_segment(body, b"reason", depth + 1);
            Self::Refused { reason: String::assemble(field, depth + 2, source)? }
        } else if disc == variant_hash("Panicked", &PANICKED_SCHEMA) {
            let body = fold_path_segment(carry, b"Panicked", depth);
            let field = fold_path_segment(body, b"message", depth + 1);
            Self::Panicked { message: String::assemble(field, depth + 2, source)? }
        } else if disc == variant_hash("TimedOut", &TIMED_OUT_SCHEMA) {
            let body = fold_path_segment(carry, b"TimedOut", depth);
            let field = fold_path_segment(body, b"after_millis", depth + 1);
            Self::TimedOut { after_millis: u64::assemble(field, depth + 2, source)? }
        } else if disc == variant_hash("Crashed", &CRASHED_SCHEMA) {
            let body = fold_path_segment(carry, b"Crashed", depth);
            let field = fold_path_segment(body, b"stderr_tail", depth + 1);
            Self::Crashed { stderr_tail: String::assemble(field, depth + 2, source)? }
        } else {
            return Err(StorageError::UnknownVariant { hash: disc });
        };
        Ok(value.capped())
    }

    fn is_absent(carry: u64, depth: u32, source: &RecordReader) -> bool {
        let var_carry = fold_path_segment(carry, VARIANT_LEAF.as_bytes(), depth);
        u64::is_absent(var_carry, depth + 1, source)
    }
}
