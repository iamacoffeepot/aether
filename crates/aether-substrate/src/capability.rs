//! ADR-0070 Phase 1: capability trait, chassis builder, and ctx.
//!
//! This module is purely additive. Existing chassis boot paths
//! (`SubstrateBoot::builder`) keep working unchanged; nothing yet
//! consumes the new builder. Phases 2–5 migrate each native sink
//! (handle, log, io, net, audio, render+camera) into a submodule of
//! `crate::capabilities` that implements [`Capability`]; Phase 4
//! wires the dispatch path to consult [`ChassisCtx::claim_fallback_router`]
//! and removes the substrate-side bubble-up in `Mailer`.
//!
//! The shape mirrors a wasm component (kinds + dispatcher + state +
//! lifecycle) but compiled in: a native capability owns mailboxes,
//! a Rust dispatcher, Rust state, and a `boot`/`shutdown` lifecycle.
//! See ADR-0070 for the full rationale.
//!
//! # Phase 1 scope
//!
//! - Trait + builder + ctx wiring against an `Arc<Registry>` and
//!   `Arc<Mailer>` supplied by the chassis.
//! - [`ChassisCtx::claim_mailbox`] registers an mpsc-fed sink on the
//!   registry under the given name and hands the capability the
//!   receiver. The registered handler converts each borrowed sink
//!   call into an owned [`Envelope`] and forwards it.
//! - [`ChassisCtx::claim_fallback_router`] stores a single fallback
//!   handler; substrate dispatch does not consult it yet (Phase 4).
//! - No sinks are migrated yet — `crate::capabilities` is an empty
//!   submodule placeholder that future PRs populate.

use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;
use std::sync::mpsc;

use crate::mail::{KindId, MailboxId, ReplyTo};
use crate::mailer::Mailer;
use crate::registry::{NameConflict, Registry};

/// One mail delivered to a capability through its mpsc receiver.
///
/// Sinks today receive borrowed args (`&str`, `&[u8]`); routing across
/// an mpsc channel forces ownership. Capabilities that care about
/// ergonomics destructure this once at the top of their loop.
#[derive(Debug)]
pub struct Envelope {
    pub kind: KindId,
    pub kind_name: String,
    pub origin: Option<String>,
    pub sender: ReplyTo,
    pub payload: Vec<u8>,
    pub count: u32,
}

/// Result returned from [`ChassisCtx::claim_mailbox`].
///
/// The capability owns the receiver afterward; the slot is consumed
/// from the registry, so a second claim for the same name fails
/// loud with [`BootError::MailboxAlreadyClaimed`].
#[derive(Debug)]
pub struct MailboxClaim {
    pub id: MailboxId,
    pub receiver: mpsc::Receiver<Envelope>,
}

/// Generic fallback-router handler: invoked by substrate dispatch when a
/// local mailbox lookup misses. Phase 1 stores the handler but does
/// not call it; Phase 4 wires `Mailer::push` to consult the slot in
/// place of today's hub-specific bubble-up.
///
/// Returning `true` means "I handled this mail" (substrate does nothing
/// further); `false` means "not mine" (substrate falls through to its
/// warn-drop path). Today only `HubClientCapability` will claim the
/// slot; other implementations are possible (test routers, multi-hub
/// fan-out).
pub type FallbackRouter = Arc<dyn Fn(&Envelope) -> bool + Send + Sync + 'static>;

/// Failure modes capability boot can raise. Per ADR-0063, any boot
/// error aborts the chassis before user code runs — no partial boots.
#[derive(Debug)]
pub enum BootError {
    /// The mailbox name is already bound, either to another
    /// capability that claimed it earlier or to a legacy
    /// `Registry::register_sink` call from `SubstrateBoot::build`.
    /// Phase 2-5 expect this during the side-by-side period and
    /// remove the legacy registration in the same diff.
    MailboxAlreadyClaimed { name: String },
    /// A second capability tried to register a fallback router after
    /// one was already installed. The slot is single-claim by design.
    FallbackRouterAlreadyClaimed,
    /// Anything else a capability's boot wants to surface.
    Other(Box<dyn StdError + Send + Sync + 'static>),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootError::MailboxAlreadyClaimed { name } => {
                write!(f, "mailbox {name:?} already claimed")
            }
            BootError::FallbackRouterAlreadyClaimed => {
                f.write_str("fallback router slot already claimed")
            }
            BootError::Other(e) => write!(f, "capability boot failed: {e}"),
        }
    }
}

