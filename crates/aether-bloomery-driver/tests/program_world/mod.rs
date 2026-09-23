//! Program scenario scripting: calls, requests, and outcomes over [`World`](crate::support::World).
//!
//! This module is declared only by the program scenario target (`core.rs`),
//! so every item here is used there and the dead-code gate stays green.

use aether_bloomery_kinds::{
    Call, ClosureArtifact, Digest, Fault, FaultReason, Invoked, NativeOrigin, OpaqueBytes, ProgramName, ProgramRef,
    RequestSource, Requested, Transition,
};
use aether_data::Kind;

use crate::support::{World, program_head};

/// One validated program name.
///
/// # Panics
///
/// Panics if the name breaks [`ProgramName`] rules.
#[must_use]
pub fn program_name(name: &str) -> ProgramName {
    ProgramName::new(name).expect("valid test program name")
}

/// One validated native origin.
///
/// # Panics
///
/// Panics if the name breaks [`NativeOrigin`] rules.
#[must_use]
pub fn origin(name: &str) -> NativeOrigin {
    NativeOrigin::new(name).expect("valid test origin")
}

/// One native call.
#[must_use]
pub fn call(program: &'static str, name: &str, input: Digest, origin: &str, key: u64) -> Call {
    Call { program: program_head(program), name: program_name(name), input, origin: self::origin(origin), key }
}

/// One recorded request over a pinned bundle.
#[must_use]
pub fn requested(bundle: Digest, name: &str, input: Digest, origin: &str, key: u64) -> Requested {
    Requested {
        program: ProgramRef::new(bundle, program_name(name)),
        input,
        source: RequestSource::Native { origin: self::origin(origin), key },
    }
}

/// One recorded execution.
#[must_use]
pub fn transition(bundle: Digest, name: &str, input: Digest, result: Digest) -> Transition {
    Transition { program: ProgramRef::new(bundle, program_name(name)), input, result }
}

/// One recorded fault.
#[must_use]
pub fn fault(bundle: Digest, name: &str, input: Digest, reason: FaultReason) -> Fault {
    Fault { program: ProgramRef::new(bundle, program_name(name)), input, reason }
}

impl World {
    /// Store one program wasm bundle, answering its load.
    #[must_use]
    pub fn store_bundle(&mut self, wasm: &[u8]) -> Digest {
        let digest = self.store(OpaqueBytes::ID, wasm);
        self.loads.insert(digest, Ok(()));
        digest
    }

    /// Script one input's closure.
    pub fn script_closure(&mut self, root: Digest, artifacts: Vec<ClosureArtifact>) {
        self.closures.insert(root, artifacts);
    }

    /// Script one input's closure as too large.
    pub fn script_oversized(&mut self, root: Digest) {
        self.oversized.insert(root);
    }

    /// Leave one bundle's load unanswered so the test feeds it by hand.
    pub fn hold_load(&mut self, bundle: Digest) {
        self.loads.remove(&bundle);
    }

    /// Script one request's invoke reply.
    pub fn script_invoke(&mut self, seq: u64, invoked: Invoked) {
        self.invokes.insert(seq, invoked);
    }
}
