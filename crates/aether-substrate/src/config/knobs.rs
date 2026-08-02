//! The resolved knob values the substrate hands down its boot seams: the
//! ADR-0165 registry queue bounds, the per-actor ring capacities, and the
//! scheduler hot-path tuning. Substrate-core never reads env (issue 464) — a
//! chassis-bin `#[derive(Config)]` knob resolves each of these bundle-side and
//! lowers it to the plain `Copy` value below, which then rides the builder and
//! spawner seams as an ordinary argument.

use aether_actor::log::DEFAULT_RING_CAP;
use aether_actor::trace::{DEFAULT_TRACE_RING_CAP, DEFAULT_TRACE_RING_MAX_CAP};

/// Default admission bound for the ADR-0165 registry owner queue, in
/// commands. Sized so a legitimate burst never touches it — a birth storm
/// parks tens of envelopes, and a batch is one per handler flush — while a
/// sender spraying nonexistent recipients is capped at a few megabytes of
/// owner-held memory rather than growing without limit.
pub const DEFAULT_REGISTRY_OWNER_QUEUE_CAPACITY: usize = 4096;

/// Default admission bound for the ADR-0165 route relay queue, in
/// continuations. The relay's whole inflow is work the owner already
/// committed to delivering, so crossing this bound is counted and warned
/// rather than shed — see [`RegistryQueueCapacities::relay`].
pub const DEFAULT_REGISTRY_RELAY_QUEUE_CAPACITY: usize = 4096;

/// The two ADR-0165 serialized-queue admission bounds, resolved once at
/// chassis boot and handed to the registry owner and route relay leases at
/// attach. `Copy` so it rides the builder seam as an ordinary value. The
/// chassis-bin `RegistryQueueConfig` derive-`Config` knob lowers to this;
/// substrate-core never reads env (issue 464), so the resolution lives
/// bundle-side and only the resolved capacities reach here.
///
/// The two bounds mean different things because their traffic differs, and
/// the difference is the point of the split (issue 4122):
///
/// - the **owner** queue carries a genuinely sheddable class — an ordinary
///   route-view miss, whose volume no engine component controls — alongside
///   a reserved class (effect batches, activation cancellations, activation
///   barriers) whose loss is a correctness failure. Its bound refuses the
///   sheddable class and never the reserved one.
/// - the **relay** queue carries only continuations the owner already
///   decided the fate of, including the parked FIFO released at Live
///   publication. Dropping one loses mail the registry promised to deliver
///   and breaks the ADR's birth-ordering contract, so the relay never sheds;
///   its bound is the declared pressure line whose crossing is counted
///   (`over_capacity`) and warned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistryQueueCapacities {
    /// Owner-queue admission bound in commands (env
    /// `AETHER_REGISTRY_OWNER_QUEUE_CAPACITY`; default
    /// [`DEFAULT_REGISTRY_OWNER_QUEUE_CAPACITY`]). At or past this depth an
    /// ordinary route-view miss is shed to the existing unknown-recipient
    /// policy instead of being parked; effect batches, activation
    /// cancellations, and activation barriers are always admitted.
    pub owner: usize,
    /// Relay-queue pressure bound in continuations (env
    /// `AETHER_REGISTRY_RELAY_QUEUE_CAPACITY`; default
    /// [`DEFAULT_REGISTRY_RELAY_QUEUE_CAPACITY`]). Continuations past this
    /// depth are still admitted — the class is not sheddable — and counted.
    pub relay: usize,
}

impl Default for RegistryQueueCapacities {
    fn default() -> Self {
        Self { owner: DEFAULT_REGISTRY_OWNER_QUEUE_CAPACITY, relay: DEFAULT_REGISTRY_RELAY_QUEUE_CAPACITY }
    }
}

