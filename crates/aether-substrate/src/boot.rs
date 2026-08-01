//! Shared boot plumbing for substrate chassis binaries.
//!
//! ADR-0035 split peripheral code out of the runtime, but left every
//! chassis's `main()` copying ~80 lines of identical initialisation:
//! `HubOutbound` + `log_install::init_subscriber` + `Engine` +
//! `Registry` + kind descriptor loop + broadcast sink + `Mailer` +
//! `Linker` + `host_fns::register` + input subscribers. `SubstrateBoot`
//! folds that path into a single builder so adding a new chassis (hub,
//! web, etc.) is just its peripheral code, not another reimplementation
//! of the shared bring-up.
//!
//! Issue 603 retired the substrate-side construction of the
//! `ControlPlane` sink. The wasm-component supervisor is now
//! `aether-component::ComponentHostCapability`, booted by chassis
//! mains via `Builder::with_actor::<ComponentHostCapability>(...)`. The
//! shared boot still wires every dependency the cap needs (engine,
//! linker, hub outbound, input subscribers) and exposes them as fields
//! the chassis main passes into `ComponentHostConfig` at the call site.
//!
//! **Hub connect is explicit.** `build()` does NOT open the engine to
//! the hub. The chassis registers its own sinks and any other state
//! that should exist before the hub knows the engine is alive, then
//! accepts the hub's connection by composing
//! `aether_rpc::RpcServerCapability` through `Builder::with_actor()`
//! (the hub dials the substrate). Without this separation, a hub-driven
//! `load_component` could race ahead of the chassis's main thread
//! and bind a chassis sink name to a freshly-loaded component before
//! the chassis's later `register_inbox` call, panicking the substrate
//! (issue #262).
//!
//! **Env-var reading is the chassis's job.** Per issue 464,
//! substrate-core takes config explicitly and chassis `main()` is
//! the single edge that reads env vars. Tests pass config in
//! directly, never touch env. Stage 2e (issue 552) extracted every
//! cap out of the substrate; the cap-specific config readers (e.g.
//! `NamespaceRoots::from_env`) live in the per-cap crates now and
//! chassis mains reach for them when composing the `Builder` chain.

use std::sync::Arc;

use aether_data::KindDescriptor;
use wasmtime::{Engine, Linker};

use crate::actor::native::local as actor_local;
use crate::mail::registry::{BootAuthority, MailDispatch};
use crate::runtime::log_install;
use crate::runtime::panic_hook;
use crate::{AETHER_DIAGNOSTICS, ComponentCtx, HubOutbound, Mailer, Registry, actor::wasm::host_fns};
use aether_kinds::descriptors;

/// Everything a chassis needs after shared boot setup. The handle
/// fields are `pub` so chassis code destructures and takes ownership of
/// the pieces it actually uses; anything unused stays on the struct and
/// gets dropped when the chassis shuts down. The one exception is the
/// [`BootAuthority`], which is private behind [`Self::take_authority`]
/// because it is spent rather than shared (iamacoffeepot/aether#4171).
///
/// Issue 603: `engine`, `linker`, `outbound` are the inputs
/// `ComponentHostCapability` consumes through `ComponentHostConfig`
/// when the chassis main installs the supervisor via
/// `Builder::with_actor::<ComponentHostCapability>(...)`. The substrate
/// boot doesn't construct the cap itself — it just holds the
/// dependencies the cap will need.
///
/// Issue 640 collapsed the shared `InputSubscribers: Arc<RwLock<...>>`
/// — `aether.input` is the sole owner of the subscriber table and
/// drivers / `ComponentHostCapability` write to it via mail.
pub struct SubstrateBoot {
    pub engine: Arc<Engine>,
    pub registry: Arc<Registry>,
    pub linker: Arc<Linker<ComponentCtx>>,
    pub queue: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    /// Retained so `connect_hub` / `connect_hub_from_env` can hand
    /// the descriptor list to `HubClient::connect`, the chassis can
    /// log the count, etc. Same `Vec` that was registered with the
    /// `Registry`.
    pub boot_descriptors: Vec<KindDescriptor>,
    /// iamacoffeepot/aether#4156: the boot path's proof that it may write
    /// the registry directly, ahead of the ADR-0165 owner. Private and
    /// one-shot (iamacoffeepot/aether#4171): a composition delta borrows it
    /// through [`Self::authority`], and the single composition point spends it
    /// through [`Self::take_authority`] as soon as that delta returns — so the
    /// token's reach is the composition rather than the whole life of the boot
    /// handle, which every chassis moves into its driver and keeps well past
    /// the seal. A handler never receives one.
    authority: Option<BootAuthority>,
}

