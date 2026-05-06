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
//! `aether-capabilities::ControlPlaneCapability`, booted by chassis
//! mains via `Builder::with_actor::<ControlPlaneCapability>(...)`. The
//! shared boot still wires every dependency the cap needs (engine,
//! linker, hub outbound, input subscribers) and exposes them as fields
//! the chassis main passes into `ControlPlaneConfig` at the call site.
//!
//! **Hub connect is explicit.** `build()` does NOT dial
//! `AETHER_HUB_URL`. The chassis registers its own sinks and any
//! other state that should exist before the hub knows the engine is
//! alive, then dials by composing `aether_hub::HubClientCapability`
//! through `Builder::with()`. Without this separation, a hub-driven
//! `load_component` could race ahead of the chassis's main thread
//! and bind a chassis sink name to a freshly-loaded component before
//! the chassis's later `register_sink` call, panicking the substrate
//! (issue #262).
//!
//! **Env-var reading is the chassis's job.** Per issue 464,
//! substrate-core takes config explicitly and chassis `main()` is
//! the single edge that reads env vars. Tests pass config in
//! directly, never touch env. Stage 2e (issue 552) extracted every
//! cap to `aether-capabilities`; the cap-specific config readers
//! (e.g. `NamespaceRoots::from_env`) live there now and chassis
//! mains reach for them when calling [`SubstrateBoot::add_actor`].

use std::sync::Arc;

use aether_data::KindDescriptor;
use wasmtime::{Engine, Linker};

use crate::{
    AETHER_DIAGNOSTICS, BootedChassis, ChassisBuilder, HubOutbound, InputSubscribers, Mailer,
    Registry, SubstrateCtx, handle_store::HandleStore, host_fns, input::new_subscribers,
};

/// Everything a chassis needs after shared boot setup. Fields are
/// `pub` so chassis code destructures and takes ownership of the
/// pieces it actually uses; anything unused stays on the struct and
/// gets dropped when the chassis shuts down.
///
/// Issue 603: `engine`, `linker`, `outbound`, `input_subscribers` are
/// the inputs `ControlPlaneCapability` consumes through
/// `ControlPlaneConfig` when the chassis main installs the
/// supervisor via `Builder::with_actor::<ControlPlaneCapability>(...)`.
/// The substrate boot doesn't construct the cap itself — it just
/// holds the dependencies the cap will need.
pub struct SubstrateBoot {
    pub engine: Arc<Engine>,
    pub registry: Arc<Registry>,
    pub linker: Arc<Linker<SubstrateCtx>>,
    pub queue: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    pub input_subscribers: InputSubscribers,
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
    /// ADR-0070 native capabilities booted during shared bring-up.
    /// Stage 2e (issue 552) extracted every cap to `aether-capabilities`,
    /// so the substrate boot no longer pre-installs any cap — chassis
    /// mains call [`Self::add_actor`] for each one
    /// (`HandleCapability` is universal and goes first;
    /// chassis-conditional `Audio` / `Render` / `Log` / `Net` / `Io` /
    /// `ControlPlane` follow). Exposed publicly so chassis binaries
    /// that want explicit control can `take()` and call
    /// [`BootedChassis::shutdown`] themselves.
    pub chassis: Option<BootedChassis>,
    /// Substrate identity passed to the boot builder. Chassis crates
    /// thread these through to `aether_hub::HubClientCapability` (via
    /// the `Hello` handshake) without re-reading their own config.
    pub name: String,
    pub version: String,
}

pub struct SubstrateBootBuilder<'a> {
    name: &'a str,
    version: &'a str,
}

impl SubstrateBoot {
    /// Begin a boot. `name` / `version` identify the substrate in the
    /// hub's `Hello` handshake — typically a short chassis-or-profile
    /// name (`"hello-triangle"`, `"headless"`) and
    /// `env!("CARGO_PKG_VERSION")`.
    pub fn builder<'a>(name: &'a str, version: &'a str) -> SubstrateBootBuilder<'a> {
        SubstrateBootBuilder { name, version }
    }

