//! `proof.clippy.result`: whether the source passed clippy, citing what clippy wrote.

use aether_bloomery_kinds::{OpaqueBytes, Ref};

/// The verdict of one clippy step, citing its stored stderr.
///
/// The verdict is the variant, so no field can disagree with it. The result
/// cites nothing else: the transition's input already cites the source, the
/// environment, and the vendor tree.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "proof.clippy.result")]
pub enum ClippyResult {
    /// The step exited 0: the source has no clippy warning.
    Passed {
        /// Everything the step wrote to stderr.
        stderr: Ref<OpaqueBytes>,
    },
    /// The step exited with any other code, or died by signal.
    Failed {
        /// Everything the step wrote to stderr, where cargo reports each denied lint.
        stderr: Ref<OpaqueBytes>,
    },
}