impl StdError for BootError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            BootError::Other(e) => Some(&**e),
            _ => None,
        }
    }
}

impl From<NameConflict> for BootError {
    fn from(e: NameConflict) -> Self {
        BootError::MailboxAlreadyClaimed { name: e.name }
    }
}

/// Forward wasmtime errors raised during chassis boot
/// (`SubstrateBoot::build`, `add_capability`, hub-client connect, etc.)
/// into [`BootError::Other`]. Any wasmtime error during boot is
/// definitionally a boot error — chassis trait impls can `?` the
/// wasmtime call directly without per-call `.map_err` glue.
impl From<wasmtime::Error> for BootError {
    fn from(e: wasmtime::Error) -> Self {
        BootError::Other(Box::new(std::io::Error::other(format!("{e}"))))
    }
}

/// A native capability: chassis-policy code that owns one or more
/// mailboxes plus the state behind them. Each capability is
/// `boot()`-ed once during chassis startup and `shutdown()`-ed once
/// during teardown.
///
/// Implementors define `Self::Running` to describe the post-boot
/// handle (typically a struct holding spawned thread joins, the
/// retained mail-send handle, and any state shared with the
/// dispatcher).
pub trait Capability: Send + 'static {
    type Running: RunningCapability;
    fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError>;
}

/// The post-boot handle returned by [`Capability::boot`]. The chassis
/// owns it for the rest of the run; on shutdown the chassis calls
/// `shutdown(self: Box<Self>)`, which is responsible for joining any
/// dispatcher threads the capability spawned.
pub trait RunningCapability: Send {
    fn shutdown(self: Box<Self>);
}

/// Kernel-side handle bundle exposed to a capability during its
/// `boot()` call. Shared (`&mut`) across every `boot()` in the
/// builder — one ctx per build, threaded through the capability list
/// in declaration order (ADR-0070 resolved decision 4).
pub struct ChassisCtx<'a> {
    registry: &'a Arc<Registry>,
    mailer: &'a Arc<Mailer>,
    fallback: &'a mut Option<FallbackRouter>,
}

impl<'a> ChassisCtx<'a> {
    /// Internal constructor used by [`ChassisBuilder::build`] and the
    /// ADR-0071 [`crate::chassis_builder::Builder`].
    pub(crate) fn new(
        registry: &'a Arc<Registry>,
        mailer: &'a Arc<Mailer>,
        fallback: &'a mut Option<FallbackRouter>,
    ) -> Self {
        Self {
            registry,
            mailer,
            fallback,
        }
    }

    /// Register an mpsc-fed sink under `name` and return both its
    /// derived [`MailboxId`] (ADR-0029 hash) and the receiver.
    ///
    /// The closure registered with the registry forwards every
    /// delivery into the sender side of the mpsc pair, so the
    /// capability's dispatcher loop is `while let Ok(env) =
    /// claim.receiver.recv() { ... }`. The receiver lives until the
    /// capability drops it; the matching sender lives in the sink
    /// closure stored on the registry until the registry itself is
    /// dropped.
    pub fn claim_mailbox(&mut self, name: &str) -> Result<MailboxClaim, BootError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let tx = Arc::new(tx);
        let id = self.registry.try_register_sink(
            name.to_owned(),
            Arc::new(
                move |kind: KindId,
                      kind_name: &str,
                      origin: Option<&str>,
                      sender: ReplyTo,
                      payload: &[u8],
                      count: u32| {
                    let env = Envelope {
                        kind,
                        kind_name: kind_name.to_owned(),
                        origin: origin.map(str::to_owned),
                        sender,
                        payload: payload.to_vec(),
                        count,
                    };
                    if tx.send(env).is_err() {
                        tracing::warn!(
                            target: "aether_substrate::capability",
                            kind = kind_name,
                            "capability mailbox receiver dropped — mail discarded"
                        );
                    }
                },
            ),
        )?;
        Ok(MailboxClaim { id, receiver: rx })
    }

    /// Clone-able mail-send handle. Capabilities stash this into
    /// their dispatcher state to send mail to other mailboxes
    /// (including other capabilities). Same `Arc<Mailer>` every
    /// capability sees, so an envelope sent here goes through the
    /// substrate's routing table the same way component-originated mail
    /// does.
    pub fn mail_send_handle(&self) -> Arc<Mailer> {
        Arc::clone(self.mailer)
    }

    /// Borrow the chassis's registry. Capabilities that resolve
    /// names or descriptors at boot (today: the hub client capability
    /// cloning the registry into its TCP reader thread) reach for
    /// this; most capabilities don't need it.
    pub fn registry(&self) -> &Arc<Registry> {
        self.registry
    }

    /// Borrow the chassis's mailer. Same shape as
    /// [`Self::mail_send_handle`] but returns a borrow instead of a
    /// clone — preferred when the capability is going to clone with
    /// `Arc::clone` itself.
    pub fn mailer(&self) -> &Arc<Mailer> {
        self.mailer
    }

    /// Install the fallback-router handler. At most one capability
    /// may claim the slot; a second call returns
    /// [`BootError::FallbackRouterAlreadyClaimed`].
    ///
    /// Phase 1 stores the handler but does not consult it from substrate
    /// dispatch. Phase 4 wires `Mailer::push` against this slot and
    /// removes today's hub-specific `Mailer.outbound` field, at
    /// which point `HubClientCapability` (in
    /// `aether-substrate-bundle::hub`) claims the slot to forward
    /// unresolved mail over TCP.
    pub fn claim_fallback_router(&mut self, handler: FallbackRouter) -> Result<(), BootError> {
        if self.fallback.is_some() {
            return Err(BootError::FallbackRouterAlreadyClaimed);
        }
        *self.fallback = Some(handler);
        Ok(())
    }
}