    /// Dial `url` and start the hub reader + heartbeat threads.
    /// Returns `Ok(Some(client))` on success — the chassis MUST keep
    /// the client alive (typically by stashing it in its own struct)
    /// for those threads to stay running. `Ok(None)` if `url` is
    /// `None` (substrate runs locally, no hub). `Err` propagates a
    /// hub-connect failure (TCP refused, handshake timeout, etc.) so
    /// the chassis can decide whether to fail the boot or run
    /// hub-disconnected.
    ///
    /// Call this **after** every chassis sink is registered (and any
    /// other state that should exist before the hub knows about the
    /// engine). Before this returns, no hub-driven `load_component`
    /// can race the chassis's setup. See issue #262.
    ///
    /// Per issue 464, this is the substrate-core entry point — the
    /// chassis main reads `AETHER_HUB_URL` from env and passes it
    /// through. `connect_hub_from_env` is a thin wrapper for callers
    /// that still want the env-driven behaviour inline.
    /// Boot one more capability into the chassis the shared boot
    /// already started. Wrapper around [`BootedChassis::add`] that
    /// supplies the substrate's own registry + mailer handles, so
    /// chassis mains compose chassis-conditional capabilities with
    /// one line per capability:
    ///
    /// ```ignore
    /// // Stage 2e: caps live in `aether-capabilities`; chassis mains
    /// // reach in there for the type and call `add_actor` per-cap.
    /// boot.add_actor::<aether_capabilities::HandleCapability>(())?;
    /// boot.add_actor::<aether_capabilities::LogCapability>(())?;
    /// ```
    ///
    /// Boot order is call order; shutdown order is the reverse, the
    /// same as build-time capabilities. The fallback-router slot is
    /// shared across build-time and post-build capabilities, so the
    /// single-claim invariant holds.
    ///
    /// Pre-PR-E3 there was a separate `add_facade` for actor caps
    /// alongside `add_capability` for legacy `Capability` caps; the
    /// legacy path retired alongside `Capability` itself.
    pub fn add_capability<C>(&mut self, cap: C) -> wasmtime::Result<()>
    where
        C: aether_actor::Actor + aether_actor::Dispatch + Send + 'static,
    {
        let chassis = self
            .chassis
            .as_mut()
            .expect("SubstrateBoot::build always installs a BootedChassis");
        chassis
            .add(&self.registry, &self.queue, cap)
            .map_err(|e| wasmtime::Error::msg(format!("capability boot failed: {e}")))
    }

    /// Issue 552 stage 2: post-build entry for a `NativeActor`.
    /// Mirror of [`Self::add_capability`] for the new cap shape.
    pub fn add_actor<A>(&mut self, config: A::Config) -> wasmtime::Result<()>
    where
        A: crate::NativeActor + crate::NativeDispatch,
    {
        let chassis = self
            .chassis
            .as_mut()
            .expect("SubstrateBoot::build always installs a BootedChassis");
        chassis
            .add_actor::<A>(&self.registry, &self.queue, config)
            .map_err(|e| wasmtime::Error::msg(format!("capability boot failed: {e}")))
    }
}

impl<'a> SubstrateBootBuilder<'a> {
    /// Execute the boot: registers `aether_kinds::descriptors::all()`,
    /// wires the diagnostic sink, and prepares the runtime handles
    /// (engine, registry, mailer, linker, outbound, input subscribers)
    /// for chassis-level cap composition. Does NOT install the
    /// wasm-component supervisor — that's
    /// `aether-capabilities::ControlPlaneCapability`, booted through
    /// `Builder::with_actor::<ControlPlaneCapability>(...)` by the
    /// chassis main using the fields exposed on [`SubstrateBoot`].
    /// Does NOT dial the hub — chassis mains compose
    /// `aether_hub::HubClientCapability` themselves (issue #262).
    pub fn build(self) -> wasmtime::Result<SubstrateBoot> {
        // Issue #321: route panics through tracing so dispatcher-thread
        // crashes surface in `engine_logs` instead of vanishing to
        // stderr. Idempotent — chassis re-entries / repeated builds in
        // tests are safe.
        crate::panic_hook::init_panic_hook();

        let outbound = HubOutbound::disconnected();
        // Issue #581: install the actor-aware tracing subscriber stack.
        crate::log_install::init_subscriber();

        let engine = Arc::new(Engine::default());
        let registry = Arc::new(Registry::new());

        let boot_descriptors = aether_kinds::descriptors::all();
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
        registry.register_sink(
            AETHER_DIAGNOSTICS,
            Arc::new(
                |_kind: aether_data::KindId,
                 kind_name: &str,
                 _origin: Option<&str>,
                 _sender,
                 bytes: &[u8],
                 _count: u32| {
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
                },
            ),
        );

        let queue = Arc::new(Mailer::new());
        queue.wire(Arc::clone(&registry));
        queue.wire_outbound(Arc::clone(&outbound));
        let handle_store = Arc::new(HandleStore::from_env());
        queue.wire_handle_store(Arc::clone(&handle_store));
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&queue))
            .build()
            .map_err(|e| wasmtime::Error::msg(format!("chassis capability boot: {e}")))?;

        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        host_fns::register(&mut linker)?;
        let linker = Arc::new(linker);

        let input_subscribers = new_subscribers();

        Ok(SubstrateBoot {
            engine,
            registry,
            linker,
            queue,
            outbound,
            input_subscribers,
            handle_store,
            boot_descriptors,
            chassis: Some(chassis),
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
    /// `Builder::with()` path instead, so `build()` is structurally
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
            .register_sink("test_chassis_sink", Arc::new(|_, _, _, _, _, _| {}));
        // No backend attached → `is_connected()` is false. Chassis
        // crates that want a hub bridge wire `HubClientCapability`
        // themselves through their `Builder`.
        assert!(!boot.outbound.is_connected());
    }
}
