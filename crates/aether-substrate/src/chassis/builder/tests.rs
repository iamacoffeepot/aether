use super::*;
use crate::actor::monitor::MonitorHandle;
use crate::actor::native::Dispatch;
use crate::actor::native::ctx::NativeCtx;
use crate::chassis::ctx::ChassisCtx;
use crate::mail::KindId;
use crate::mail::MailboxId;
use crate::mail::registry;
use crate::testing::{TestChassis, bare_substrate};
use crate::{BootError, Chassis, NativeActor, NativeInitCtx};
use aether_actor::{Addressable, HandlesKind};
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

macro_rules! pod_kind {
    ($type:ident { $field:ident: $field_ty:ty }, $name:literal, $id:expr) => {
        #[repr(C)]
        #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
        struct $type {
            $field: $field_ty,
        }

        impl aether_data::Kind for $type {
            const NAME: &'static str = $name;
            const ID: aether_data::KindId = aether_data::KindId($id);

            fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
                if bytes.len() != std::mem::size_of::<Self>() {
                    return None;
                }
                Some(bytemuck::pod_read_unaligned(bytes))
            }

            fn encode_into_bytes(&self) -> Vec<u8> {
                bytemuck::bytes_of(self).to_vec()
            }
        }
    };
}

macro_rules! count_on_kind_actor {
    ($type:ident, $namespace:literal, $kind:ty, $field:ident) => {
        struct $type {
            $field: Arc<AtomicU32>,
        }

        impl Addressable for $type {
            const NAMESPACE: &'static str = $namespace;
            type Resolver = aether_actor::Many;
        }

        impl HandlesKind<$kind> for $type {}

        impl aether_actor::Lifecycle<Self> for $type {
            type Config = Arc<AtomicU32>;
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a>;

            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { $field: config })
            }
        }

        impl NativeActor for $type {
            type State = Self;
        }

        impl Dispatch<Self> for $type {
            fn dispatch(
                state: &mut Self,
                _ctx: &mut NativeCtx<'_, crate::Manual>,
                kind: KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == <$kind as aether_data::Kind>::ID.0 {
                    let _ = <$kind as aether_data::Kind>::decode_from_bytes(payload)?;
                    state.$field.fetch_add(1, AtomicOrdering::SeqCst);
                    return Some(());
                }
                None
            }
        }
    };
}

macro_rules! close_observed_state {
    ($type:ident, $namespace:literal) => {
        struct $type {
            close_observed: Arc<AtomicU32>,
        }

        impl Addressable for $type {
            const NAMESPACE: &'static str = $namespace;
            type Resolver = aether_actor::Many;
        }

        impl aether_actor::Lifecycle<Self> for $type {
            type Config = Arc<AtomicU32>;
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a>;

            fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self { close_observed: config })
            }

            fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
                state.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }

        impl NativeActor for $type {
            type State = Self;
        }
    };
}

macro_rules! close_observed_actor {
    ($type:ident, $namespace:literal) => {
        close_observed_state!($type, $namespace);

        impl Dispatch<Self> for $type {
            fn dispatch(
                _state: &mut Self,
                _ctx: &mut NativeCtx<'_, crate::Manual>,
                _kind: KindId,
                _payload: &[u8],
            ) -> Option<()> {
                None
            }
        }
    };
}

macro_rules! shutdown_on_kind_actor {
    ($type:ident, $namespace:literal, $kind:ty) => {
        close_observed_state!($type, $namespace);

        impl HandlesKind<$kind> for $type {}

        shutdown_dispatch!($type, $kind);
    };
}

macro_rules! shutdown_dispatch {
    ($type:ident, $kind:ty) => {
        impl Dispatch<Self> for $type {
            fn dispatch(
                _state: &mut Self,
                ctx: &mut NativeCtx<'_, crate::Manual>,
                kind: KindId,
                payload: &[u8],
            ) -> Option<()> {
                if kind.0 == <$kind as aether_data::Kind>::ID.0 {
                    let _ = <$kind as aether_data::Kind>::decode_from_bytes(payload)?;
                    ctx.shutdown();
                    return Some(());
                }
                None
            }
        }
    };
}

macro_rules! unit_shutdown_actor {
    ($type:ident, $namespace:literal, $kind:ty) => {
        struct $type;

        impl Addressable for $type {
            const NAMESPACE: &'static str = $namespace;
            type Resolver = aether_actor::Many;
        }

        impl HandlesKind<$kind> for $type {}

        impl aether_actor::Lifecycle<Self> for $type {
            type Config = ();
            type InitError = BootError;
            type InitCtx<'a> = NativeInitCtx<'a>;
            type Ctx<'a> = NativeCtx<'a>;

            fn init((): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
                Ok(Self)
            }
        }

        impl NativeActor for $type {
            type State = Self;
        }

        shutdown_dispatch!($type, $kind);
    };
}

/// Lightweight passive-cap fixture for chassis-level boot tests.
/// The chassis-builder tests don't care about handler dispatch
/// (per-cap dispatch coverage lives in the per-cap crates); the
/// real caps would force a circular dep, so this stub stands in.
struct StubLog;
impl Addressable for StubLog {
    const NAMESPACE: &'static str = "test.chassis_builder.stub_log";
    type Resolver = aether_actor::One;
}

impl aether_actor::Lifecycle<Self> for StubLog {
    type Config = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;
    fn init((): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }
}

impl NativeActor for StubLog {
    type State = Self;
}

impl Dispatch<Self> for StubLog {
    fn dispatch(
        _state: &mut Self,
        _ctx: &mut NativeCtx<'_, crate::Manual>,
        _kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        None
    }
}

/// Fixture chassis for driver-build tests. Generic over the
/// concrete `DriverCapability` so each test can pair the chassis
/// type with whatever driver it's exercising.
struct DrivenTestChassis<D: DriverCapability>(PhantomData<fn() -> D>);
impl<D: DriverCapability + 'static> Chassis for DrivenTestChassis<D> {
    const PROFILE: &'static str = "test-driven";
    type Driver = D;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("DrivenTestChassis is driven by Builder::new directly in unit tests");
    }
}

/// Test driver: records that it ran, then exits.
struct RanDriver {
    ran: Arc<AtomicBool>,
}

struct RanDriverRunning {
    ran: Arc<AtomicBool>,
}

impl DriverCapability for RanDriver {
    type Running = RanDriverRunning;
    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        Ok(RanDriverRunning { ran: self.ran })
    }
}

impl DriverRunning for RanDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Driver build path: passives boot, driver runs, passives tear
/// down on chassis drop. Per-cap dispatch coverage lives in the
/// individual cap modules; this test exercises the chassis-level
/// boot + run + teardown sequence.
#[test]
fn driver_build_runs_driver_and_tears_down_passives() {
    let (registry, mailer) = bare_substrate();
    let ran = Arc::new(AtomicBool::new(false));

    let chassis = Builder::<DrivenTestChassis<RanDriver>>::new(registry, mailer)
        .with_actor::<StubLog>(())
        .driver(RanDriver { ran: Arc::clone(&ran) })
        .build()
        .expect("build succeeds");

    chassis.run().expect("driver run succeeds");
    assert!(ran.load(Ordering::SeqCst));
}

/// Test driver whose value-free ADR-0155 claim hook reserves a
/// driver-as-actor mailbox (the shape the desktop driver's
/// `aether.window` claim will take once the Env split lands). `boot` is
/// never reached by the claim-only terminal — the driver value is never
/// constructed.
struct ClaimingDriver;
struct ClaimingDriverRunning;

impl DriverCapability for ClaimingDriver {
    type Running = ClaimingDriverRunning;