/// Type-erased boot trampoline so [`ChassisBuilder`] can collect
/// heterogeneous capability types into one `Vec`. Each entry, when
/// invoked, takes the ctx, calls the underlying `Capability::boot`,
/// and boxes the resulting `Running` handle.
type BootFn =
    Box<dyn FnOnce(&mut ChassisCtx<'_>) -> Result<Box<dyn RunningCapability>, BootError> + Send>;

/// Declarative chassis composition. Capabilities are added in
/// declaration order (ADR-0070 resolved decision 3); `build()` boots
/// them in the same order. The first failure aborts the build and
/// shuts down any capabilities that already booted, so no chassis
/// observes a partially-booted state.
pub struct ChassisBuilder {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    pending: Vec<BootFn>,
}

impl ChassisBuilder {
    /// Construct a fresh builder against the given substrate handles.
    /// Phase 1 leaves it to the caller to supply these — Phase 6
    /// (TestBench rewrite) folds substrate construction into the
    /// builder; until then, the existing `SubstrateBoot::build` is
    /// the construction site.
    pub fn new(registry: Arc<Registry>, mailer: Arc<Mailer>) -> Self {
        Self {
            registry,
            mailer,
            pending: Vec::new(),
        }
    }

    /// Append a capability. Boot order is declaration order.
    pub fn with<C>(mut self, cap: C) -> Self
    where
        C: Capability,
    {
        self.pending.push(Box::new(move |ctx| {
            let running = cap.boot(ctx)?;
            Ok(Box::new(running) as Box<dyn RunningCapability>)
        }));
        self
    }

    /// Boot every capability. On the first error, already-booted
    /// capabilities are shut down in reverse order before the error
    /// propagates — no partial-boot state is ever returned.
    pub fn build(self) -> Result<BootedChassis, BootError> {
        let ChassisBuilder {
            registry,
            mailer,
            pending,
        } = self;
        let mut fallback: Option<FallbackRouter> = None;
        let mut booted: Vec<Box<dyn RunningCapability>> = Vec::with_capacity(pending.len());
        for boot in pending {
            let mut ctx = ChassisCtx::new(&registry, &mailer, &mut fallback);
            match boot(&mut ctx) {
                Ok(running) => booted.push(running),
                Err(e) => {
                    while let Some(c) = booted.pop() {
                        c.shutdown();
                    }
                    return Err(e);
                }
            }
        }
        Ok(BootedChassis {
            running: booted,
            _fallback: fallback,
        })
    }
}

/// The output of [`ChassisBuilder::build`]. Holds every booted
/// capability's `Running` handle plus the (optionally claimed)
/// fallback router. On `shutdown` the capabilities are torn down in
/// reverse boot order so later-booted state can rely on
/// earlier-booted state during its own shutdown.
pub struct BootedChassis {
    running: Vec<Box<dyn RunningCapability>>,
    /// Held for the lifetime of the chassis; Phase 4 will read this
    /// from `Mailer` dispatch. Today it's just owned-and-not-called.
    _fallback: Option<FallbackRouter>,
}

impl fmt::Debug for BootedChassis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootedChassis")
            .field("running", &self.running.len())
            .field("fallback_claimed", &self._fallback.is_some())
            .finish()
    }
}

