//! The benchmark runner's mail vocabulary and its rendered run document
//! (ADR-0184).
//!
//! The door accepts a run and hands back a handle; the run itself outlives that
//! response, so what crosses between the two is a request and a rendering rather
//! than a held obligation. None of these are journaled: a run is trial-mode
//! bookkeeping, and the blooms it seals are the durable record.

use aether_bloomery::{BloomId, BloomStatus, Digest};

use super::golden::GoldenTask;

/// The standing statement every rendered run carries about its own durability.
///
/// A run is deliberately not a persisted fact (ADR-0184 is about measuring the
/// line, not about adding a second thing to recover), so a restart ends it. That
/// is only honest if it is *stated*: a reader who cannot find a run must be able
/// to tell "this coordinator restarted" from "you mistyped the handle", and the
/// blooms are in the journal either way.
pub const RUN_VOLATILITY: &str = concat!(
    "A benchmark run is trial-mode bookkeeping held in memory, never a journaled fact: a coordinator restart ends ",
    "the run in flight and this document with it, and an unknown handle after a restart means exactly that. Every ",
    "bloom the run sealed is in the journal regardless, so what a lost run costs is the sequence, not the ",
    "measurement.",
);

/// Start a benchmark run (ADR-0184). The door has already proved trial mode and
/// that the request states a reason and an operator; what reaches the runner is
/// the run itself.
#[aether_data::kind(name = "aether.bloomery.benchmark.start", eq)]
pub struct StartBenchmark {
    /// What to call this golden-task set.
    pub set: String,
    /// The base every cell replays over, and the set's version.
    pub base: Digest,
    /// The landed pull requests to draw tasks from.
    pub pull_requests: Vec<u64>,
    /// One recorded `aether.bloomery.model_override` address per profile cell.
    pub cells: Vec<Digest>,
    /// How many blooms to seal per `(task, cell)`.
    pub samples: u32,
    /// The recorded `aether.bloomery.model_process_instructions` bundle every
    /// bloom pins (ADR-0214).
    pub instructions: Digest,
}

/// Whether a run was accepted, and its handle if so.
#[aether_data::kind(name = "aether.bloomery.benchmark.start_result", eq)]
pub enum StartBenchmarkResult {
    /// Accepted and started. The first cell's bloom is sealed from here on; the
    /// operator reads progress rather than waiting for it.
    Accepted {
        /// The handle `GET /benchmark/{run}` reads.
        run: u64,
    },
    /// Refused before anything was sealed.
    Refused {
        /// Why.
        error: String,
    },
}

/// Read one run's rendered state.
#[aether_data::kind(name = "aether.bloomery.benchmark.read", eq)]
pub struct ReadBenchmark {
    /// The handle a start returned.
    pub run: u64,
}

/// One run's rendered state, or nothing under that handle.
#[aether_data::kind(name = "aether.bloomery.benchmark.read_result", eq)]
pub enum ReadBenchmarkResult {
    /// The run.
    Ok {
        /// Its rendered state.
        run: BenchmarkRun,
    },
    /// No run under that handle — a mistyped one, or a run a restart ended
    /// ([`RUN_VOLATILITY`]).
    NotFound,
}

/// The self-addressed wake the runner's poll timer fires each interval.
#[aether_data::kind(name = "aether.bloomery.benchmark.tick", default, eq)]
pub struct BenchmarkTick {}

/// Where a run stands.
#[aether_data::kind(name = "aether.bloomery.benchmark.status", eq)]
pub enum BenchmarkStatus {
    /// Cells remain; one bloom is sealed or about to be.
    Running,
    /// Every cell's bloom reached a terminal status.
    Finished,
    /// The sequence stopped early and will not resume.
    Aborted {
        /// Why it stopped.
        reason: String,
    },
}

/// What one cell's bloom is doing.
#[aether_data::kind(name = "aether.bloomery.benchmark.cell_state", eq)]
pub enum BenchmarkCellState {
    /// Not sealed yet — an earlier cell still holds the one active-bloom slot.
    Pending,
    /// Sealed and walking.
    Sealed,
    /// Reached a terminal status. The run reads the reducer's own word for it
    /// rather than collapsing to "done", because a cell that resolved by being
    /// withdrawn measured a different thing from one that landed.
    Resolved {
        /// The terminal status.
        status: BloomStatus,
    },
    /// The seal was refused, so this cell measured nothing.
    Refused {
        /// The reducer's or the door's words.
        error: String,
    },
}

/// One cell of a run: what it measures, and the bloom that measures it.
#[aether_data::kind(name = "aether.bloomery.benchmark.cell", eq)]
pub struct BenchmarkCell {
    /// The workpiece this cell's single member covers.
    pub workpiece: String,
    /// The landed pull request its order came from.
    pub pull_request: u64,
    /// The cell selector — the `ModelOverride` address the ledger's rows are
    /// attributable to.
    pub cell: Digest,
    /// Which repetition within the cell this is, from zero.
    pub sample: u32,
    /// The bloom that carries it. Known before the seal, because a sealed
    /// spec's id is its own content address.
    pub bloom: BloomId,
    /// Where it stands.
    pub state: BenchmarkCellState,
}

/// A benchmark run, as an operator reads it (ADR-0184).
#[aether_data::kind(name = "aether.bloomery.benchmark.run", eq)]
pub struct BenchmarkRun {
    /// The handle.
    pub run: u64,
    /// The golden-task set being replayed.
    pub set: String,
    /// Its version — the digest that pins exactly which comparison these cells
    /// belong to.
    pub set_version: Digest,
    /// The base every cell replays over, and the base mainline is reset to
    /// between cells.
    pub base: Digest,
    /// Where the run stands.
    pub status: BenchmarkStatus,
    /// The tasks drawn, in the order they were named.
    pub tasks: Vec<GoldenTask>,
    /// The cells, in the order they are sealed.
    pub cells: Vec<BenchmarkCell>,
    /// The ledger's own honesty boundary
    /// ([`LEDGER_CAVEAT`](aether_bloomery::LEDGER_CAVEAT)).
    pub caveat: String,
    /// The under-reporting boundary
    /// ([`COST_CAVEAT`](aether_bloomery::COST_CAVEAT)), beside the ledger's own
    /// and never inside a count.
    pub cost_caveat: String,
    /// [`RUN_VOLATILITY`].
    pub volatility: String,
}