/// The two per-actor ring capacities resolved once at chassis boot and
/// threaded down the spawn path (ADR-0081 log ring + ADR-0086 trace
/// ring). `Copy` so it rides every `Spawner` / builder seam as an
/// ordinary value — no process-global, no atomics. The chassis-bin
/// `ActorRingConfig` derive-`Config` knob lowers to this; substrate-core
/// never reads env (issue 464), so the resolution lives bundle-side and
/// only the resolved capacities reach here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingCapacities {
    /// Per-actor [`ActorLogRing`](aether_actor::log::ActorLogRing)
    /// capacity (chassis-boot key `AETHER_ACTOR_LOG_RING_SIZE`; default
    /// [`DEFAULT_RING_CAP`]).
    pub log: usize,
    /// Per-actor [`ActorTraceRing`](aether_actor::trace::ActorTraceRing)
    /// and chassis-host-ring *floor* capacity — the size each ring starts
    /// at (chassis-boot key `AETHER_ACTOR_TRACE_RING_SIZE`; default
    /// [`DEFAULT_TRACE_RING_CAP`]).
    pub trace: usize,
    /// Ceiling a saturating trace ring grows to before it resumes
    /// drop-oldest (chassis-boot key `AETHER_ACTOR_TRACE_RING_MAX_SIZE`; default
    /// [`DEFAULT_TRACE_RING_MAX_CAP`]). The trace ring grows geometrically
    /// from [`trace`](Self::trace) toward this; the log ring has no such
    /// ceiling (drop-oldest is its intended semantic).
    pub trace_max: usize,
}

impl Default for RingCapacities {
    fn default() -> Self {
        Self { log: DEFAULT_RING_CAP, trace: DEFAULT_TRACE_RING_CAP, trace_max: DEFAULT_TRACE_RING_MAX_CAP }
    }
}

/// The nine scheduler hot-path tuning knobs resolved once at chassis boot
/// and installed into the scheduler's process-global before the pool
/// starts (`crate::scheduler::install_tuning`). `Copy` so it rides the
/// builder seam as an ordinary value; the deep hot-path getters (the
/// worker loop, the blob-flush recruiter, the handoff-EWMA seed) read the
/// installed value rather than env. The chassis-bin `SchedulerTuningConfig`
/// derive-`Config` knob lowers to this; substrate-core never reads env
/// (issue 464), so the resolution lives bundle-side and only the resolved
/// values reach here.
///
/// Six knobs carry concrete defaults; the three adaptive knobs
/// ([`time_budget_micros`](Self::time_budget_micros),
/// [`handoff_cost_nanos`](Self::handoff_cost_nanos),
/// [`wake_cost_nanos`](Self::wake_cost_nanos)) are `Option` — `None`
/// selects the measured/derived behaviour, `Some` pins the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerTuning {
    /// Route-to-spinner spin-window (microseconds) before a worker parks
    /// (chassis-boot key `AETHER_SPIN_WINDOW_USEC`; default `50`).
    pub spin_window_micros: u64,
    /// Deque-length backstop: max slots a worker keeps on its own deque
    /// before forcing a spill (chassis-boot key `AETHER_LOCAL_STICKY_MAX`; default
    /// `256`).
    pub local_sticky_max: usize,
    /// Keep-local time valve (microseconds): `Some` pins/disables the
    /// burst spill valve (`0` disables it), `None` derives it from the
    /// measured handoff cost (chassis-boot key `AETHER_LOCAL_TIME_BUDGET_US`; default
    /// `None`).
    pub time_budget_micros: Option<u64>,
    /// Whether idle workers may raid siblings' deques (peer-deque
    /// stealing); default owner-only (chassis-boot key `AETHER_PEER_STEAL`; default
    /// `false`).
    pub peer_steal: bool,
    /// Every-K injector backstop for keep-local chains (env
    /// `AETHER_LOCAL_CHAIN_BACKSTOP`; default `64`).
    pub local_chain_backstop: u32,
    /// Pins the cross-worker handoff-cost estimate (nanoseconds) and
    /// freezes live refinement; `None` boot-probes and live-refines (env
    /// `AETHER_HANDOFF_COST_NS`; default `None`).
    pub handoff_cost_nanos: Option<u64>,
    /// Minimum fresh-group count for a flush to broadcast-recruit siblings
    /// (chassis-boot key `AETHER_BLOB_RECRUIT_MIN`; default `9`).
    pub blob_recruit_min: usize,
    /// Cap on the number of sibling copies a single flush injects when
    /// recruiting (chassis-boot key `AETHER_BLOB_RECRUIT_MAX`; default `32`).
    pub blob_recruit_max: usize,
    /// Pins the recruit wake break-even (nanoseconds) and freezes live
    /// refinement; `None` uses the box-measured handoff cost (env
    /// `AETHER_WAKE_COST_NANOS`; default `None`).
    pub wake_cost_nanos: Option<u64>,
}

impl Default for SchedulerTuning {
    fn default() -> Self {
        Self {
            spin_window_micros: 50,
            local_sticky_max: 256,
            time_budget_micros: None,
            peer_steal: false,
            local_chain_backstop: 64,
            handoff_cost_nanos: None,
            blob_recruit_min: 9,
            blob_recruit_max: 32,
            wake_cost_nanos: None,
        }
    }
}
