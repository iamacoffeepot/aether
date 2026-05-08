//! aether-substrate: runtime that every substrate chassis shares.
//!
//! Hosts the wasmtime engine, the mail router, the kind manifest, the
//! reply-handle table, and the hub-socket client. Chassis-specific
//! peripherals (window, GPU, TCP listener, event loop) live in the
//! chassis crate that binds this as a dependency. See ADR-0035.
//!
//! Each loaded wasm component runs as a `WasmTrampoline` —
//! a `NativeActor` instanced under `aether.component.trampoline:NAME`
//! that delegates incoming mail to the wasm guest via `#[fallback]`
//! (issue 634 Phase 4). The trampoline lives in
//! `aether-capabilities`; the substrate-side
//! `ComponentHostCapability` shrinks to a `LoadComponent` handler
//! that spawns the trampoline (and forwarders for `DropComponent` /
//! `ReplaceComponent`). Phase 4 PR 2 retired the per-frame drain
//! barrier and the `DrainSummary` / `DrainDeath` / `DrainOutcome`
//! aggregate types: trampoline traps now fail-fast directly via
//! `NativeBinding::fatal_abort` at the trap site (ADR-0063).
//!
//! The `Chassis` trait (ADR-0035, redefined by ADR-0071) is universal
//! but intentionally narrow: `const PROFILE` (the chassis's stable
//! identifier — `"desktop"`, `"headless"`, `"hub"`, `"test-bench"`),
//! `type Driver: DriverCapability` (the capability that owns the main
//! thread), `type Env` (resolved-config bag), and
//! `fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError>`.
//! The chassis instance you `run()` is the [`BuiltChassis<Self>`] the
//! trait method returns, not a value of `Self` itself.

// Issue 552 stage 2: the `#[actor] impl NativeActor for X` macro
// emits `impl ::aether_substrate::NativeDispatch for X` so external
// callers (caps in user crates, `aether-capabilities` once the move
// in stage 2c lands) resolve unambiguously. For caps written *inside*
// aether-substrate (today: every cap under `capabilities/`) the
// `::aether_substrate` prefix is in-crate; the self-alias makes
// absolute paths resolve without a separate "internal vs external"
// macro arm.
extern crate self as aether_substrate;

pub mod actor;
pub mod boot;
pub mod capture;
pub mod chassis;
pub mod handle_store;
pub mod input;
pub mod mail;
#[cfg(feature = "render")]
pub mod render;
pub mod runtime;

pub use actor::monitor::MonitorHandle;
pub use actor::native::binding::NativeBinding;
pub use actor::native::ctx::{ExportedHandles, NativeCtx, NativeInitCtx};
pub use actor::native::envelope::Envelope;
pub use actor::native::spawn::{SpawnBuilder, SpawnError, Spawner, Subname};
pub use actor::native::{NativeActor, NativeDispatch};
pub use actor::registry::{ActorEntry, ActorRegistry, MonitorEntry, MonitorError};
pub use actor::wasm::component::{Component, ComponentCtx};
pub use aether_actor::Actor;
pub use boot::{SubstrateBoot, SubstrateBootBuilder};
pub use chassis::Chassis;
pub use chassis::builder::{
    Builder, BuilderState, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, HasDriver,
    NeverDriver, NeverDriverRunning, NoDriver, PassiveChassis, RunError,
};
pub use chassis::ctx::{
    ActorErased, BootedChassis, ChassisBuilder, ChassisCtx, DropOnShutdownClaim, FallbackRouter,
    FrameBoundClaim, MailboxClaim, MailboxSender,
};
pub use chassis::error::{BootError, WedgedFrameBound};
pub use input::{InputSubscribers, new_subscribers, remove_from_all, subscribers_for};
pub use mail::mailer::Mailer;
pub use mail::outbound::{
    DroppingBackend, EgressBackend, EgressEvent, HubOutbound, LogEntry, LogLevel, RecordingBackend,
};
pub use mail::registry::{MailboxEntry, MailboxHandler, Registry};
pub use mail::{KindId, Mail, MailKind, MailboxId, ReplyTarget, ReplyTo};
pub use runtime::panic_hook::init_panic_hook;

/// Well-known mailbox name for substrate-level diagnostic events
/// delivered back to this engine. Today the only kind delivered here
/// is `aether.mail.unresolved` (issue #185), pushed by the hub when
/// an engine's bubbled-up mail (ADR-0037) can't be resolved at the
/// hub either. The sink handler re-warns via `tracing::warn!` so the
/// diagnostic surfaces in this engine's own `engine_logs`.
pub const AETHER_DIAGNOSTICS: &str = "aether.diagnostics";