impl SubstrateBoot {
    /// Execute the boot: registers `aether_kinds::descriptors::all()`,
    /// wires the diagnostic sink, and prepares the runtime handles
    /// (engine, registry, mailer, linker, outbound, input subscribers)
    /// for chassis-level cap composition. Does NOT install the
    /// wasm-component supervisor — that's
    /// `aether-component::ComponentHostCapability`, booted through
    /// `Builder::with_actor::<ComponentHostCapability>(...)` by the
    /// chassis main using the fields exposed on [`SubstrateBoot`].
    /// Does NOT open the engine to the hub — chassis mains compose
    /// `aether_rpc::RpcServerCapability` themselves so the hub can dial
    /// in (issue #262).
    ///
    /// # Panics
    /// Panics if `aether_kinds::descriptors::all()` contains a
    /// duplicate kind id, or if any of the substrate's internal
    /// locks are poisoned during the boot sequence — fail-fast per
    /// ADR-0063: both conditions indicate a substrate-level invariant
    /// violation discovered before any user code runs.
    pub fn build() -> wasmtime::Result<Self> {
        // Issue #321: route panics through tracing so dispatcher-thread
        // crashes surface in `engine_logs` instead of vanishing to
        // stderr. Idempotent — chassis re-entries / repeated builds in
        // tests are safe.
        panic_hook::init_panic_hook();

        let outbound = HubOutbound::disconnected();
        // Issue #581: install the actor-aware tracing subscriber stack.
        log_install::init_subscriber();
        // #2070: install the host thread-local backend for `aether_actor::Local`
        // before any actor dispatch stamps it.
        actor_local::install();

        let engine = Arc::new(Engine::default());
        let registry = Arc::new(Registry::new());

        let authority = BootAuthority::new();
        let boot_descriptors = descriptors::all();
        for d in &boot_descriptors {
            registry.register_kind_with_descriptor(&authority, d.clone()).expect("duplicate kind in substrate init");
        }

        // Diagnostic sink for hub → originating-engine typo reports
        // (ADR-0037 follow-up, issue #185). Re-emits the unresolved-
        // mail record as a local `tracing::warn!` so the detail
        // surfaces in this engine's own `engine_logs` rather than only
        // in the hub's. Kind vocabulary is `aether.mail.unresolved`
        // today; the sink is structured as a general diagnostic
        // channel so future diagnostic kinds can land here without
        // needing another sink.
        //
        // Issue 838: registered as `Sink` (not `Closure`) so the
        // `Mailer::push` route brackets the inline handler with
        // `Received`/`Finished`. The handler runs synchronously
        // (just emits a `tracing::warn!`) — there's no actor
        // dispatch loop behind it, so without the bracket the
        // chain's `in_flight` would leak.
        registry.register_inline(
            &authority,
            AETHER_DIAGNOSTICS,
            Arc::new(|dispatch: MailDispatch<'_>| {
                let kind = dispatch.kind;
                let bytes = dispatch.payload;
                if kind == <aether_kinds::UnresolvedMail as aether_data::Kind>::ID
                    && let Ok(record) = bytemuck::try_from_bytes::<aether_kinds::UnresolvedMail>(bytes)
                {
                    tracing::warn!(
                        target: "aether_substrate::diagnostics",
                        recipient_mailbox_id = %record.recipient_mailbox_id,
                        kind_id = %record.kind_id,
                        "hub could not resolve bubbled-up mail recipient (ADR-0037); \
                         mail dropped. Likely a typoed mailbox name at the sender.",
                    );
                    return;
                }
                tracing::warn!(
                    target: "aether_substrate::diagnostics",
                    kind = %kind,
                    "aether.diagnostics received an unexpected kind or malformed payload",
                );
            }),
        );

