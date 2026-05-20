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
//! `aether-capabilities::ComponentHostCapability`, booted by chassis
//! mains via `Builder::with_actor::<ComponentHostCapability>(...)`. The
//! shared boot still wires every dependency the cap needs (engine,
//! linker, hub outbound, input subscribers) and exposes them as fields
//! the chassis main passes into `ComponentHostConfig` at the call site.
//!
//! **Hub connect is explicit.** `build()` does NOT dial
//! `AETHER_HUB_URL`. The chassis registers its own sinks and any
//! other state that should exist before the hub knows the engine is
//! alive, then dials by composing `aether_hub::HubClientCapability`
//! through `Builder::with_actor()`. Without this separation, a hub-driven
//! `load_component` could race ahead of the chassis's main thread
//! and bind a chassis sink name to a freshly-loaded component before
//! the chassis's later `register_inbox` call, panicking the substrate
//! (issue #262).
//!
//! **Env-var reading is the chassis's job.** Per issue 464,
//! substrate-core takes config explicitly and chassis `main()` is
//! the single edge that reads env vars. Tests pass config in
//! directly, never touch env. Stage 2e (issue 552) extracted every
//! cap to `aether-capabilities`; the cap-specific config readers
//! (e.g. `NamespaceRoots::from_env`) live there now and chassis
//! mains reach for them when composing the `Builder` chain.

use std::sync::Arc;

use aether_data::KindDescriptor;
use wasmtime::{Engine, Linker};

use crate::mail::registry::MailDispatch;
use crate::runtime::log_install;
use crate::runtime::panic_hook;
use crate::{
    AETHER_DIAGNOSTICS, ComponentCtx, HubOutbound, Mailer, Registry, actor::wasm::host_fns,
    handle_store::HandleStore,
};
use aether_kinds::descriptors;

/// Everything a chassis needs after shared boot setup. Fields are
/// `pub` so chassis code destructures and takes ownership of the
/// pieces it actually uses; anything unused stays on the struct and
/// gets dropped when the chassis shuts down.
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
    /// ADR-0045 typed-handle store. Sized from
    /// `AETHER_HANDLE_STORE_MAX_BYTES` (default 256 MB). Wired into
    /// `Mailer` so dispatch resolves `Ref::Handle` payloads on the
    /// way through; chassis-level handlers (PR 3 host-fn shims)
    /// will publish into it via `Mailer::handle_store()`.
    pub handle_store: Arc<HandleStore>,
    /// Retained so `connect_hub` / `connect_hub_from_env` can hand
    /// the descriptor list to `HubClient::connect`, the chassis can
    /// log the count, etc. Same `Vec` that was registered with the
    /// `Registry`.
    pub boot_descriptors: Vec<KindDescriptor>,
    /// Substrate identity passed to the boot builder. Chassis crates
    /// thread these through to `aether_hub::HubClientCapability` (via
    /// the `Hello` handshake) without re-reading their own config.
    pub name: String,
    pub version: String,
}

pub struct SubstrateBootBuilder<'a> {
    name: &'a str,
    version: &'a str,
    /// Whether this chassis enables on-disk handle persistence
    /// (ADR-0049 §9). Desktop + headless set `true`; the hub leaves it
    /// `false` (it hosts no handles, so there's nothing to persist).
    /// Defaults to `false` so a chassis that forgets to opt in stays
    /// in-memory-only rather than silently writing to the user's data
    /// dir.
    persist_enabled: bool,
}

impl SubstrateBoot {
    /// Begin a boot. `name` / `version` identify the substrate in the
    /// hub's `Hello` handshake — typically a short chassis-or-profile
    /// name (`"hello-triangle"`, `"headless"`) and
    /// `env!("CARGO_PKG_VERSION")`.
    #[must_use]
    pub fn builder<'a>(name: &'a str, version: &'a str) -> SubstrateBootBuilder<'a> {
        SubstrateBootBuilder {
            name,
            version,
            persist_enabled: false,
        }
    }
}

impl<'a> SubstrateBootBuilder<'a> {
    /// Opt this chassis into ADR-0049 on-disk handle persistence. The
    /// desktop + headless chassis call this; the hub does not. Whether
    /// persistence actually activates still depends on the env
    /// (`AETHER_HANDLE_STORE_PERSIST_DISABLE`, data-dir resolution) —
    /// this is just the chassis vote.
    #[must_use]
    pub fn persist_enabled(mut self, enabled: bool) -> Self {
        self.persist_enabled = enabled;
        self
    }
}

