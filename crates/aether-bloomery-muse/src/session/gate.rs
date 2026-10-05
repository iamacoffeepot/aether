//! `muse.session.gate`: the proofs a `Done` end must pass, and the answer to
//! an end call whose gate did not pass (ADR-0234 decision 11).
//!
//! A session opens with its [`RequiredProofs`], chosen by whoever opens it
//! and never by the model. When the model ends `Done`, the loop runs every
//! required proof the session has not already passed on its current tree.
//! When one fails, or the proofs' passing trees never agree within
//! [`MAX_GATE_RUNS`] runs each, the loop answers the end call with the text
//! this program stages, and the session goes on.

use std::collections::BTreeSet;

use aether_bloomery_kinds::{Detail, Mode, ProgramName, Refusal};
use aether_bloomery_program::{Env, ErasedEdited, Program, Sync, function_name, program};
use aether_data::{ErasedRef, Ref, Utf8Text};

use crate::input::OfferedTools;

/// The most times one end call's gate runs one required proof: a proof whose
/// tree another proof's run changed is run again once.
pub const MAX_GATE_RUNS: u32 = 2;

/// One proof a `Done` end must pass: the proof tool's program and the
/// arguments the opener picked for it.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub struct RequiredProof {
    /// The proof tool's program, which the session must offer as a proof.
    program: ProgramName,
    /// The cited arguments the gate calls the proof with, of the kind its
    /// offered input schema names.
    args: ErasedRef,
}

impl RequiredProof {
    /// Require `program` to pass when called with `args`.
    #[must_use]
    pub const fn new(program: ProgramName, args: ErasedRef) -> Self {
        Self { program, args }
    }

    /// The proof tool's program.
    #[must_use]
    pub const fn program(&self) -> &ProgramName {
        &self.program
    }

    /// The cited arguments the gate calls the proof with.
    #[must_use]
    pub const fn args(&self) -> ErasedRef {
        self.args
    }
}

/// Why [`RequiredProofs::new`] or decode refused a required proof list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredProofsError {
    /// More than [`OfferedTools::MAX_TOOLS`] proofs.
    TooMany,
    /// One program was required twice.
    DuplicateProgram,
}

impl RequiredProofsError {
    const fn reason(self) -> &'static str {
        match self {
            Self::TooMany => "too-many",
            Self::DuplicateProgram => "duplicate-program",
        }
    }
}

/// The proofs a `Done` end must pass, in the order the gate runs them: at
/// most [`OfferedTools::MAX_TOOLS`], no program twice. Empty gates nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, aether_data::Storage)]
#[storage(validate)]
pub struct RequiredProofs(Vec<RequiredProof>);

impl RequiredProofs {
    /// Accept a list that requires each program once.
    ///
    /// # Errors
    ///
    /// The [`RequiredProofsError`] naming the rule the list broke.
    pub fn new(proofs: Vec<RequiredProof>) -> Result<Self, RequiredProofsError> {
        Self::check(&proofs)?;
        Ok(Self(proofs))
    }

    /// Every required proof, in the order the gate runs them.
    #[must_use]
    pub fn as_slice(&self) -> &[RequiredProof] {
        &self.0
    }

    fn check(proofs: &[RequiredProof]) -> Result<(), RequiredProofsError> {
        if proofs.len() > OfferedTools::MAX_TOOLS {
            return Err(RequiredProofsError::TooMany);
        }
        let mut programs = BTreeSet::new();
        if !proofs.iter().all(|proof| programs.insert(&proof.program)) {
            return Err(RequiredProofsError::DuplicateProgram);
        }
        Ok(())
    }
}

invariant_errors!(RequiredProofsError);

/// An end call whose gate did not pass, and why.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.gate.input")]
pub struct GateInput {
    /// Why the gate did not pass.
    failure: GateFailure,
}

impl GateInput {
    /// The gate's run of `proof` answered `result`, which did not pass.
    #[must_use]
    pub const fn failed(proof: ProgramName, result: Ref<ErasedEdited>) -> Self {
        Self { failure: GateFailure::Failed { proof, result } }
    }