impl BootedChassis {
    /// Number of booted capabilities. Useful for tests and boot
    /// logs; not expected to vary at runtime.
    pub fn len(&self) -> usize {
        self.running.len()
    }

    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// Boot one more capability into an already-built chassis. The
    /// capability sees the same `ChassisCtx` shape as those booted
    /// through [`ChassisBuilder::with`] — the same registry, the
    /// same mail-send handle, and (crucially) the same fallback-
    /// router slot, so the single-claim invariant still holds across
    /// the build-time and post-build sets.
    ///
    /// Used by chassis mains to compose chassis-conditional
    /// capabilities (`LogCapability`, `IoCapability`, etc.) on top
    /// of the universal capabilities `SubstrateBoot::build` already
    /// installed. Boots run in call order; shutdown tears down in
    /// reverse, exactly like the build-time path.
    pub fn add<C>(
        &mut self,
        registry: &Arc<Registry>,
        mailer: &Arc<Mailer>,
        cap: C,
    ) -> Result<(), BootError>
    where
        C: Capability,
    {
        let mut ctx = ChassisCtx::new(registry, mailer, &mut self._fallback);
        let running = cap.boot(&mut ctx)?;
        self.running.push(Box::new(running));
        Ok(())
    }

    /// Tear down every capability in reverse boot order. Idempotent
    /// with [`Drop`] — calling `shutdown` first leaves [`Drop`] with
    /// nothing to do.
    pub fn shutdown(mut self) {
        self.shutdown_in_place();
    }

    fn shutdown_in_place(&mut self) {
        while let Some(c) = self.running.pop() {
            c.shutdown();
        }
    }
}

impl Drop for BootedChassis {
    fn drop(&mut self) {
        // Forgotten-shutdown safety net: the chassis owner can drop
        // a `BootedChassis` without calling `shutdown` and still
        // get its capability dispatcher threads joined. Phase 2's
        // `HandleCapability` polls a flag every 100ms, so worst-case
        // drop latency is one poll interval per capability.
        self.shutdown_in_place();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::ReplyTo;
    use crate::registry::MailboxEntry;
    use std::sync::Mutex;

    /// Test-only capability that claims one mailbox and records
    /// every envelope it receives plus whether shutdown ran.
    struct EchoCapability {
        name: &'static str,
        log: Arc<Mutex<Vec<Envelope>>>,
        shutdown_flag: Arc<Mutex<bool>>,
    }

    struct EchoRunning {
        receiver: mpsc::Receiver<Envelope>,
        log: Arc<Mutex<Vec<Envelope>>>,
        shutdown_flag: Arc<Mutex<bool>>,
    }

    impl Capability for EchoCapability {
        type Running = EchoRunning;
        fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
            let claim = ctx.claim_mailbox(self.name)?;
            Ok(EchoRunning {
                receiver: claim.receiver,
                log: self.log,
                shutdown_flag: self.shutdown_flag,
            })
        }
    }

