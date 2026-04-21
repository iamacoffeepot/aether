//! Shared boot plumbing for substrate chassis binaries.
//!
//! ADR-0035 split peripheral code out of the runtime, but left every
//! chassis's `main()` copying ~80 lines of identical initialisation:
//! `HubOutbound` + `log_capture::init` + `Engine` + `Registry` + kind
//! descriptor loop + broadcast sink + `Mailer` + `Linker` +
//! `host_fns::register` + `Scheduler` + input subscribers +
//! `ControlPlane` + optional `AETHER_HUB_URL` connect. `SubstrateBoot`
//! folds that path into a single builder so adding a new chassis
//! (hub, web, etc.) is just its peripheral code, not another
//! reimplementation of the shared bring-up.
//!
//! The chassis handler is supplied via a closure that runs *during*
//! `build()`, after the runtime handles exist but before the
//! `ControlPlane` sink is registered. This lets the closure
//! `Arc::clone` the runtime pieces (registry, queue, outbound) it
//! needs to close over while staying on the happy path where the
//! `ControlPlane` is wired up once, not in two steps.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub, KindDescriptor};
use wasmtime::{Engine, Linker};

use crate::{
    AETHER_CONTROL, ChassisControlHandler, ControlPlane, HUB_CLAUDE_BROADCAST, HubClient,
    HubOutbound, InputSubscribers, Mailer, Registry, Scheduler, SubstrateCtx, host_fns,
    input::new_subscribers, log_capture, mail::MailboxId,
};

/// Everything a chassis needs after shared boot setup. Fields are
/// `pub` so chassis code destructures and takes ownership of the
/// pieces it actually uses; anything unused (e.g. a headless chassis
/// never touches `linker` directly, only via `ControlPlane`'s load
/// path) stays on the struct and gets dropped when the chassis
/// shuts down.
pub struct SubstrateBoot {
    pub engine: Arc<Engine>,
    pub registry: Arc<Registry>,
    pub linker: Arc<Linker<SubstrateCtx>>,
    pub queue: Arc<Mailer>,
    pub outbound: Arc<HubOutbound>,
    pub input_subscribers: InputSubscribers,
    pub broadcast_mbox: MailboxId,
    pub scheduler: Scheduler,
    /// Retained so the caller can hand the descriptor list to a late
    /// hub connect, log the count, etc. Same `Vec` that was
    /// registered with the `Registry`.
    pub boot_descriptors: Vec<KindDescriptor>,
    /// Populated iff `AETHER_HUB_URL` was set and `HubClient::connect`
    /// succeeded. Kept alive by the chassis for the process lifetime
    /// so the reader + heartbeat threads stay running.
    pub hub: Option<HubClient>,
}

/// Handles the chassis handler closure closes over when building its
/// `ChassisControlHandler`. Built by `SubstrateBootBuilder::build`
/// after the runtime pieces exist and passed to the closure by
/// reference so it can `Arc::clone` what it needs without taking
/// ownership away from the boot itself.
pub struct ChassisHandlerContext<'a> {
    pub registry: &'a Arc<Registry>,
    pub queue: &'a Arc<Mailer>,
    pub outbound: &'a Arc<HubOutbound>,
}

type ChassisHandlerFactory =
    Box<dyn FnOnce(&ChassisHandlerContext<'_>) -> Option<ChassisControlHandler>>;

pub struct SubstrateBootBuilder<'a> {
    name: &'a str,
    version: &'a str,
    workers: usize,
    build_handler: ChassisHandlerFactory,
    /// When `true`, `build()` skips the `AETHER_HUB_URL` env-var check
    /// and leaves `boot.hub = None` unconditionally. Set by the hub
    /// chassis (ADR-0034 Phase 2) since the hub is the hub — there is
    /// no upstream parent to dial, and attaching to its own listener
    /// would deadlock the tokio runtime at boot.
    skip_upstream_hub: bool,
}

impl SubstrateBoot {
    /// Begin a boot. `name` / `version` identify the substrate in the
    /// hub's `Hello` handshake — typically a short chassis-or-profile
    /// name (`"hello-triangle"`, `"headless"`) and
    /// `env!("CARGO_PKG_VERSION")`.
    pub fn builder<'a>(name: &'a str, version: &'a str) -> SubstrateBootBuilder<'a> {
        SubstrateBootBuilder {
            name,
            version,
            workers: 2,
            build_handler: Box::new(|_| None),
            skip_upstream_hub: false,
        }
    }
}