    /// The passing runs of `proofs` kept leaving trees another had not
    /// passed on.
    #[must_use]
    pub const fn unsettled(proofs: Vec<ProgramName>) -> Self {
        Self { failure: GateFailure::Unsettled { proofs } }
    }
}

/// Why an end call's gate did not pass.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
pub enum GateFailure {
    /// A required proof's run did not pass.
    Failed {
        /// The proof's program.
        proof: ProgramName,
        /// The cited result of the run that did not pass.
        result: Ref<ErasedEdited>,
    },
    /// The required proofs' passing runs kept leaving trees another had not
    /// passed on, after [`MAX_GATE_RUNS`] runs of one of them.
    Unsettled {
        /// The proofs the gate ran, in the order they are required.
        proofs: Vec<ProgramName>,
    },
}

/// The staged refusal that answers an end call whose gate did not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "muse.session.gate")]
pub struct Gated {
    /// The cited text the end call is answered with.
    refusal: Ref<Utf8Text>,
}

impl Gated {
    /// The cited text the end call is answered with.
    #[must_use]
    pub const fn refusal(&self) -> Ref<Utf8Text> {
        self.refusal
    }
}

/// The `muse.session.gate` program.
pub struct SessionGate;

/// Stages the text that answers a `Done` end call whose gate did not pass:
/// the failed proof, by the function name the model calls it with, and the
/// proof's own summary, its verdict, rewritten files, and diagnostics; or
/// the proofs whose passing trees never agreed.
#[program]
impl Program for SessionGate {
    const NAME: &'static str = "muse.session.gate";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Stage the answer to a done end call whose required proofs did not pass.";
    type Input = GateInput;
    type Result = Gated;

    fn run(input: Self::Input, env: &mut Env<Sync>) -> Result<Self::Result, Refusal> {
        let text = match input.failure {
            GateFailure::Failed { proof, result } => {
                let summary = env.injected(result)?.summary().to_owned();
                format!(
                    "`muse-end` with `Done` needs `{}` to pass on the session's tree; it failed:\n\n{summary}",
                    function(&proof)?
                )
            }
            GateFailure::Unsettled { proofs } => {
                let named = proofs.iter().map(function).collect::<Result<Vec<_>, _>>()?.join("`, `");
                format!(
                    "`muse-end` with `Done` needs `{named}` to pass on one tree, but each run left a tree another had \
                     not passed on after {MAX_GATE_RUNS} runs each; make the tree satisfy them all, then end again"
                )
            }
        };
        Ok(Gated { refusal: env.stage_text(&text) })
    }
}

/// `program` by the function name the model calls it with.
fn function(program: &ProgramName) -> Result<String, Refusal> {
    function_name(program)
        .map_err(|error| Refusal::Refused { reason: Detail::new(format!("{}: {error}", program.as_str())) })
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{ProgramName, Tree};
    use aether_bloomery_program::{Edited, ErasedEdited};
    use aether_bloomery_workspace_programs::proof::ProofVerdict;
    use aether_data::Ref;

    use super::{GateInput, SessionGate};
    use crate::session::fixture::{run, stored};

    #[test]
    fn a_failed_gate_names_the_proof_as_the_model_calls_it_and_carries_its_summary_verbatim() {
        // Catches a gate answer that drops the proof's diagnostics or names the dotted program the model cannot
        // call.
        let verdict = ProofVerdict::Failed { diagnostics: Ref::of_text("error: unused variable `slag`") };
        let summary = "`cargo clippy` failed.\n\nerror: unused variable `slag`";
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let edited = Edited::new(tree, summary.to_owned(), Ref::of_encoded(&verdict).expect("verdict"));
        let result = Ref::<ErasedEdited>::from_digest(Ref::of_encoded(&edited).expect("edited").digest());
        let proof = ProgramName::new("proof.clippy").expect("program");

        let gated = run::<SessionGate>(&GateInput::failed(proof, result), vec![stored(&edited)])
            .expect("a failed gate is answered");
        let text = format!(
            "`muse-end` with `Done` needs `proof-clippy` to pass on the session's tree; it failed:\n\n{summary}"
        );
        assert_eq!(gated.refusal(), Ref::of_text(&text));
    }
}