impl SubstrateBootBuilder<'_> {
    /// Execute the boot: registers `aether_kinds::descriptors::all()`,
    /// wires the diagnostic sink, and prepares the runtime handles
    /// (engine, registry, mailer, linker, outbound, input subscribers)
    /// for chassis-level cap composition. Does NOT install the
    /// wasm-component supervisor — that's
    /// `aether-capabilities::ComponentHostCapability`, booted through
    /// `Builder::with_actor::<ComponentHostCapability>(...)` by the
    /// chassis main using the fields exposed on [`SubstrateBoot`].
    /// Does NOT dial the hub — chassis mains compose
    /// `aether_hub::HubClientCapability` themselves (issue #262).
    ///
    /// # Panics
    /// Panics if `aether_kinds::descriptors::all()` contains a
    /// duplicate kind id, or if any of the substrate's internal
    /// locks are poisoned during the boot sequence — fail-fast per
    /// ADR-0063: both conditions indicate a substrate-level invariant
    /// violation discovered before any user code runs.
    pub fn build(self) -> wasmtime::Result<SubstrateBoot> {
        // Issue #321: route panics through tracing so dispatcher-thread
        // crashes surface in `engine_logs` instead of vanishing to
        // stderr. Idempotent — chassis re-entries / repeated builds in
        // tests are safe.
        panic_hook::init_panic_hook();

        let outbound = HubOutbound::disconnected();
        // Issue #581: install the actor-aware tracing subscriber stack.
        log_install::init_subscriber();

        let engine = Arc::new(Engine::default());
        let registry = Arc::new(Registry::new());

        let boot_descriptors = descriptors::all();
        for d in &boot_descriptors {
            registry
                .register_kind_with_descriptor(d.clone())
                .expect("duplicate kind in substrate init");
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
            AETHER_DIAGNOSTICS,
            Arc::new(|dispatch: MailDispatch<'_>| {
                let kind_name = dispatch.kind_name;
                let bytes = dispatch.payload;
                if kind_name == <aether_kinds::UnresolvedMail as aether_data::Kind>::NAME
                    && let Ok(record) =
                        bytemuck::try_from_bytes::<aether_kinds::UnresolvedMail>(bytes)
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
                    kind = %kind_name,
                    "aether.diagnostics received an unexpected kind or malformed payload",
                );
            }),
        );

        let handle_store = Arc::new(HandleStore::from_env_persistent(self.persist_enabled));
        // ADR-0049 §5: start the background disk-eviction tick. No-op
        // when persistence is disabled (hub chassis / test fixtures).
        handle_store.spawn_eviction_thread();
        let queue = Arc::new(
            Mailer::new(Arc::clone(&registry), Arc::clone(&handle_store))
                .with_outbound(Arc::clone(&outbound)),
        );

        let mut linker: Linker<ComponentCtx> = Linker::new(&engine);
        host_fns::register(&mut linker)?;
        let linker = Arc::new(linker);

        Ok(SubstrateBoot {
            engine,
            registry,
            linker,
            queue,
            outbound,
            handle_store,
            boot_descriptors,
            name: self.name.to_owned(),
            version: self.version.to_owned(),
        })
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
    /// chassis composes `aether_hub::HubClientCapability` via the
    /// `Builder::with_actor()` path instead, so `build()` is structurally
    /// incapable of reaching the hub. This test asserts the
    /// substrate-core invariant: `build()` returns a fully-wired
    /// boot whose `outbound` is disconnected.
    #[test]
    fn build_does_not_dial_hub() {
        let boot = SubstrateBoot::builder("test", env!("CARGO_PKG_VERSION"))
            .build()
            .expect("build must succeed without dialling the hub");
        // The boot is alive; chassis sinks can be registered without
        // racing a hub-driven load.
        boot.registry
            .register_inbox("test_chassis_sink", Arc::new(|_dispatch| {}));
        // No backend attached → `is_connected()` is false. Chassis
        // crates that want a hub bridge wire `HubClientCapability`
        // themselves through their `Builder`.
        assert!(!boot.outbound.is_connected());
    }
}
