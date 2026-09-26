//! Where a bootstrap run stands, and the pure builders for the mail each step
//! sends.

use aether_actor::{ActorInitError, ErasedActorRef};
use aether_bloomery_kinds::{
    Call, ClosureArtifact, Digest, EncodedArtifact, Head, HeadNameError, NativeOrigin, OpaqueBytes, ProgramName,
    Publish, RecordedHead, RecordedHeadMove, Ref, Tree,
};
use aether_bloomery_workspace_programs::environment::MergeInput;
use aether_data::{Kind, Storage, StorageError};
use aether_workspace::Environment;

/// The head the operator binds to the workspace programs bundle.
const WORKSPACE_PROGRAMS: Head<OpaqueBytes> = Head::new("workspace-programs");

/// The program that builds the environment.
const MERGE: &str = "environment.merge";

/// The origin every call names, so the driver's idempotency key is scoped to
/// this script (ADR-0226 decision 11).
const ORIGIN: &str = "aether.bloomery.bootstrap";

/// The two native actors the script mails, proven once at `wire`.
#[derive(Debug, Clone, Copy)]
pub struct Peers {
    /// The journal owner: `ReadHead`, `Publish`, and `ReadArtifact` go here.
    pub journal: ErasedActorRef,
    /// The bundle driver: the merge `Call` goes here.
    pub driver: ErasedActorRef,
}

/// The bootstrap's lifecycle. Stored state holds only proofs, never an id or a
/// path.
#[derive(Debug)]
pub enum Run {
    /// Loaded; `wire` has not run.
    Unwired,
    /// Both peers are proven and one request is in flight.
    Live {
        /// The proven peers.
        peers: Peers,
        /// The step whose reply the script waits on.
        phase: Phase,
    },
    /// A step was refused and logged; nothing more is sent.
    Stopped,
}

/// The step whose reply the script waits on. One request is in flight at a
/// time.
#[derive(Debug)]
pub enum Phase {
    /// The base `Import` is in flight.
    ImportingBase,
    /// The toolchain `Import` is in flight.
    ImportingToolchain {
        /// The imported base tree.
        base: Ref<Tree>,
    },
    /// The journal's `ReadHead` is in flight.
    ReadingHead {
        /// The imported base tree.
        base: Ref<Tree>,
        /// The imported toolchain tree.
        toolchain: Ref<Tree>,
    },
    /// The `Publish` staging the `MergeInput` is in flight.
    Staging {
        /// The publish, resent at the journal's head on a fence conflict.
        publish: Publish,
    },
    /// The merge `Call` is in flight.
    Calling,
    /// The `ReadArtifact` of the merged environment is in flight.
    Reading {
        /// The transition's result, the environment's digest.
        result: Digest,
        /// The transition's sequence, the fence of the head move.
        seq: u64,
    },
    /// The `Publish` moving the environment head is in flight.
    Moving {
        /// The publish, resent at the journal's head on a fence conflict.
        publish: Publish,
    },
    /// The environment head moved; nothing more is sent.
    Done,
}

/// The `environment.merge` program in the bundle bound under
/// `workspace-programs`, called under this script's origin.
#[derive(Debug, Clone)]
pub struct MergeProgram {
    name: ProgramName,
    origin: NativeOrigin,
}

impl MergeProgram {
    /// The program and origin, validated once at `init`.
    ///
    /// # Errors
    ///
    /// An [`ActorInitError`] when either name breaks its grammar, which only
    /// an edit to this file can cause.
    pub fn new() -> Result<Self, ActorInitError> {
        Ok(Self {
            name: ProgramName::new(MERGE).map_err(|error| ActorInitError::new(format!("{MERGE}: {error}")))?,
            origin: NativeOrigin::new(ORIGIN).map_err(|error| ActorInitError::new(format!("{ORIGIN}: {error}")))?,
        })
    }

    /// The `Call` that merges the staged `input`. Its key is the input
    /// digest's first eight bytes, big-endian: a rerun over the same images
    /// replays the recorded answer (ADR-0226 decision 11), and new images get
    /// a new key, so a rerun is never answered `KeyReused`.
    #[must_use]
    pub fn call(&self, input: Digest) -> Call {
        let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = *input.as_bytes();
        Call {
            program: WORKSPACE_PROGRAMS,
            name: self.name.clone(),
            input,
            origin: self.origin.clone(),
            key: u64::from_be_bytes([b0, b1, b2, b3, b4, b5, b6, b7]),
        }
    }
}

/// The `Publish` staging the `MergeInput` over the two imported trees, fenced
/// at the journal's `head`.
///
/// # Errors
///
/// The [`StorageError`] when the input does not encode.
pub fn stage_input(base: Ref<Tree>, toolchain: Ref<Tree>, head: u64) -> Result<Publish, StorageError> {
    Ok(Publish::new(vec![EncodedArtifact::new(&MergeInput { base, toolchain })?], Vec::new(), head))
}

/// The environment the journal answered for `result`, verified against the
/// digest and decoded.
///
/// # Errors
///
/// The refusal's text when the artifact is not an environment, does not hash
/// to `result`, or does not decode.
pub fn environment(artifact: &ClosureArtifact, result: Digest) -> Result<Environment, String> {
    if artifact.kind() != Environment::ID {
        return Err(format!("{result} is not an {}", Environment::NAME));
    }
    let bytes = artifact.load(result).map_err(|error| error.to_string())?;
    Environment::decode_storage(&bytes).map(|data| data.value).map_err(|error| error.to_string())
}

/// The `Publish` moving the head `(aether.workspace.environment, <platform>)`
/// to `result`, fenced at `fence`.
///
/// # Errors
///
/// The [`HeadNameError`] when the platform does not name a head.
pub fn head_move(environment: &Environment, result: Digest, fence: u64) -> Result<Publish, HeadNameError> {
    let head = RecordedHead::new(Environment::ID, environment.platform.as_str())?;
    Ok(Publish::new(Vec::new(), vec![RecordedHeadMove::new(head, result)], fence))
}

/// `publish` again, fenced at the journal's `actual` head after a conflict.
#[must_use]
pub fn refence(publish: &Publish, actual: u64) -> Publish {
    let (artifacts, moves, _) = publish.clone().into_parts();
    Publish::new(artifacts, moves, actual)
}
