//! `proof.verdict`: whether a proof passed, citing what failed it.

use aether_bloomery_kinds::{Ref, Utf8Text};

/// The verdict of a proof run, the detail of the `Edited` it returns.
///
/// The verdict is the variant, so no field can disagree with it. A reader
/// such as a gate on ending a session reads it here instead of parsing the
/// summary, and can answer with the cited diagnostics as they are.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.verdict")]
pub enum ProofVerdict {
    /// Every step exited 0: the tree is formatted and the cargo step passed.
    Passed,
    /// A step exited with any other code, or died by signal.
    Failed {
        /// What the failing step reported, capped with the cut marked: the
        /// cargo step's rendered diagnostics, test failure blocks, or its
        /// stderr when it rendered none.
        diagnostics: Ref<Utf8Text>,
    },
}