        let queue = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(Arc::clone(&outbound)));

        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        host_fns::register(&mut linker)?;
        let linker = Arc::new(linker);

        Ok(Self { engine, registry, linker, queue, outbound, boot_descriptors, authority: Some(authority) })
    }

    /// Borrow the boot path's [`BootAuthority`] — the proof a composition
    /// delta needs to name the registry's direct mutators
    /// (`register_inline`, `try_register_inbox_with_id`,
    /// `register_kind_with_descriptor`) while the chassis is still composing.
    /// `None` once the token has been spent.
    ///
    /// The borrow is the bound: it lives no longer than the `&SubstrateBoot`
    /// it came from, so a delta can use the token but cannot stash it. The
    /// sibling of [`ChassisCtx::boot_authority`], which lends the same proof
    /// to a capability's own boot pass.
    ///
    /// [`ChassisCtx::boot_authority`]: crate::ChassisCtx::boot_authority
    #[must_use]
    pub fn authority(&self) -> Option<&BootAuthority> {
        self.authority.as_ref()
    }

    /// Move the boot path's [`BootAuthority`] out of this handle, or `None`
    /// if it was already spent.
    ///
    /// ADR-0165's seal argument rests on every `BootAuthority` mint being
    /// "spent or dropped" before `Spawner::seal` runs. The shared boot's mint
    /// was the one holder that did not honour that: the handle outlives the
    /// seal (every chassis moves it into its driver), so a `pub` field made
    /// the direct registry mutators nameable long after the owner took over
    /// (iamacoffeepot/aether#4171). Taking the token is what spends it, and
    /// [`composed`](crate::chassis::composed) does so unconditionally once a
    /// chassis's composition delta has run — for every chassis, whether or not
    /// its delta ever asked for the token.
    ///
    /// A second take is a genuine double-compose of the same boot, which the
    /// composition point reports as [`BootError::AlreadyComposed`]. There is
    /// no assertion and no panic here: #4154 is the standing reason a
    /// diagnostic near this machinery is expensive.
    ///
    /// [`BootError::AlreadyComposed`]: crate::chassis::error::BootError::AlreadyComposed
    pub fn take_authority(&mut self) -> Option<BootAuthority> {
        self.authority.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build()` must NOT dial the hub. Issue #262: hub-driven
    /// `load_component` running before the chassis registers its
    /// sinks can race ahead and bind a chassis sink name to a
    /// component, panicking the substrate when the chassis later
    /// tries to install the real sink handler. ADR-0070 phase 4 /
    /// ADR-0071 phase 7 retired `boot.connect_hub` entirely — the
    /// chassis composes `aether_rpc::RpcServerCapability` via the
    /// `Builder::with_actor()` path instead so the hub can dial in, so
    /// `build()` is structurally incapable of reaching the hub. This test asserts the
    /// substrate-core invariant: `build()` returns a fully-wired
    /// boot whose `outbound` is disconnected.
    #[test]
    fn build_does_not_dial_hub() {
        let boot = SubstrateBoot::build().expect("build must succeed without dialling the hub");
        // The boot is alive; chassis sinks can be registered without
        // racing a hub-driven load.
        let authority = boot.authority().expect("a fresh boot still holds its authority");
        boot.registry.register_inbox(authority, "test_chassis_sink", Arc::new(|_dispatch| {}));
        // No backend attached → `is_connected()` is false. Chassis
        // crates that want a hub bridge wire `RpcServerCapability`
        // themselves through their `Builder`.
        assert!(!boot.outbound.is_connected());
    }
}