    fn claim(ctx: &mut ChassisCtx<'_>) -> Result<(), BootError> {
        ctx.claim_mailbox_with_override("test.claim_only.window")?;
        Ok(())
    }

    fn boot(self, _ctx: &mut DriverCtx<'_>) -> Result<Self::Running, BootError> {
        Ok(ClaimingDriverRunning)
    }
}

impl DriverRunning for ClaimingDriverRunning {
    fn run(self: Box<Self>) -> Result<(), RunError> {
        Ok(())
    }
}

/// ADR-0155 claim-only terminal: `claim_namespaces` reports exactly the
/// namespaces the three registration contributors reserve — the
/// `with_actor` chain, an inline sink registered directly on the shared
/// registry, and the driver type's value-free claim hook — and runs ONLY
/// the Claim stage, never advancing to Init (a cap's `init` side effect
/// stays unfired). The un-fired `init` is the load-bearing proof that no
/// OS resource is touched and no worker pool starts: Init is the first
/// stage that touches OS resources (ADR-0155), and Start (dispatcher
/// threads, the pool) is strictly after it in the fused boot path, so a
/// terminal that stops before Init spawns no thread by construction.
#[test]
fn claim_namespaces_reports_all_contributors_and_skips_init() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // A cap whose `init` increments a counter — the tripwire for "Claim
    // ran, Init did not".
    struct InitTripwireCap {
        _init_count: Arc<AtomicU32>,
    }
    impl Addressable for InitTripwireCap {
        const NAMESPACE: &'static str = "test.claim_only.init_tripwire";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Lifecycle<Self> for InitTripwireCap {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            config.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Self { _init_count: config })
        }
    }
    impl NativeActor for InitTripwireCap {
        type State = Self;
    }
    impl Dispatch<Self> for InitTripwireCap {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let init_count = Arc::new(AtomicU32::new(0));

    // Inline sink registered directly on the shared registry — the
    // headless chassis's `aether.audio` fail-fast sink takes this path,
    // outside the `with_actor` chain.
    registry.register_inline("test.claim_only.inline_sink", Arc::new(|_dispatch: registry::MailDispatch<'_>| {}));

    let claimed = Builder::<DrivenTestChassis<ClaimingDriver>>::new(registry, Arc::clone(&mailer))
        .with_actor::<StubLog>(())
        .with_actor::<InitTripwireCap>(Arc::clone(&init_count))
        .claim_namespaces()
        .expect("claim-only succeeds");

    let expected: std::collections::BTreeSet<String> = [
        "test.chassis_builder.stub_log",
        "test.claim_only.init_tripwire",
        "test.claim_only.inline_sink",
        "test.claim_only.window",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(claimed, expected, "claim-only reports every claimed namespace and nothing else");

    assert_eq!(
        init_count.load(AtomicOrdering::SeqCst),
        0,
        "claim-only stops at Claim — no cap's init runs, so no OS resource is touched and no pool starts",
    );
}

/// Boot-time mailbox-claim collision aborts the build (and runs
/// the prior cap's drop). Two `StubLog` instances both claim
/// `test.chassis_builder.stub_log`; the second hits the
/// duplicate-claim guard.
#[test]
fn duplicate_passive_mailbox_aborts_build_and_shuts_down_prior() {
    let (registry, mailer) = bare_substrate();

    let err = Builder::<TestChassis>::new(registry, mailer)
        .with_actor::<StubLog>(())
        .with_actor::<StubLog>(())
        .build_passive()
        .expect_err("second passive must fail with duplicate claim");

    assert!(matches!(err, BootError::MailboxAlreadyClaimed { .. }));
}

/// Issue 607 Phase 7: a singleton whose `init` returns `Err`
/// releases its slot before `with_actor` propagates the error.
/// After the failed build, the chassis's `Registry` has no sink
/// at the cap's namespace and the `ActorRegistry`'s `name_owners`
/// no longer claims the namespace — so a fresh chassis can boot
/// a different cap with the same namespace string (or the same
/// cap with a different config) without colliding.
#[test]
fn failed_singleton_init_releases_namespace_and_sink() {
    struct FailingCap;
    impl Addressable for FailingCap {
        const NAMESPACE: &'static str = "test.phase7.failing_cap";
        type Resolver = aether_actor::One;
    }

    impl aether_actor::Lifecycle<Self> for FailingCap {
        type Config = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Err(BootError::Other(Box::new(io::Error::other("intentional init failure for Phase 7 cleanup test"))))
        }
    }
    impl NativeActor for FailingCap {
        type State = Self;
    }
    impl Dispatch<Self> for FailingCap {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<FailingCap>(())
        .build_passive()
        .expect_err("init failure must propagate");
    // The error wraps init's std::io::Error message.
    assert!(format!("{err:?}").contains("intentional init failure"), "expected init error to propagate, got {err:?}");

    // Sink at the cap's namespace must be gone — Registry::lookup
    // returns None for absent entries.
    assert!(
        registry.lookup(FailingCap::NAMESPACE).is_none(),
        "sink at {} should be removed after failed init",
        FailingCap::NAMESPACE,
    );
}

/// Issue 552 stage 1: end-to-end smoke for the new
/// [`Builder::with_actor`] boot path. Boots a hand-rolled
/// `NativeActor` fixture, looks it up via
/// [`PassiveChassis::actor`], pushes one envelope at the cap's
/// mailbox, and asserts the dispatcher routed it to the right
/// handler. Stage 1 lands the infrastructure; stage 2 migrates
/// real caps onto it. This test is the load-bearing acceptance
/// gate.
#[test]
fn with_actor_boots_dispatches_and_tears_down() {
    use crate::mail::registry::MailboxEntry;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // Fixture kind: a 4-byte cast-shape payload so encode_into_bytes
    // lands on the bytemuck path.
    pod_kind!(Ping { tag: u32 }, "test.with_actor.ping", 0xA1B2_C3D4_E5F6_0001);

    // Fixture cap. State behind interior mutability so `&self`
    // dispatch can mutate it (the post-552 norm).
    struct ProbeCap {
        received: Arc<AtomicU32>,
    }
    impl Addressable for ProbeCap {
        const NAMESPACE: &'static str = "test.with_actor.probe";
        type Resolver = aether_actor::One;
    }
    impl HandlesKind<Ping> for ProbeCap {}

    impl aether_actor::Lifecycle<Self> for ProbeCap {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received: config })
        }
    }

    impl NativeActor for ProbeCap {
        type State = Self;
    }

    // Hand-rolled Dispatch — what the macro arm emits in
    // task #731. The if-arm decodes Ping bytes, calls the
    // handler, returns Some(()) on success.
    impl Dispatch<Self> for ProbeCap {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Ping::ID.0 {
                let _decoded = Ping::decode_from_bytes(payload)?;
                state.received.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let received = Arc::new(AtomicU32::new(0));

    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<ProbeCap>(Arc::clone(&received))
        .build_passive()
        .expect("with_actor boot succeeds");

    // Issue 629 / Phase A: chassis-level `actor::<X>()` retired.
    // The cap is owned by its dispatcher thread; the test verifies
    // the cap is alive via the mail dispatch round-trip below.

    // Push one envelope at the cap's mailbox via the registry's
    // sink handler. The dispatcher thread pulls from its inbox
    // and routes through __aether_dispatch_envelope → on_ping.
    let mailbox_id = registry.lookup(<ProbeCap as Addressable>::NAMESPACE).expect("with_actor claimed the mailbox");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(mailbox_id).expect("sink registered") else {
        panic!("ProbeCap claim must be a sink entry");
    };

    let payload = Ping { tag: 0xDEAD_BEEF };
    let bytes = payload.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(<Ping as Kind>::ID, Ping::NAME, &bytes, 1));

    // Wait briefly for the dispatcher thread to dispatch.
    let deadline = Instant::now() + Duration::from_millis(500);
    while received.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        received.load(AtomicOrdering::SeqCst),
        1,
        "dispatcher should have routed Ping → on_ping within the wait budget"
    );

    drop(chassis);
}

