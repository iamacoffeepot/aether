//! The `aether.bloomery.benchmark` capability — the sequencer a benchmark run
//! actually runs on (ADR-0184).
//!
//! A run is a sequence of blooms, and the reducer permits one active bloom at a
//! time, so something has to hold the sequence: seal a cell, watch it to a
//! terminal status, reset the fixture's mainline to the golden-task base, seal
//! the next. That is reactor work — observe the projection on a poll cadence and
//! act on what it says — so it lives in its own capability rather than inside
//! the REST router, which answers requests and holds nothing that outlives one.
//!
//! # Why the door does not wait
//!
//! A run is minutes to hours of bloom lifetimes. `POST /benchmark` therefore
//! hands back a handle and returns; the operator reads progress from
//! `GET /benchmark/{run}`. The alternative — holding the HTTP request across the
//! whole sequence — would put an ingress timeout in charge of how long a
//! benchmark may take.
//!
//! # Trial mode is structural here
//!
//! The runner is mounted with the fixture repository or with nothing. Without
//! one it refuses every run, so the mainline reset — the affordance that makes
//! "the same task under four profiles" runnable at all — cannot exist on a
//! coordinator that talks to the estate's repository. The door's `409` on a
//! live-classed journal is the first gate; this is the second, and it is a type
//! rather than a check.
//!
//! Identity/runtime split (ADR-0122): this ZST is the addressing identity; the
//! sequencing lives in [`runtime`].

use aether_bloomery::StoreClass;
use aether_bloomery_git::fixture::FakeGithub;

use aether_actor::actor;

mod runtime;

pub use runtime::BenchmarkRunnerState;

/// Composer-supplied parts for the benchmark runner.
pub struct BenchmarkRunnerSetup {
    /// The in-memory repository a run replays landed history against and resets
    /// between cells. `None` mounts the runner refusing — which is every
    /// coordinator that is not in trial mode.
    pub fixture: Option<FakeGithub>,
    /// Which world this coordinator's journal records (ADR-0184). A run is
    /// refused outright unless it is [`StoreClass::Trial`].
    pub store_class: StoreClass,
    /// The mainline ref a reset points back at the golden-task base — the
    /// `heads/…` short form the fixture keys its refs by.
    pub mainline_ref: String,
    /// How often to wake and look at the bloom the run is waiting on.
    pub poll_interval_secs: u64,
}

/// Addressing identity for the `aether.bloomery.benchmark` capability
/// (ADR-0122).
#[actor(singleton, root)]
pub struct BenchmarkRunnerCapability;
