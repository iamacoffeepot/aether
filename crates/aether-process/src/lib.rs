//! `aether.process` cap (ADR-0157). One chassis-owned mailbox exposing
//! one-shot subprocess execution as a mail-addressable edge capability
//! alongside `aether.fs` and `aether.http`: any actor — a wasm component
//! or a native cap — can mail `aether.process.run` to run a *permitted*
//! binary to completion and receive its captured output as a typed
//! `aether.process.run_result` reply.
//!
//! The run rides the ADR-0093 hold-until-resolve dispatch (via the
//! cap-level [`TaskQueue`](aether_substrate::actor::native::TaskQueue)
//! concurrency bound): the spawn-and-capture loop runs off the dispatcher
//! on a worker thread, the caller's settlement chain stays held across
//! the whole run, and a `#[handler(task)]` completion re-replies the
//! result. The loop itself ([`runner`]) is the single reviewed
//! implementation of the deadline / drain / group-reap discipline the
//! workspace previously hand-rolled per consumer.
//!
//! Security (ADR-0157 §Security) is the reason this is a deliberate,
//! reviewed surface rather than a thin `Command` wrapper: a
//! deny-by-default binary allowlist (empty until an operator names
//! permitted binaries), argv-only construction (never a shell string), a
//! constructed child environment (the substrate's own environment — which
//! holds fleet secrets — is never inherited), and working-directory
//! confinement to the `aether.fs` namespace root. The posture is
//! configuration, not a handler-time environment read.

// Always-on: the mail kinds carry the marker face. The handler-signature
// kinds resolve at file root because `#[actor]` emits `impl HandlesKind<K>`
// markers against the identity.
mod config;
mod kinds;

pub use config::ProcessConfig;
pub use kinds::{EnvVar, ProcessError, Run, RunResult};

// Runtime-only: the `Config`-derive layer/overlay + the substrate-typed
// runtime half (the allowlist state, the dispatch queue, the
// spawn-and-capture loop) live behind the one `feature = "runtime"` gate,
// so a marker-only build never names them nor pulls the substrate stack.
#[cfg(feature = "runtime")]
pub use config::{ProcessConfigLayer, ProcessOverlay};

#[cfg(feature = "runtime")]
mod runner;
#[cfg(feature = "runtime")]
pub use runtime::ProcessParams;

/// Default per-cap concurrency bound when `AETHER_PROCESS_MAX_IN_FLIGHT`
/// is unset. Subprocess runs are cheaper than a paid provider call but
/// still bounded so a burst cannot exhaust the host's process table.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 8;

/// Default per-run deadline in milliseconds when a `run` request carries
/// `timeout_millis == 0` and `AETHER_PROCESS_TIMEOUT_MS` is unset.
pub const DEFAULT_TIMEOUT_MILLIS: u32 = 30_000;

/// `aether.process` mailbox cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable`
/// (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind` markers, and
/// the name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`ProcessCapabilityState`, which holds the
/// allowlist + the `aether_substrate`-typed task queue) lives behind the
/// one `feature = "runtime"` gate, so a transport-only build never names
/// it nor pulls `aether_substrate` through this cap.
#[actor(singleton)]
pub struct ProcessCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate`
// type — the handler/init ctx, the runtime state, the dispatch queue, the
// spawn-and-capture loop — lives in the `runtime` module, gated once by
// `feature = "runtime"`; the `#[actor] impl` reaches all of it through the
// single `use runtime::*` glob.
use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