    impl RunningCapability for EchoRunning {
        fn shutdown(self: Box<Self>) {
            // Drain any pending envelopes synchronously so tests can
            // assert against `log` after `shutdown` returns.
            while let Ok(env) = self.receiver.try_recv() {
                self.log.lock().unwrap().push(env);
            }
            *self.shutdown_flag.lock().unwrap() = true;
        }
    }

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        (Arc::new(Registry::new()), Arc::new(Mailer::new()))
    }

    fn deliver(registry: &Registry, name: &str, payload: &[u8]) {
        let id = registry.lookup(name).expect("mailbox registered");
        let MailboxEntry::Sink(handler) = registry.entry(id).expect("entry exists") else {
            panic!("expected sink entry for {name}");
        };
        handler(KindId(42), "test.kind", None, ReplyTo::NONE, payload, 1);
    }

    #[test]
    fn capability_claims_mailbox_and_receives_mail() {
        let (registry, mailer) = fresh_substrate();
        let log = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(Mutex::new(false));

        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(EchoCapability {
                name: "test.echo",
                log: Arc::clone(&log),
                shutdown_flag: Arc::clone(&flag),
            })
            .build()
            .expect("build succeeds");
        assert_eq!(chassis.len(), 1);

        deliver(&registry, "test.echo", b"hello");
        deliver(&registry, "test.echo", b"world");

        chassis.shutdown();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].payload, b"hello");
        assert_eq!(log[1].payload, b"world");
        assert!(*flag.lock().unwrap());
    }

    #[test]
    fn duplicate_mailbox_claim_fails_with_loud_error() {
        let (registry, mailer) = fresh_substrate();
        // Pre-register the name to simulate the side-by-side period
        // where legacy `register_sink` and a new capability would
        // both target the same mailbox.
        registry.register_sink("test.collide", Arc::new(|_, _, _, _, _, _| {}));

        let log = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(Mutex::new(false));
        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(EchoCapability {
                name: "test.collide",
                log,
                shutdown_flag: flag,
            })
            .build()
            .expect_err("build must reject duplicate claim");
        assert!(
            matches!(err, BootError::MailboxAlreadyClaimed { ref name } if name == "test.collide")
        );
    }

    #[test]
    fn boot_failure_shuts_down_already_booted_capabilities() {
        let (registry, mailer) = fresh_substrate();
        // First capability claims a fresh name; the second is set up
        // to fail by pre-registering its target name.
        registry.register_sink("test.fail.second", Arc::new(|_, _, _, _, _, _| {}));

        let first_flag = Arc::new(Mutex::new(false));
        let second_flag = Arc::new(Mutex::new(false));
        let log = Arc::new(Mutex::new(Vec::new()));

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(EchoCapability {
                name: "test.fail.first",
                log: Arc::clone(&log),
                shutdown_flag: Arc::clone(&first_flag),
            })
            .with(EchoCapability {
                name: "test.fail.second",
                log: Arc::clone(&log),
                shutdown_flag: Arc::clone(&second_flag),
            })
            .build()
            .expect_err("second capability must fail");
        assert!(matches!(err, BootError::MailboxAlreadyClaimed { .. }));
        assert!(
            *first_flag.lock().unwrap(),
            "first capability shut down on boot abort"
        );
        assert!(
            !*second_flag.lock().unwrap(),
            "second capability never booted"
        );
    }

    #[test]
    fn fallback_router_slot_is_single_claim() {
        let (registry, mailer) = fresh_substrate();

        struct FallbackCap {
            should_succeed: bool,
        }
        struct FallbackRunning;
        impl Capability for FallbackCap {
            type Running = FallbackRunning;
            fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
                let handler: FallbackRouter = Arc::new(|_env: &Envelope| true);
                ctx.claim_fallback_router(handler)?;
                if self.should_succeed {
                    Ok(FallbackRunning)
                } else {
                    Err(BootError::Other("unreachable".into()))
                }
            }
        }
        impl RunningCapability for FallbackRunning {
            fn shutdown(self: Box<Self>) {}
        }

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(FallbackCap {
                should_succeed: true,
            })
            .with(FallbackCap {
                should_succeed: true,
            })
            .build()
            .expect_err("second fallback claim must fail");
        assert!(matches!(err, BootError::FallbackRouterAlreadyClaimed));
    }

    #[test]
    fn mail_send_handle_clones_to_same_mailer() {
        let (registry, mailer) = fresh_substrate();

        struct ProbeCap {
            captured: Arc<Mutex<Option<Arc<Mailer>>>>,
        }
        struct ProbeRunning;
        impl Capability for ProbeCap {
            type Running = ProbeRunning;
            fn boot(self, ctx: &mut ChassisCtx<'_>) -> Result<Self::Running, BootError> {
                *self.captured.lock().unwrap() = Some(ctx.mail_send_handle());
                Ok(ProbeRunning)
            }
        }
        impl RunningCapability for ProbeRunning {
            fn shutdown(self: Box<Self>) {}
        }

        let captured = Arc::new(Mutex::new(None));
        ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(ProbeCap {
                captured: Arc::clone(&captured),
            })
            .build()
            .expect("build succeeds")
            .shutdown();

        let captured = captured.lock().unwrap().take().expect("handle captured");
        assert!(Arc::ptr_eq(&captured, &mailer));
    }
}