impl<'a> SubstrateBootBuilder<'a> {
    /// Scheduler worker count. Default 2.
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers;
        self
    }

    /// Skip the `AETHER_HUB_URL` env-var check during `build()`.
    /// The hub chassis (ADR-0034 Phase 2) uses this because it *is*
    /// the hub — no upstream to dial, and self-dialling before the
    /// listener task is running would deadlock the tokio runtime.
    /// The hub chassis wires an in-process loopback to
    /// `boot.outbound` after `build()` returns.
    pub fn skip_upstream_hub(mut self) -> Self {
        self.skip_upstream_hub = true;
        self
    }

    /// Supply the closure that constructs the chassis's
    /// `ChassisControlHandler`. The closure runs during `build()`
    /// once `registry` / `queue` / `outbound` exist, and returns
    /// `None` for chassis that don't own any control kinds (e.g.
    /// early tests or a future chassis that maps every peripheral
    /// kind through a different path).
    pub fn chassis_handler<F>(mut self, build: F) -> Self
    where
        F: FnOnce(&ChassisHandlerContext<'_>) -> Option<ChassisControlHandler> + 'static,
    {
        self.build_handler = Box::new(build);
        self
    }

    /// Execute the boot: registers `aether_kinds::descriptors::all()`,
    /// wires the broadcast + control-plane sinks, starts the
    /// scheduler's workers, and optionally dials the hub at
    /// `AETHER_HUB_URL`. Chassis-specific sinks (desktop's `render`,
    /// headless's nop `render`, etc.) are registered by the chassis
    /// after this returns.
    pub fn build(self) -> wasmtime::Result<SubstrateBoot> {
        let outbound = HubOutbound::disconnected();
        log_capture::init(Arc::clone(&outbound));

        let engine = Arc::new(Engine::default());
        let registry = Arc::new(Registry::new());

        let boot_descriptors = aether_kinds::descriptors::all();
        for d in &boot_descriptors {
            registry
                .register_kind_with_descriptor(d.clone())
                .expect("duplicate kind in substrate init");
        }

        let broadcast_mbox = {
            let outbound = Arc::clone(&outbound);
            registry.register_sink(
                HUB_CLAUDE_BROADCAST,
                Arc::new(
                    move |_kind_id: u64,
                          kind_name: &str,
                          origin: Option<&str>,
                          _sender,
                          bytes: &[u8],
                          _count: u32| {
                        if kind_name.is_empty() {
                            tracing::warn!(
                                target: "aether_substrate::broadcast",
                                "{HUB_CLAUDE_BROADCAST} received mail with unregistered kind — dropping",
                            );
                            return;
                        }
                        outbound.send(EngineToHub::Mail(EngineMailFrame {
                            address: ClaudeAddress::Broadcast,
                            kind_name: kind_name.to_owned(),
                            payload: bytes.to_vec(),
                            origin: origin.map(str::to_owned),
                        }));
                    },
                ),
            )
        };

        let queue = Arc::new(Mailer::new());

        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        host_fns::register(&mut linker)?;
        let linker = Arc::new(linker);

        let scheduler = Scheduler::new(Arc::clone(&registry), Arc::clone(&queue), self.workers);

        let input_subscribers = new_subscribers();

        let chassis_handler = {
            let ctx = ChassisHandlerContext {
                registry: &registry,
                queue: &queue,
                outbound: &outbound,
            };
            (self.build_handler)(&ctx)
        };

        let control_plane = ControlPlane {
            engine: Arc::clone(&engine),
            linker: Arc::clone(&linker),
            registry: Arc::clone(&registry),
            queue: Arc::clone(&queue),
            outbound: Arc::clone(&outbound),
            components: scheduler.components().clone(),
            input_subscribers: Arc::clone(&input_subscribers),
            default_name_counter: Arc::new(AtomicU64::new(0)),
            chassis_handler,
        };
        registry.register_sink(AETHER_CONTROL, control_plane.into_sink_handler());

        let hub = if self.skip_upstream_hub {
            None
        } else {
            match std::env::var("AETHER_HUB_URL") {
                Ok(url) => match HubClient::connect(
                    url.as_str(),
                    self.name,
                    self.version,
                    boot_descriptors.clone(),
                    Arc::clone(&registry),
                    Arc::clone(&queue),
                    Arc::clone(&outbound),
                ) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::error!(
                            target: "aether_substrate::boot",
                            url = %url,
                            error = %e,
                            "hub connect failed",
                        );
                        None
                    }
                },
                Err(_) => None,
            }
        };

        Ok(SubstrateBoot {
            engine,
            registry,
            linker,
            queue,
            outbound,
            input_subscribers,
            broadcast_mbox,
            scheduler,
            boot_descriptors,
            hub,
        })
    }
}
