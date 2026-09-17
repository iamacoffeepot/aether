//! Pure reactor selection at one journal event boundary.

use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{Head, OpaqueBytes, ReactorSet, Ref, Seq};

use crate::Heads;

/// One selected cluster identity and its exact bundle artifact reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedReactor {
    /// Cluster identity. Separate heads remain separate even for equal bytes.
    pub head: Head<OpaqueBytes>,
    /// Bundle reference bound to `head` before the event.
    pub artifact: Ref<OpaqueBytes>,
}

/// Why historical reactor selection could not resolve an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// Journal event sequences start at one.
    InvalidEventSeq,
    /// The supplied fold is not exactly the prefix before this event.
    WrongPrefix { expected: Seq, actual: Seq },
    /// The set root is not bound in that prefix.
    SetUnbound { root: Head<ReactorSet> },
    /// The decoded set does not match the root's selected artifact reference.
    SetMismatch { expected: Ref<ReactorSet>, supplied: Ref<ReactorSet> },
    /// The selected set omitted the required kernel head.
    KernelMissing { head: Head<OpaqueBytes> },
    /// A selected member head has no bundle binding in that prefix.
    MemberUnbound { head: Head<OpaqueBytes> },
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEventSeq => f.write_str("reactor selection requires an event sequence of at least one"),
            Self::WrongPrefix { expected, actual } => {
                write!(f, "reactor selection needs prefix {expected}, got {actual}")
            }
            Self::SetUnbound { root } => write!(f, "reactor set head {} is unbound", root.as_str()),
            Self::SetMismatch { .. } => f.write_str("supplied reactor set is not the selected set binding"),
            Self::KernelMissing { head } => write!(f, "reactor set omits required kernel head {}", head.as_str()),
            Self::MemberUnbound { head } => write!(f, "reactor bundle head {} is unbound", head.as_str()),
        }
    }
}

impl Error for SelectionError {}

/// Select recipients for `event_seq` from exactly prefix `event_seq - 1`.
///
/// `set_ref` and `set` must be a caller-verified artifact pair. This function
/// checks that the reference equals the root binding in `heads`; the caller
/// remains responsible for loading and verifying the artifact bytes. Missing
/// roots or members are errors, including at an empty genesis prefix.
///
/// # Errors
///
/// [`SelectionError`] when the cursor, selected set, required kernel, or a
/// member binding does not match the event's historical prefix.
pub fn select_reactors(
    heads: &Heads,
    event_seq: Seq,
    set_root: &Head<ReactorSet>,
    kernel: &Head<OpaqueBytes>,
    set_ref: Ref<ReactorSet>,
    set: &ReactorSet,
) -> Result<Vec<SelectedReactor>, SelectionError> {
    let expected = event_seq.0.checked_sub(1).map(Seq).ok_or(SelectionError::InvalidEventSeq)?;
    if heads.cursor() != expected {
        return Err(SelectionError::WrongPrefix { expected, actual: heads.cursor() });
    }

    let selected_set = heads.get(set_root).ok_or_else(|| SelectionError::SetUnbound { root: set_root.clone() })?;
    if selected_set != set_ref {
        return Err(SelectionError::SetMismatch { expected: selected_set, supplied: set_ref });
    }
    if !set.clusters().contains(kernel) {
        return Err(SelectionError::KernelMissing { head: kernel.clone() });
    }

    set.clusters()
        .iter()
        .map(|head| {
            heads
                .get(head)
                .map(|artifact| SelectedReactor { head: head.clone(), artifact })
                .ok_or_else(|| SelectionError::MemberUnbound { head: head.clone() })
        })
        .collect()
}
