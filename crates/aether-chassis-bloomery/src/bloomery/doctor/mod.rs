//! The coordinator doctor: the lane-host kit preflight (#5035) and the
//! cross-source state invariants that keep quiet breakage loud (#5176).

mod kit;
pub use kit::{KitReport, KitTool, REQUIRED_KIT, ResolvedTool, ToolStatus};

mod invariants;
#[cfg(feature = "github")]
mod runtime;

pub use invariants::{
    Ancestry, CheckResult, DETERMINISTIC_RETRY_BOUND, DoctorReport, Invariant, LiveState, OpenDispatch,
    REPLICA_AGE_BOUND, ReplicaObservation, evaluate,
};
#[cfg(feature = "github")]
pub use runtime::{DoctorBoard, DoctorReactorState, DoctorTick};

#[cfg(feature = "github")]
use crate::bloomery::{ExecutorShell, SourceShell};
#[cfg(feature = "github")]
use aether_actor::actor;
#[cfg(feature = "github")]
use aether_bloomery::SharedCorrespondence;

/// Composer-supplied parts for the doctor reactor.
#[cfg(feature = "github")]
pub struct DoctorReactorSetup {
    /// The source that enumerates claim refs and answers ancestry.
    pub source: Option<SourceShell>,
    /// The executor whose occupancy answers "is a lane live".
    pub executor: Option<ExecutorShell>,
    /// The correspondence table `/view` mainline is checked through.
    pub correspondence: Option<SharedCorrespondence>,
    /// The store the journal and outbox are read from.
    pub store_path: String,
    /// Scratch-worktree base: `{nonce}-evidence` dirs live here.
    pub worktree_base: String,
    /// How often to wake and re-evaluate.
    pub poll_interval_secs: u64,
    /// The shared report cell `/view` overlays.
    pub board: DoctorBoard,
}

/// Addressing identity for the doctor reactor capability.
#[cfg(feature = "github")]
#[actor(singleton, root)]
pub struct DoctorReactorCapability;
