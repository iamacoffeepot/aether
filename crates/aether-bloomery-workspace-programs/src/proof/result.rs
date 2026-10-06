//! `proof.verdict`: whether a proof passed, citing what failed it.

use aether_data::{Ref, Utf8Text};

/// The verdict of a proof run, the detail of the `Edited` it returns.
///
/// The verdict is the variant, so no field can disagree with it. A reader
/// such as a gate on ending a session reads it here instead of parsing the
/// summary, and can answer with the cited diagnostics as they are.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.verdict")]
pub enum ProofVerdict {
    /// Every step exited 0: the tree is formatted and the cargo step passed.
    /// Whole-workspace passes only: clippy always, test with an empty scope.
    /// Only this variant marks a program proven on the tree at `Done`.
    Passed,
    /// Every step exited 0 over a scoped test run: the tree is formatted
    /// and the scoped cargo step passed. It adopts its formatted tree but
    /// never marks proven, so the gate's whole-workspace run still runs at
    /// `Done`.
    PassedScoped {
        /// The scope the passing run built and ran.
        scope: super::TestScope,
    },
    /// A step exited with any other code, or died by signal.
    Failed {
        /// What the failing step reported, capped with the cut marked: the
        /// cargo step's rendered diagnostics, test failure blocks, or its
        /// stderr when it rendered none.
        diagnostics: Ref<Utf8Text>,
    },
}
