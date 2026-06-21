//! aether-substrate: runtime that every substrate chassis shares.
//!
//! Hosts the wasmtime engine, the mail router, the kind manifest, the
//! reply-handle table, and the hub-socket client. Chassis-specific
//! peripherals (window, GPU, TCP listener, event loop) live in the
//! chassis crate that binds this as a dependency. See ADR-0035.
//!
//! Each loaded wasm component runs as an `aether_capabilities::trampoline::WasmTrampoline`
//! — a `NativeActor` instanced under `aether.embedded:NAME`
//! that delegates incoming mail to the wasm guest via `#[fallback]`
//! (issue 634 Phase 4; trampoline moved to capabilities by issue 654
//! so its `Addressable::NAMESPACE` is the single cap-owned declaration of
//! the prefix). The chassis-side `ComponentHostCapability`
//! (in `aether-capabilities`) shrinks to a `LoadComponent` handler
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

// The `#[actor] impl NativeActor for X` macro emits
// `impl ::aether_substrate::NativeDispatch for X` so external callers
// (caps in `aether-capabilities`, user-crate caps) resolve
// unambiguously. For impls written *inside* aether-substrate the
// `::aether_substrate` prefix is in-crate; the self-alias makes
// absolute paths resolve without a separate "internal vs external"
// macro arm. (Pre-issue-654 the wasm trampoline was one such in-crate
// impl; post-654 it lives in `aether-capabilities`, but the alias
// stays because future substrate-internal `#[actor]` impls would hit
// the same need.)
extern crate self as aether_substrate;

pub mod actor;
pub mod atomic_write;
// iamacoffeepot/aether#1275: `boot` builds a wasmtime `Engine` + `Linker`,
// so it rides the `wasm` feature. Default-on; only `aether-derive`'s
// trybuild fixtures opt out (they don't reach the boot path).
#[cfg(feature = "wasm")]
pub mod boot;
pub mod capture;
pub mod chassis;
pub mod config;
pub mod handle_store;
pub mod mail;
pub mod pid_lock;
#[cfg(feature = "render")]
pub mod render;
pub mod runtime;
pub mod scheduler;
#[cfg(test)]
mod test_util;
pub mod transform;

pub use actor::monitor::MonitorHandle;
pub use actor::native::binding::NativeBinding;
pub use actor::native::ctx::{ExportedHandles, NativeCtx, NativeInitCtx};
// ADR-0112: the per-handler ctx reply-mode markers, re-exported next to
// `NativeCtx` so chassis / harness code naming `NativeCtx<'_, Manual>`
// reaches them without an `aether_actor` import.
pub use actor::native::envelope::Envelope;
pub use actor::native::spawn::{SpawnBuilder, SpawnError, Spawner, Subname};
pub use actor::native::{NativeActor, NativeDispatch};
pub use actor::registry::{ActorEntry, ActorRegistry, MonitorEntry, MonitorError};
#[cfg(feature = "wasm")]
pub use actor::wasm::component::{Component, ComponentCtx};
pub use aether_actor::Addressable;
pub use aether_actor::{Manual, ReplyMode, Single, Stream};
pub use aether_derive::Config;
#[cfg(feature = "wasm")]
pub use boot::{SubstrateBoot, SubstrateBootBuilder};
pub use chassis::Chassis;
pub use chassis::builder::{
    Builder, BuilderState, BuiltChassis, DriverCapability, DriverCtx, DriverRunning, HasDriver,
    NeverDriver, NeverDriverRunning, NoDriver, PassiveChassis, RunError,
};
pub use chassis::ctx::{
    ChassisCtx, DropOnShutdownClaim, FallbackRouter, MailboxClaim, MailboxSender, SharedActorSlots,
};
pub use chassis::error::BootError;
pub use chassis::inbox::{InboundMail, SettlingInbox};
pub use config::{
    ConfigError, FromArgvThenEnv, KnobKind, KnobRecord, KnownKeys, RingCapacities, dump_config,
    known_keys, validate_env,
};
pub use mail::mailer::Mailer;
pub use mail::outbound::{
    DroppingBackend, EgressBackend, EgressEvent, HubOutbound, RecordingBackend,
};
pub use mail::registry::{InboxHandler, InlineHandler, MailboxEntry, OwnedDispatch, Registry};
pub use mail::{KindId, Mail, MailKind, MailRef, MailboxId, Source, SourceAddr};
pub use runtime::panic_hook::init_panic_hook;

/// Well-known mailbox name for substrate-level diagnostic events
/// delivered back to this engine. Today the only kind delivered here
/// is `aether.mail.unresolved` (issue #185), pushed by the hub when
/// an engine's bubbled-up mail (ADR-0037) can't be resolved at the
/// hub either. The sink handler re-warns via `tracing::warn!` so the
/// diagnostic surfaces in this engine's own `engine_logs`.
pub const AETHER_DIAGNOSTICS: &str = "aether.diagnostics";
