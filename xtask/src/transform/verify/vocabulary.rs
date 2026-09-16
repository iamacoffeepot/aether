//! The verifier vocabulary: which identities exist, and what each verify
//! position's fan-out actually runs.
//!
//! Held here, beside the lane that runs it, rather than read out of a checked-in
//! manifest. The manifest existed so a coordinator dispatching a lane from
//! outside the tree could seal the vocabulary the tree declared; nothing
//! dispatches this lane from outside any more, so the one reader of the
//! vocabulary is also its one author.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::failure::VerifyFailure;

/// The verifier vocabulary and what each verify position's fan-out runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct DeclaredVerifiers {
    /// Every verifier identity, in canonical order — a position is an
    /// identity's bit in a recorded failure set, so the order is append-only.
    pub(super) identities: Vec<String>,
    /// The fan-out each verify command runs, keyed by that command.
    ///
    /// A subset of [`identities`](Self::identities): an identity judged outside
    /// the umbrella is declared without appearing in any position's list, which
    /// is how "a legal identity no lane runs" is stated as data rather than
    /// remembered.
    pub(super) runs: BTreeMap<String, Vec<String>>,
}

/// The lane vocabulary this repository runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct PipelineManifest {
    /// The verifier vocabulary and per-position fan-out.
    pub(super) verifiers: DeclaredVerifiers,
}

/// The fan-out the whole-tree positions run — documentation included.
///
/// `verify.containment` is a legal identity no lane runs and is therefore
/// absent.
const FOLD_RUNS: [VerifyFailure; 9] = [
    VerifyFailure::Preflight,
    VerifyFailure::Fmt,
    VerifyFailure::Clippy,
    VerifyFailure::Docs,
    VerifyFailure::Test,
    VerifyFailure::Dup,
    VerifyFailure::Deps,
    VerifyFailure::Suppress,
    VerifyFailure::Lock,
];

/// The fan-out the per-member position runs — [`FOLD_RUNS`] less
/// [`VerifyFailure::Docs`].
///
/// Documentation correctness is a whole-workspace property: an intra-doc link
/// resolves across crates, so a narrowed closure can neither break it alone nor
/// prove it alone, while running it there was the most expensive gate that
/// position carried.
const MEMBER_RUNS: [VerifyFailure; 8] = [
    VerifyFailure::Preflight,
    VerifyFailure::Fmt,
    VerifyFailure::Clippy,
    VerifyFailure::Test,
    VerifyFailure::Dup,
    VerifyFailure::Deps,
    VerifyFailure::Suppress,
    VerifyFailure::Lock,
];

impl PipelineManifest {
    /// The vocabulary this binary compiles.
    #[must_use]
    pub(super) fn compiled() -> Self {
        let runs = [
            (super::VERIFY_MEMBER, MEMBER_RUNS.as_slice()),
            (super::VERIFY_CHECK, FOLD_RUNS.as_slice()),
            (super::VERIFY_BASE, FOLD_RUNS.as_slice()),
        ]
        .into_iter()
        .map(|(command, gates)| {
            (command.to_owned(), gates.iter().map(|gate| gate.as_str().to_owned()).collect::<Vec<String>>())
        })
        .collect();

        Self {
            verifiers: DeclaredVerifiers {
                identities: VerifyFailure::ALL.iter().map(|gate| gate.as_str().to_owned()).collect(),
                runs,
            },
        }
    }

    /// The identity `name` spells, or `None` when this vocabulary does not
    /// declare it.
    ///
    /// A membership test against the declared list, not a bare parse: an
    /// identity the vocabulary does not carry has no position, so nothing can
    /// record a failure under it.
    #[must_use]
    pub(super) fn intern(&self, name: &str) -> Option<VerifyFailure> {
        if !self.verifiers.identities.iter().any(|declared| declared == name) {
            return None;
        }
        VerifyFailure::from_name(name)
    }
}

/// The vocabulary every reader in this lane shares.
///
/// Cached for the process: one umbrella pass asks more than once (preflight,
/// fan-out, intern).
pub(super) fn checkout_vocabulary() -> &'static PipelineManifest {
    static VOCABULARY: OnceLock<PipelineManifest> = OnceLock::new();
    VOCABULARY.get_or_init(PipelineManifest::compiled)
}

#[cfg(test)]
mod tests {
    use super::super::failure::MAX_VERIFIER_IDENTITIES;
    use super::{PipelineManifest, VerifyFailure, checkout_vocabulary};

    #[test]
    fn every_declared_identity_interns_and_nothing_else_does() {
        let manifest = checkout_vocabulary();

        for failure in VerifyFailure::ALL {
            assert_eq!(manifest.intern(failure.as_str()), Some(failure));
        }
        assert_eq!(manifest.intern("verify.nothing"), None);
        assert!(manifest.verifiers.identities.len() <= MAX_VERIFIER_IDENTITIES);
    }

    /// Tripwire: containment is declared but no position runs it. A fan-out
    /// that silently picked it up would spawn a gate with no argv.
    #[test]
    fn a_declared_identity_no_position_runs_stays_out_of_every_fan_out() {
        let manifest = PipelineManifest::compiled();

        assert!(manifest.verifiers.identities.iter().any(|id| id == VerifyFailure::Containment.as_str()));
        for gates in manifest.verifiers.runs.values() {
            assert!(!gates.iter().any(|id| id == VerifyFailure::Containment.as_str()), "{gates:?}");
            assert!(gates.iter().all(|id| manifest.intern(id).is_some()), "{gates:?}");
        }
    }
}