/// Issue 582: the chassis dispatcher trampoline stamps the
/// per-actor [`ActorSlots`] into TLS
/// for the duration of `init` and each handler call. A cap that
/// reaches for `Local::with_mut` from inside both lifecycle
/// stages must see its own state — verified end-to-end here so
/// the stamping wiring can't silently regress.
#[test]
fn with_actor_stamps_local_for_init_and_handler() {
    use crate::mail::registry::MailboxEntry;
    use aether_actor::Local;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Tick { seq: u32 }, "test.local.tick", 0xA1B2_C3D4_E5F6_0002);

    // The cap holds an Arc<AtomicU32> the test reads after each
    // dispatch. The actor-local counter is keyed by `TypeId<Counter>`
    // — the chassis stamp is what makes `with_mut` resolve at
    // all (outside a stamp it would `debug_assert!` panic).
    struct LocalProbe {
        observed: Arc<AtomicU32>,
    }
    impl Addressable for LocalProbe {
        const NAMESPACE: &'static str = "test.local.probe";
        type Resolver = aether_actor::One;
    }
    impl HandlesKind<Tick> for LocalProbe {}

    // Newtype-per-slot is the Local convention: each
    // logical storage gets its own type, so two probes that
    // both want a u32 don't alias under TypeId. The
    // `#[local]` attribute is the shorthand for the
    // marker impl.
    #[derive(Default)]
    #[aether_actor::local]
    struct Counter(u32);

    impl aether_actor::Lifecycle<Self> for LocalProbe {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            // Init runs inside the chassis builder's stamp guard
            // — write a sentinel so the handler test below proves
            // the same slots are reused across init→dispatch.
            Counter::with_mut(|c| c.0 = 100);
            Ok(Self { observed: config })
        }
    }

    impl NativeActor for LocalProbe {
        type State = Self;
    }

    impl Dispatch<Self> for LocalProbe {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Tick::ID.0 {
                let _decoded = Tick::decode_from_bytes(payload)?;
                Counter::with_mut(|c| c.0 += 1);
                let snapshot = Counter::with(|c| c.0);
                state.observed.store(snapshot, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let observed = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<LocalProbe>(Arc::clone(&observed))
        .build_passive()
        .expect("LocalProbe boots");

    let mailbox_id = registry.lookup(<LocalProbe as Addressable>::NAMESPACE).expect("with_actor claimed the mailbox");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(mailbox_id).expect("sink registered") else {
        panic!("LocalProbe claim must be a sink entry");
    };

    // Three dispatches. Init seeded 100; the handler bumps once
    // per dispatch and snapshots — so observed should walk
    // 101, 102, 103 in order. We assert the final 103 with a
    // wait budget to cover dispatcher-thread scheduling.
    for seq in 0..3 {
        let payload = Tick { seq };
        let bytes = payload.encode_into_bytes();
        handler.enqueue(registry::test_owned_dispatch(<Tick as Kind>::ID, Tick::NAME, &bytes, 1));
    }

    let deadline = Instant::now() + Duration::from_millis(500);
    while observed.load(AtomicOrdering::SeqCst) != 103 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        observed.load(AtomicOrdering::SeqCst),
        103,
        "init seeded 100 + 3 handler bumps ⇒ Local at 103 (proves the same \
         ActorSlots is stamped across init and dispatch)"
    );

    drop(chassis);
}

/// Issue 607 Phase 3b verify: a singleton parent's handler calls
/// `ctx.spawn_child::<Child>(...)` to launch an instanced actor.
/// Asserts the child's `MailboxId` lands in the chassis's
/// `ActorRegistry` as a Live entry, and that the parent-pre-loaded
/// `after_init` mail dispatches as the child's first envelope.
#[test]
fn ctx_spawn_child_routes_through_handler() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Hatch { tag: u32 }, "test.spawn_child.hatch", 0xC0C1_C2C3_C4C5_C6C7);

    pod_kind!(Ping { tag: u32 }, "test.spawn_child.ping", 0xD0D1_D2D3_D4D5_D6D7);

    count_on_kind_actor!(ChildCap, "test.spawn_child.child", Ping, received);

    struct ParentCap {
        spawn_count: Arc<AtomicU32>,
        child_received: Arc<AtomicU32>,
    }
    impl Addressable for ParentCap {
        const NAMESPACE: &'static str = "test.spawn_child.parent";
        type Resolver = aether_actor::One;
    }
    impl HandlesKind<Hatch> for ParentCap {}
    impl aether_actor::Lifecycle<Self> for ParentCap {
        type Config = (Arc<AtomicU32>, Arc<AtomicU32>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((spawn_count, child_received): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { spawn_count, child_received })
        }
    }
    impl NativeActor for ParentCap {
        type State = Self;
    }
    impl Dispatch<Self> for ParentCap {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Hatch::ID.0 {
                let _ = Hatch::decode_from_bytes(payload)?;
                let _id = ctx
                    .spawn_child::<ChildCap>(Subname::Counter, Arc::clone(&state.child_received))
                    .after_init(Ping { tag: 42 })
                    .finish()
                    .expect("spawn_child must succeed");
                state.spawn_count.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let spawn_count = Arc::new(AtomicU32::new(0));
    let child_received = Arc::new(AtomicU32::new(0));

    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<ParentCap>((Arc::clone(&spawn_count), Arc::clone(&child_received)))
        .build_passive()
        .expect("ParentCap boots");

    // Push Hatch at the parent's mailbox; the parent's handler
    // calls `ctx.spawn_child::<ChildCap>` which in turn pushes a
    // Ping at the new child via the after_init bootstrap.
    let parent_id = registry.lookup(<ParentCap as Addressable>::NAMESPACE).expect("ParentCap claimed");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(parent_id).expect("sink") else {
        panic!("expected mailbox entry");
    };
    let bytes = (Hatch { tag: 1 }).encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(<Hatch as Kind>::ID, Hatch::NAME, &bytes, 1));

    let deadline = Instant::now() + Duration::from_millis(500);
    while child_received.load(AtomicOrdering::SeqCst) < 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(spawn_count.load(AtomicOrdering::SeqCst), 1, "parent's handler ran spawn_child exactly once");
    assert_eq!(
        child_received.load(AtomicOrdering::SeqCst),
        1,
        "spawn_child's after_init mail dispatched as the child's first envelope"
    );

    // Child is Live in the chassis's actor registry under the
    // ADR-0099 §3 lineage fold: the parent is a root cap (depth-1,
    // carry == id), so the child's id folds the child node's ActorId
    // onto the parent's id — not the flat `hash(NAMESPACE:subname)`.
    let child_id = MailboxId(aether_data::with_tag(
        aether_data::Tag::Mailbox,
        aether_data::fold_lineage(parent_id.0, aether_data::ActorId::instanced("test.spawn_child.child", "0")),
    ));
    assert!(
        chassis.actor_registry().is_live(child_id),
        "spawned child should be Live in the actor registry under the lineage-folded id"
    );

    drop(chassis);
}

/// Issue 607 Phase 4a verify: `ctx.shutdown()` from inside an
/// instanced actor's handler triggers the drain → unwire → exit
/// path, flips the `actor_registry` slot to `Dead`, and inserts the
/// id into `tombstones`. A reused subname after retirement returns
/// `SpawnError::SubnameRetired`.
#[test]
fn ctx_shutdown_marks_dead_runs_unwire_tombstones_id() {
    use crate::actor::native::spawn::{SpawnError, Subname};
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Quit { tag: u32 }, "test.shutdown.quit", 0xE0E1_E2E3_E4E5_E6E7);

    shutdown_on_kind_actor!(Closer, "test.shutdown.closer", Quit);

    let (registry, mailer) = bare_substrate();
    let close_observed = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id = chassis
        .spawn_actor::<Closer>(Subname::Counter, Arc::clone(&close_observed))
        .finish()
        .expect("spawn instanced actor");

    // Push a Quit envelope at the spawned mailbox via the
    // registered sink handler. The handler's `ctx.shutdown()`
    // flips the dispatcher's flag; after the handler returns the
    // trampoline drains, runs `unwire`, marks Dead, tombstones.
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("sink registered") else {
        panic!("expected mailbox entry for instanced actor");
    };
    let bytes = (Quit { tag: 1 }).encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(<Quit as Kind>::ID, Quit::NAME, &bytes, 1));

    // Wait for unwire to run + the registry slot to flip Dead.
    let deadline = Instant::now() + Duration::from_millis(500);
    while close_observed.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        close_observed.load(AtomicOrdering::SeqCst),
        1,
        "unwire fired exactly once after the dispatcher drained"
    );
    // Spin until the slot transitions Dead — the dispatcher
    // thread runs `mark_dead` after `unwire`, so there's a
    // small window between the close-observed bump above and the
    // registry update.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!chassis.actor_registry().is_live(id), "registry slot should transition Live → Dead after unwire runs");
    assert!(chassis.actor_registry().is_tombstoned(id), "tombstone insertion forbids reuse of the retired full name");

    // Spawning again under the same `Subname::Counter` would
    // increment the per-Spawner counter (so it'd target a fresh
    // id, not collide); reuse the same `Named` subname to land
    // back at the tombstoned id.
    let err = chassis
        .spawn_actor::<Closer>(Subname::Named("0"), Arc::clone(&close_observed))
        .finish()
        .expect_err("retired subname must reject");
    assert!(matches!(err, SpawnError::SubnameRetired { .. }), "expected SubnameRetired, got {err:?}");

    drop(chassis);
}

/// Issue 3051: spawned native actors seed their declared handler costs into
/// both indexes before dispatch, framework kinds remain unseeded, and mailbox
/// finalization removes the global rows after `unwire`.
#[test]
fn spawned_actor_costs_seed_fold_filter_and_drop_on_finalization() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_data::{Kind, ReplyContract};
    use aether_kinds::{ComponentCapabilities, CostTail, CostTailResult, HandlerCapability};
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(CostPing { tag: u32 }, "test.spawn_cost.ping", 0xE1E2_E3E4_E5E6_E7E8);
    pod_kind!(CostQuit { tag: u32 }, "test.spawn_cost.quit", 0xE8E7_E6E5_E4E3_E2E1);

    struct SpawnCostProbe {
        ping_count: Arc<AtomicU32>,
    }

    impl Addressable for SpawnCostProbe {
        const NAMESPACE: &'static str = "test.spawn_cost.probe";
        type Resolver = aether_actor::Many;
    }

    impl HandlesKind<CostPing> for SpawnCostProbe {}
    impl HandlesKind<CostQuit> for SpawnCostProbe {}

    impl aether_actor::Lifecycle<Self> for SpawnCostProbe {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;

        fn init(ping_count: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { ping_count })
        }
    }

    impl NativeActor for SpawnCostProbe {
        type State = Self;
    }

    impl Dispatch<Self> for SpawnCostProbe {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind == CostPing::ID {
                let _ = CostPing::decode_from_bytes(payload)?;
                state.ping_count.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            if kind == CostQuit::ID {
                let _ = CostQuit::decode_from_bytes(payload)?;
                ctx.shutdown();
                return Some(());
            }
            None
        }

        fn capabilities() -> ComponentCapabilities {
            ComponentCapabilities {
                handlers: [
                    HandlerCapability {
                        id: CostPing::ID,
                        name: CostPing::NAME.to_owned(),
                        doc: None,
                        reply: ReplyContract::None,
                    },
                    HandlerCapability {
                        id: CostQuit::ID,
                        name: CostQuit::NAME.to_owned(),
                        doc: None,
                        reply: ReplyContract::None,
                    },
                ]
                .into(),
                fallback: None,
                doc: None,
                config: None,
            }
        }
    }

    let (registry, mailer) = bare_substrate();
    let ping_count = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");
    let id = chassis
        .spawn_actor::<SpawnCostProbe>(Subname::Named("measured"), Arc::clone(&ping_count))
        .finish()
        .expect("spawn cost probe");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostPing::ID) }) else {
        panic!("spawned actor cost tail succeeds");
    };
    assert_eq!(rows.len(), 1, "declared handler has one construction-time neutral row");
    assert_eq!(rows[0].samples, 0, "declared handler starts at the neutral seed");

    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("spawned actor inbox registered") else {
        panic!("expected spawned actor inbox");
    };
    let framework = CostTail { kind: None }.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(CostTail::ID, CostTail::NAME, &framework, 1));
    let ping = CostPing { tag: 1 }.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(CostPing::ID, CostPing::NAME, &ping, 1));

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostPing::ID) }) else {
            panic!("spawned actor cost tail succeeds while awaiting dispatch");
        };
        if rows.iter().any(|row| row.samples > 0) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(ping_count.load(AtomicOrdering::SeqCst), 1, "declared handler dispatches exactly once");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostPing::ID) }) else {
        panic!("spawned actor cost tail succeeds after dispatch");
    };
    assert_eq!(rows.len(), 1, "filtered tail returns only the declared handler row");
    assert!(rows[0].samples > 0, "declared handler folds a nonzero sample count");
    assert!(rows[0].mean_nanos > 0, "declared handler folds a nonzero execution cost");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: Some(CostTail::ID) }) else {
        panic!("framework-kind cost tail succeeds");
    };
    assert!(rows.is_empty(), "framework-handled CostTail never creates a handler-cost row");

    let quit = CostQuit { tag: 1 }.encode_into_bytes();
    handler.enqueue(registry::test_owned_dispatch(CostQuit::ID, CostQuit::NAME, &quit, 1));
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!chassis.actor_registry().is_live(id), "quit finalizes the spawned mailbox");

    let CostTailResult::Ok { rows } = mailer.cost_table().tail(id, &CostTail { kind: None }) else {
        panic!("finalized actor cost tail succeeds");
    };
    assert!(rows.is_empty(), "finalization removes every global cost row for the spawned mailbox");

    drop(chassis);
}

/// Issue 685: chassis teardown drives `unwire` on every spawned
/// instanced actor, even those that never received a self-shutdown
/// trigger. Pre-685 the Pooled spawn path's slot was reachable
/// from the chassis only through the wake's `Weak`, and nothing
/// signaled shutdown at chassis exit — so spawned actors silently
/// skipped their close path. The Spawner's `shutdown_instanced`
/// step now signals + wakes every spawned slot before the pool
/// drops, and the chassis waits for each `Drainable::is_closed`.
#[test]
fn chassis_teardown_runs_unwire_for_pooled_spawned_actors() {
    use crate::actor::native::spawn::Subname;

    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    close_observed_actor!(Quiet, "test.teardown.quiet");

    let (registry, mailer) = bare_substrate();
    let close_observed = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id = chassis
        .spawn_actor::<Quiet>(Subname::Counter, Arc::clone(&close_observed))
        .finish()
        .expect("spawn instanced actor");

    // No mail at all — the actor sits idle from the moment it
    // spawns. Pre-685 chassis teardown skipped its close path
    // entirely; post-685 the teardown step signals + wakes it and
    // the worker runs the close cycle before the pool drops.
    assert_eq!(close_observed.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);

    assert_eq!(
        close_observed.load(AtomicOrdering::SeqCst),
        1,
        "chassis teardown must drive unwire exactly once for a quiet spawned actor",
    );
    // Drop the unused id binding so clippy stays quiet — its
    // referent (the actor_registry's Live entry) drops with the
    // chassis above.
    let _ = id;
}

/// Issue 714: stress version of the chassis-teardown contract.
/// Spawn N=64 instanced actors and assert all N `close_observed`
/// counters tick to exactly 1 after `drop(chassis)`. Pre-714 the
/// polling-based `shutdown_instanced` could lose individual wakes
/// under contention; the channel-signal rewrite is deterministic
/// — even one missed `unwire` here fails the test.
#[test]
fn chassis_teardown_runs_unwire_for_many_pooled_actors() {
    use crate::actor::native::spawn::Subname;

    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    close_observed_actor!(Quiet, "test.teardown.quiet_many");

    const N: usize = 64;

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let counters: Vec<Arc<AtomicU32>> = (0..N).map(|_| Arc::new(AtomicU32::new(0))).collect();
    for (i, counter) in counters.iter().enumerate() {
        let name = format!("inst-{i}");
        chassis
            .spawn_actor::<Quiet>(Subname::Named(&name), Arc::clone(counter))
            .finish()
            .expect("spawn instanced actor");
    }

    for counter in &counters {
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 0);
    }

    drop(chassis);

    for (i, counter) in counters.iter().enumerate() {
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 1, "actor {i} must have run unwire exactly once");
    }
}

/// Issue 607 Phase 5: type mismatch through `resolve_actor` returns
/// `None` rather than a downcast that succeeds against the wrong
/// type. Two instanced types live under different namespaces; a
/// lookup with one type at the other's id mismatches and returns
/// None.
#[test]
fn resolve_actor_returns_none_on_type_mismatch() {
    use crate::actor::native::spawn::Subname;

    struct Foo;
    impl Addressable for Foo {
        const NAMESPACE: &'static str = "test.resolve_mismatch.foo";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Lifecycle<Self> for Foo {
        type Config = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for Foo {
        type State = Self;
    }
    impl Dispatch<Self> for Foo {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct Bar;
    impl Addressable for Bar {
        const NAMESPACE: &'static str = "test.resolve_mismatch.bar";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Lifecycle<Self> for Bar {
        type Config = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for Bar {
        type State = Self;
    }
    impl Dispatch<Self> for Bar {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let _ = chassis.spawn_actor::<Foo>(Subname::Named("only"), ()).finish().expect("spawn foo");

    // Resolving with the same subname but the wrong type returns
    // None — the namespaces differ so the hashed full names differ
    // and Bar's "only" is just not present. (The TypeId guard
    // would catch a hash collision.)
    assert!(chassis.resolve_actor::<Bar>("only").is_none());

    // resolve_actors::<Bar>() is empty because no Bar instances
    // were spawned, even though a Foo with the same subname exists.
    assert_eq!(chassis.resolve_actors::<Bar>().len(), 0);
    assert_eq!(chassis.resolve_actors::<Foo>().len(), 1);

    drop(chassis);
}

/// Issue 607 Phase 4b verify: a `ctx.monitor(target)` registration
/// fires exactly one `MonitorNotice` at the watcher when the
/// target self-shuts. Two-actor scenario: Watcher (instanced)
/// holds a `MonitorHandle` against Target (instanced) and counts
/// the notices it receives; Target self-shuts on `Quit`. After
/// the close fan-out we assert (1) the watcher saw the notice
/// once with the right target id, (2) the target's slot is Dead +
/// tombstoned, and (3) the registry's forward index drained.
#[test]
fn ctx_monitor_fires_notice_at_target_close() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering};

    // Self-shutdown trigger for the target.
    pod_kind!(Quit { tag: u32 }, "test.monitor.quit", 0xC0DE_C0DE_4B4B_4B4B);

    // Tells the watcher which target to monitor. The watcher's
    // handler reads `target_id` and calls `ctx.monitor`.
    pod_kind!(WatchOrder { target_id: u64 }, "test.monitor.watch_order", 0x4B4B_C0DE_C0DE_C0DE);

    // Target — handles Quit by self-shutting.
    unit_shutdown_actor!(Target, "test.monitor.target", Quit);

    // Watcher — handles WatchOrder by registering a monitor;
    // handles MonitorNotice by recording the target id and
    // bumping a counter.
    struct Watcher {
        notice_count: Arc<AtomicU32>,
        last_target: Arc<AtomicU64>,
        handle: Mutex<Option<MonitorHandle>>,
    }
    impl Addressable for Watcher {
        const NAMESPACE: &'static str = "test.monitor.watcher";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<WatchOrder> for Watcher {}
    impl HandlesKind<aether_kinds::MonitorNotice> for Watcher {}
    impl aether_actor::Lifecycle<Self> for Watcher {
        type Config = (Arc<AtomicU32>, Arc<AtomicU64>);
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { notice_count: config.0, last_target: config.1, handle: Mutex::new(None) })
        }
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let target = MailboxId(order.target_id);
                let h = ctx.monitor(target).expect("target must be Live at order time");
                *state.handle.lock().unwrap() = Some(h);
                return Some(());
            }
            if kind.0 == <aether_kinds::MonitorNotice as Kind>::ID.0 {
                let notice = <aether_kinds::MonitorNotice as Kind>::decode_from_bytes(payload)?;
                state.last_target.store(notice.target.0, AtomicOrdering::SeqCst);
                state.notice_count.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    // Spawn target first so the watcher can register against a
    // Live id.
    let target_id = chassis.spawn_actor::<Target>(Subname::Counter, ()).finish().expect("spawn target");

    let notice_count = Arc::new(AtomicU32::new(0));
    let last_target = Arc::new(AtomicU64::new(0));
    let watcher_id = chassis
        .spawn_actor::<Watcher>(Subname::Counter, (Arc::clone(&notice_count), Arc::clone(&last_target)))
        .finish()
        .expect("spawn watcher");

    // Drive the watcher to register the monitor by pushing a
    // WatchOrder through its sink handler. After this returns
    // the watcher's handle is stored in `self.handle`.
    let MailboxEntry::Inbox { handler: watcher_handler, .. } =
        registry.entry(watcher_id).expect("watcher sink registered")
    else {
        panic!("expected mailbox entry for watcher");
    };
    let order = WatchOrder { target_id: target_id.0 };
    watcher_handler.enqueue(registry::test_owned_dispatch(
        <WatchOrder as Kind>::ID,
        WatchOrder::NAME,
        &order.encode_into_bytes(),
        1,
    ));

    // Wait until the registry sees the monitor entry.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().monitor_count(target_id) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        chassis.actor_registry().monitor_count(target_id),
        1,
        "watcher's monitor should be registered against target",
    );
    assert_eq!(chassis.actor_registry().monitoring_count(watcher_id), 1, "watcher should appear in the reverse index");

    // Fire Quit at the target — its handler self-shuts; the
    // dispatcher's close path runs `close_actor`, which fans out
    // a MonitorNotice mail to watcher_id.
    let MailboxEntry::Inbox { handler: target_handler, .. } =
        registry.entry(target_id).expect("target sink registered")
    else {
        panic!("expected mailbox entry for target");
    };
    target_handler.enqueue(registry::test_owned_dispatch(
        <Quit as Kind>::ID,
        Quit::NAME,
        &(Quit { tag: 1 }).encode_into_bytes(),
        1,
    ));

    // Wait for the notice to land at the watcher.
    let deadline = Instant::now() + Duration::from_millis(500);
    while notice_count.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(notice_count.load(AtomicOrdering::SeqCst), 1, "watcher should have received exactly one MonitorNotice");
    assert_eq!(
        last_target.load(AtomicOrdering::SeqCst),
        target_id.0,
        "MonitorNotice.target should match the closed actor's id",
    );

    // Wait for target slot to flip Dead (the close path runs
    // close_actor → mark_dead after fan-out).
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(target_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !chassis.actor_registry().is_live(target_id),
        "target slot should transition Live → Dead after close fan-out",
    );
    assert!(chassis.actor_registry().is_tombstoned(target_id), "target id should be tombstoned");
    // Forward index for target was drained.
    assert_eq!(chassis.actor_registry().monitor_count(target_id), 0, "monitors_of[target] must drain after fan-out");

    drop(chassis);
}

/// Issue 607 Phase 4b verify: when the *watcher* dies first, the
/// reverse-index walk prunes the watcher's entry from each
/// monitored target's `monitors_of`. No `MonitorNotice` fires (the
/// watcher is the one closing; targets are still alive).
#[test]
fn watcher_close_prunes_targets_forward_index() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // Re-use Quit + WatchOrder shape inline (test isolation).
    pod_kind!(Quit { tag: u32 }, "test.monitor.quit2", 0xCAFE_BABE_DEAD_BEEF);
    pod_kind!(WatchOrder { target_id: u64 }, "test.monitor.watch_order2", 0xBEEF_DEAD_BABE_CAFE);

    struct Target;
    impl Addressable for Target {
        const NAMESPACE: &'static str = "test.monitor.target2";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Lifecycle<Self> for Target {
        type Config = ();
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init((): Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }
    }
    impl NativeActor for Target {
        type State = Self;
    }
    impl Dispatch<Self> for Target {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct Watcher {
        handle: Mutex<Option<MonitorHandle>>,
        close_observed: Arc<AtomicU32>,
    }
    impl Addressable for Watcher {
        const NAMESPACE: &'static str = "test.monitor.watcher2";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<WatchOrder> for Watcher {}
    impl HandlesKind<Quit> for Watcher {}
    impl aether_actor::Lifecycle<Self> for Watcher {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { handle: Mutex::new(None), close_observed: config })
        }
        fn unwire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.close_observed.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for Watcher {
        type State = Self;
    }
    impl Dispatch<Self> for Watcher {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WatchOrder::ID.0 {
                let order = WatchOrder::decode_from_bytes(payload)?;
                let target = MailboxId(order.target_id);
                let h = ctx.monitor(target).expect("target Live");
                *state.handle.lock().unwrap() = Some(h);
                return Some(());
            }
            if kind.0 == Quit::ID.0 {
                let _ = Quit::decode_from_bytes(payload)?;
                ctx.shutdown();
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let target_id = chassis.spawn_actor::<Target>(Subname::Counter, ()).finish().expect("spawn target");
    let close_observed = Arc::new(AtomicU32::new(0));
    let watcher_id =
        chassis.spawn_actor::<Watcher>(Subname::Counter, Arc::clone(&close_observed)).finish().expect("spawn watcher");

    // Watcher registers monitor against target.
    let MailboxEntry::Inbox { handler: watcher_handler, .. } =
        registry.entry(watcher_id).expect("watcher sink registered")
    else {
        panic!("expected mailbox entry for watcher");
    };
    let order = WatchOrder { target_id: target_id.0 };
    watcher_handler.enqueue(registry::test_owned_dispatch(
        <WatchOrder as Kind>::ID,
        WatchOrder::NAME,
        &order.encode_into_bytes(),
        1,
    ));

    // Wait for register to land.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().monitor_count(target_id) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(chassis.actor_registry().monitor_count(target_id), 1);

    // Quit watcher — its close path walks `monitoring[watcher]` and
    // prunes watcher from `monitors_of[target]`.
    watcher_handler.enqueue(registry::test_owned_dispatch(
        <Quit as Kind>::ID,
        Quit::NAME,
        &(Quit { tag: 1 }).encode_into_bytes(),
        1,
    ));

    let deadline = Instant::now() + Duration::from_millis(500);
    while close_observed.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(close_observed.load(AtomicOrdering::SeqCst), 1, "watcher's unwire fired exactly once");

    // Watcher slot tombstones; target slot still Live; target's
    // forward index drained of the dead watcher.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(watcher_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(chassis.actor_registry().is_tombstoned(watcher_id), "watcher tombstoned");
    assert!(chassis.actor_registry().is_live(target_id), "target should still be Live (watcher closed, not target)");
    assert_eq!(
        chassis.actor_registry().monitor_count(target_id),
        0,
        "target's monitors_of should drop the dead watcher",
    );

    drop(chassis);
}

/// Issue 607 Phase 5 verify: `resolve_actor` and `resolve_actors`
/// against a multi-instance fixture. Spawns three instanced actors
/// under one type, asserts:
///   - `resolve_actor::<A>("a")` finds the named instance.
///   - `resolve_actor::<A>("missing")` returns `None`.
///   - `resolve_actors::<A>()` enumerates all three (subname-keyed).
///   - After one closes, the iterator drops to two and the closed
///     name returns `None` from `resolve_actor`.
#[test]
fn resolve_actor_finds_named_instance_resolve_actors_enumerates() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(Quit { tag: u32 }, "test.resolve.quit", 0xF00D_F00D_F00D_F00D);

    // The `tag` field is set at init from the per-instance config
    // and would be read by handler code; Phase A's resolve_actor
    // returns MailboxId rather than `Arc<Member>` so the tag is no
    // longer externally observable. Kept as an init payload so the
    // spawn path covers the full Config-threaded shape.
    #[allow(dead_code)]
    struct Member {
        tag: u32,
    }
    impl Addressable for Member {
        const NAMESPACE: &'static str = "test.resolve.member";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<Quit> for Member {}
    impl aether_actor::Lifecycle<Self> for Member {
        type Config = u32;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(tag: u32, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { tag })
        }
    }
    impl NativeActor for Member {
        type State = Self;
    }
    impl Dispatch<Self> for Member {
        fn dispatch(
            _state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Quit::ID.0 {
                let _ = Quit::decode_from_bytes(payload)?;
                ctx.shutdown();
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id_a = chassis.spawn_actor::<Member>(Subname::Named("a"), 1).finish().expect("spawn a");
    let _id_b = chassis.spawn_actor::<Member>(Subname::Named("b"), 2).finish().expect("spawn b");
    let id_c = chassis.spawn_actor::<Member>(Subname::Named("c"), 3).finish().expect("spawn c");

    // Issue 629 / Phase A: resolve_actor returns the address
    // (`MailboxId`), not `Arc<A>`. Verify the address resolves and
    // matches the spawn-time id.
    let a_id = chassis.resolve_actor::<Member>("a").expect("a is live");
    assert_eq!(a_id, id_a, "resolve_actor returns the matching MailboxId");

    // Missing subname → None.
    assert!(chassis.resolve_actor::<Member>("missing").is_none(), "unknown subname should return None");

    // resolve_actors enumerates all three. Order is registry-defined
    // (HashMap iteration), so collect into a sorted subname vec for
    // assertions. The Member's per-instance tag is dispatcher-thread
    // owned (Phase A) and not externally observable here; the
    // subname uniquely identifies the instance.
    let mut all: Vec<String> = chassis.resolve_actors::<Member>().into_iter().map(|(name, _id)| name).collect();
    all.sort();
    assert_eq!(
        all,
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        "resolve_actors should enumerate every Live instance subname",
    );

    // Close c — Quit it through the sink handler. After close,
    // resolve_actors drops to two and resolve_actor::<Member>("c")
    // returns None.
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id_c).expect("c sink registered") else {
        panic!("expected mailbox entry for c");
    };
    handler.enqueue(registry::test_owned_dispatch(
        <Quit as Kind>::ID,
        Quit::NAME,
        &(Quit { tag: 1 }).encode_into_bytes(),
        1,
    ));

    // Wait for c's slot to flip Dead.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(id_c) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    assert!(chassis.resolve_actor::<Member>("c").is_none(), "closed instance should disappear from resolve_actor");
    let mut after: Vec<String> = chassis.resolve_actors::<Member>().into_iter().map(|(name, _id)| name).collect();
    after.sort();
    assert_eq!(after, vec!["a".to_owned(), "b".to_owned()], "resolve_actors should drop the closed instance");

    // Counter for unused warning. (`_id_a` / `_id_b` retain their
    // names elsewhere; this guard keeps the compiler happy.)
    let _ = AtomicU32::new(0).load(AtomicOrdering::SeqCst);

    drop(chassis);
}

/// Issue 607 Phase 5.5 verify: an instanced parent's handler calls
/// `ctx.spawn_child::<Grandchild>(...)` to launch an instanced
/// grandchild. Phase 3b shipped `Arc<Spawner>` threading through
/// every spawned actor's transport precisely so this works; this
/// test is the first end-to-end coverage of the instanced→instanced
/// path. Phase 6b (`TcpListenerActor` → `TcpSessionActor`) structurally
/// depends on this — listeners spawning sessions IS the recursive
/// case.
///
/// Asserts:
///   1. Grandchild's `MailboxId` is `Live` in the registry.
///   2. `chassis.resolve_actor::<Grandchild>(name)` resolves it.
///   3. Grandchild's `after_init` mail dispatches as its first
///      envelope (received counter bumps to 1).
///   4. Closing the parent does NOT cascade-close the grandchild —
///      no parent-child shutdown coupling is wired by default;
///      that's monitor-driven, opt-in.
#[test]
fn instanced_can_spawn_grandchild() {
    use crate::actor::native::spawn::Subname;
    use crate::mail::registry::MailboxEntry;
    use aether_actor::HandlesKind;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    // Trigger to make the parent spawn its grandchild.
    pod_kind!(Hatch { tag: u32 }, "test.recursive.hatch", 0xA00A_A00A_A00A_A00A);

    // Pre-loaded onto the grandchild via after_init.
    pod_kind!(Ping { tag: u32 }, "test.recursive.ping", 0xB00B_B00B_B00B_B00B);

    // Self-shutdown trigger for the parent.
    pod_kind!(Quit { tag: u32 }, "test.recursive.quit", 0xC00C_C00C_C00C_C00C);

    struct Grandchild {
        received: Arc<AtomicU32>,
    }
    impl Addressable for Grandchild {
        const NAMESPACE: &'static str = "test.recursive.grandchild";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<Ping> for Grandchild {}
    impl aether_actor::Lifecycle<Self> for Grandchild {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received: config })
        }
    }
    impl NativeActor for Grandchild {
        type State = Self;
    }
    impl Dispatch<Self> for Grandchild {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Ping::ID.0 {
                let _ = Ping::decode_from_bytes(payload)?;
                state.received.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    struct Parent {
        grandchild_received: Arc<AtomicU32>,
    }
    impl Addressable for Parent {
        const NAMESPACE: &'static str = "test.recursive.parent";
        type Resolver = aether_actor::Many;
    }
    impl HandlesKind<Hatch> for Parent {}
    impl HandlesKind<Quit> for Parent {}
    impl aether_actor::Lifecycle<Self> for Parent {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { grandchild_received: config })
        }
    }
    impl NativeActor for Parent {
        type State = Self;
    }
    impl Dispatch<Self> for Parent {
        fn dispatch(
            state: &mut Self,
            ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == Hatch::ID.0 {
                let _ = Hatch::decode_from_bytes(payload)?;
                // Recursive spawn: instanced parent → instanced
                // grandchild. Pre-load a Ping so the grandchild's
                // first envelope dispatches without an external
                // mail step.
                let _id = ctx
                    .spawn_child::<Grandchild>(Subname::Named("only"), Arc::clone(&state.grandchild_received))
                    .after_init(Ping { tag: 0xCAFE })
                    .finish()
                    .expect("recursive spawn must succeed");
                return Some(());
            }
            if kind.0 == Quit::ID.0 {
                let _ = Quit::decode_from_bytes(payload)?;
                ctx.shutdown();
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let grandchild_received = Arc::new(AtomicU32::new(0));
    let parent_id = chassis
        .spawn_actor::<Parent>(Subname::Named("p1"), Arc::clone(&grandchild_received))
        .finish()
        .expect("spawn parent");

    // Trigger parent → grandchild spawn.
    let MailboxEntry::Inbox { handler: parent_handler, .. } =
        registry.entry(parent_id).expect("parent sink registered")
    else {
        panic!("expected mailbox entry for parent");
    };
    parent_handler.enqueue(registry::test_owned_dispatch(
        <Hatch as Kind>::ID,
        Hatch::NAME,
        &(Hatch { tag: 1 }).encode_into_bytes(),
        1,
    ));

    // Wait for the grandchild's after_init Ping to dispatch (proves
    // the recursive spawn happened AND the after_init plumbing
    // works through it).
    let deadline = Instant::now() + Duration::from_millis(500);
    while grandchild_received.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        grandchild_received.load(AtomicOrdering::SeqCst),
        1,
        "grandchild's after_init Ping should dispatch as its first envelope",
    );

    // Grandchild is Live under the ADR-0099 §3 lineage fold. The
    // parent was chassis-spawned (no parent → depth-1, carry == id),
    // so the grandchild's id folds its node's ActorId onto the
    // parent's id — not the flat `hash(NAMESPACE:subname)`.
    let grandchild_id = MailboxId(aether_data::with_tag(
        aether_data::Tag::Mailbox,
        aether_data::fold_lineage(parent_id.0, aether_data::ActorId::instanced("test.recursive.grandchild", "only")),
    ));
    assert!(
        chassis.actor_registry().is_live(grandchild_id),
        "grandchild should be Live in the registry under the lineage-folded id",
    );

    // Issue 629 / Phase A: resolve_actor returns the address.
    // Verify it resolves and matches the registry id.
    let resolved = chassis.resolve_actor::<Grandchild>("only").expect("resolve_actor must find the grandchild");
    assert_eq!(resolved, grandchild_id, "resolve_actor returns the matching MailboxId");
    // The grandchild is alive (verifies the dispatcher's Arc<AtomicU32>
    // is the same one passed in via config — the test's `received`
    // counter sees handler dispatches against the live instance).
    let _ = &grandchild_received;

    // Closing the parent does NOT cascade-close the grandchild.
    // Parent-child shutdown coupling is opt-in via monitor; without
    // it, the grandchild keeps running.
    parent_handler.enqueue(registry::test_owned_dispatch(
        <Quit as Kind>::ID,
        Quit::NAME,
        &(Quit { tag: 1 }).encode_into_bytes(),
        1,
    ));

    // Wait for parent slot to flip Dead.
    let deadline = Instant::now() + Duration::from_millis(500);
    while chassis.actor_registry().is_live(parent_id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(chassis.actor_registry().is_tombstoned(parent_id), "parent should have tombstoned");
    // Grandchild survives — no cascade.
    assert!(
        chassis.actor_registry().is_live(grandchild_id),
        "grandchild should outlive parent (no automatic cascade-close)",
    );
    assert!(
        chassis.resolve_actor::<Grandchild>("only").is_some(),
        "grandchild remains resolvable after parent's death",
    );

    drop(chassis);
}

/// Issue 697 multi-pass model: wire-time mail crosses actors
/// regardless of declaration order. Pinger's `wire` mails Ponger;
/// Ponger's handler increments a counter. With Pinger declared
/// FIRST, a single-pass interleaved boot would have Pinger's wire
/// fire before Ponger's claim — the mail would warn-drop. The
/// multi-pass model (claim-all → init-all → wire-all → spawn-all)
/// claims both mailboxes before any wire runs, so the mail queues
/// in Ponger's inbox and processes once dispatchers come up.
#[test]
fn wire_pass_mail_crosses_actors_pinger_first() {
    wire_pass_mail_crosses_actors(/* pinger_first */ true);
}

/// Mirror of [`wire_pass_mail_crosses_actors_pinger_first`] with
/// the registration order reversed. Multi-pass model means both
/// orderings are valid; this test pins the symmetry.
#[test]
fn wire_pass_mail_crosses_actors_ponger_first() {
    wire_pass_mail_crosses_actors(/* pinger_first */ false);
}

/// Issue 584 Phase 2a runtime sibling: `Spawner::spawn_actor` runs
/// `wire` exactly once on a freshly-spawned instanced actor —
/// after `init` Ok and after the mailbox is published, before
/// pre-load mail or the dispatcher pull. Runtime spawn doesn't
/// need the chassis-boot multi-pass barrier (the substrate is
/// already steady-state).
#[test]
fn spawn_actor_runs_wire_once_after_init() {
    use crate::actor::native::spawn::Subname;

    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    struct WireSpawnProbe {
        wire_count: Arc<AtomicU32>,
    }
    impl Addressable for WireSpawnProbe {
        const NAMESPACE: &'static str = "test.spawn_wire.probe";
        type Resolver = aether_actor::Many;
    }
    impl aether_actor::Lifecycle<Self> for WireSpawnProbe {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_count: config })
        }
        fn wire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for WireSpawnProbe {
        type State = Self;
    }
    impl Dispatch<Self> for WireSpawnProbe {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let wire_count = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .build_passive()
        .expect("empty chassis boots");

    let id = chassis
        .spawn_actor::<WireSpawnProbe>(Subname::Counter, Arc::clone(&wire_count))
        .finish()
        .expect("spawn instanced actor");

    assert_eq!(wire_count.load(AtomicOrdering::SeqCst), 1, "wire must fire exactly once on Spawner::spawn_actor");

    drop(chassis);
    let _ = id;
}

/// Issue 584 Phase 2a / 697 wire pass: `wire` runs exactly once
/// for a singleton actor at chassis boot, after `init` succeeds
/// and before the dispatcher pulls the first envelope.
#[test]
fn with_actor_runs_wire_once_at_chassis_boot() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    struct WireProbe {
        wire_count: Arc<AtomicU32>,
    }
    impl Addressable for WireProbe {
        const NAMESPACE: &'static str = "test.wire.singleton";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Lifecycle<Self> for WireProbe {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_count: config })
        }
        fn wire(state: &mut Self, _ctx: &mut NativeCtx<'_>) {
            state.wire_count.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for WireProbe {
        type State = Self;
    }
    impl Dispatch<Self> for WireProbe {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let wire_count = Arc::new(AtomicU32::new(0));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<WireProbe>(Arc::clone(&wire_count))
        .build_passive()
        .expect("with_actor boot succeeds");

    assert_eq!(
        wire_count.load(AtomicOrdering::SeqCst),
        1,
        "wire must fire exactly once during builder.with_actor boot",
    );

    drop(chassis);
}

fn wire_pass_mail_crosses_actors(pinger_first: bool) {
    use aether_actor::MailSender;
    use aether_data::Kind;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    pod_kind!(WireBarrierPing { tag: u32 }, "test.barrier.wire_ping", 0xB0B1_B2B3_B4B5_B6B7);

    struct Pinger {
        wire_ran: Arc<AtomicU32>,
    }
    impl Addressable for Pinger {
        const NAMESPACE: &'static str = "test.barrier.pinger";
        type Resolver = aether_actor::One;
    }
    impl aether_actor::Lifecycle<Self> for Pinger {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { wire_ran: config })
        }
        fn wire(state: &mut Self, ctx: &mut NativeCtx<'_>) {
            ctx.send_to_named::<WireBarrierPing>(Ponger::NAMESPACE, &WireBarrierPing { tag: 1 });
            state.wire_ran.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }
    impl NativeActor for Pinger {
        type State = Self;
    }
    impl Dispatch<Self> for Pinger {
        fn dispatch(
            _state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            _kind: KindId,
            _payload: &[u8],
        ) -> Option<()> {
            None
        }
    }

    struct Ponger {
        received: Arc<AtomicU32>,
    }
    impl Addressable for Ponger {
        const NAMESPACE: &'static str = "test.barrier.ponger";
        type Resolver = aether_actor::One;
    }
    impl HandlesKind<WireBarrierPing> for Ponger {}
    impl aether_actor::Lifecycle<Self> for Ponger {
        type Config = Arc<AtomicU32>;
        type InitError = BootError;
        type InitCtx<'a> = NativeInitCtx<'a>;
        type Ctx<'a> = NativeCtx<'a>;
        fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { received: config })
        }
    }
    impl NativeActor for Ponger {
        type State = Self;
    }
    impl Dispatch<Self> for Ponger {
        fn dispatch(
            state: &mut Self,
            _ctx: &mut NativeCtx<'_, crate::Manual>,
            kind: KindId,
            payload: &[u8],
        ) -> Option<()> {
            if kind.0 == WireBarrierPing::ID.0 {
                let _ = WireBarrierPing::decode_from_bytes(payload)?;
                state.received.fetch_add(1, AtomicOrdering::SeqCst);
                return Some(());
            }
            None
        }
    }

    let (registry, mailer) = bare_substrate();
    let received = Arc::new(AtomicU32::new(0));
    let wire_ran = Arc::new(AtomicU32::new(0));

    let builder = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer));
    let builder = if pinger_first {
        builder.with_actor::<Pinger>(Arc::clone(&wire_ran)).with_actor::<Ponger>(Arc::clone(&received))
    } else {
        builder.with_actor::<Ponger>(Arc::clone(&received)).with_actor::<Pinger>(Arc::clone(&wire_ran))
    };
    let chassis = builder.build_passive().expect("multi-pass boot succeeds");

    assert_eq!(wire_ran.load(AtomicOrdering::SeqCst), 1, "pinger's wire must have run during the wire pass");

    // Wait for Ponger's dispatcher to drain the wire-emitted ping.
    let deadline = Instant::now() + Duration::from_millis(500);
    while received.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        received.load(AtomicOrdering::SeqCst),
        1,
        "ponger must observe pinger's wire-emitted ping (multi-pass barrier)",
    );

    drop(chassis);
}

/// Issue 745: `Some(0)` clamps to 1 since the pool requires at
/// least one worker.
#[test]
fn with_workers_some_zero_clamps_to_one() {
    let (registry, mailer) = bare_substrate();
    let builder = Builder::<TestChassis>::new(registry, mailer).with_workers(Some(0));
    assert_eq!(builder.workers, Some(1));
}

/// Issue 745: the override survives the type-state transition into
/// [`HasDriver`] so chassis mains can call `.with_workers(...)`
/// either before or after `.driver(_)`.
#[test]
fn with_workers_survives_driver_transition() {
    let (registry, mailer) = bare_substrate();
    let ran = Arc::new(AtomicBool::new(false));
    let builder = Builder::<DrivenTestChassis<RanDriver>>::new(registry, mailer)
        .with_workers(Some(3))
        .driver(RanDriver { ran: Arc::clone(&ran) });
    assert_eq!(builder.workers, Some(3));
}
