//! Issue 552 stage 1: integration test for the
//! `#[actor] impl NativeActor for X` macro arm.
//!
//! Lives as a standalone integration test (not in `#[cfg(test)] mod`)
//! because the macro emits absolute paths (`::aether_substrate::*`)
//! that the lib's own test mod can't resolve via implicit `extern
//! crate self`. An integration test depends on `aether_substrate`
//! externally, so the path resolves naturally.
//!
//! The chassis-side hand-rolled fixture in
//! `chassis_builder::tests::with_actor_boots_dispatches_and_tears_down`
//! covers the same end-to-end guarantee without the macro layer; this
//! test is the additional gate that the macro's codegen produces a
//! working `Actor + HandlesKind + NativeActor + NativeDispatch`
//! stack on a real cap shape.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use aether_data::{Kind, ReplyTo};
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::registry::OwnedDispatch;
use aether_substrate::mail::{MailId, MailRef};
use aether_substrate::{
    Actor, BootError, Builder, BuiltChassis, Chassis, Mailer, NativeActor, NativeCtx,
    NativeInitCtx, NeverDriver, PassiveChassis, Registry, mail::MailboxId,
};
use std::thread;

/// Postcard-shape kind via the derive — exercises the
/// `decode_from_bytes` postcard path the macro's dispatch arm uses.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.greet")]
struct Greet {
    tag: u32,
}

/// Cast-shape kind so both arms (postcard + cast) get exercised
/// through one cap.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.ping")]
struct Ping {
    seq: u32,
}

struct MacroProbeCap {
    greet_total: Arc<AtomicU32>,
    ping_total: Arc<AtomicU32>,
}

impl aether_actor::Singleton for MacroProbeCap {}

/// Per-cap config — caps without a domain-specific config type
/// would write `()`, but here we thread shared atomic counters in
/// so the test can observe each handler's effect.
#[derive(Clone)]
struct ProbeConfig {
    greet_total: Arc<AtomicU32>,
    ping_total: Arc<AtomicU32>,
}

#[aether_data::actor]
impl NativeActor for MacroProbeCap {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "test.macro_native_actor.probe";

    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            greet_total: config.greet_total,
            ping_total: config.ping_total,
        })
    }

    /// Handles postcard-shape `Greet` mail.
    #[aether_data::handler]
    fn on_greet(&self, _ctx: &mut NativeCtx<'_>, mail: Greet) {
        self.greet_total.fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }

    /// Handles cast-shape `Ping` mail.
    #[aether_data::handler]
    fn on_ping(&self, _ctx: &mut NativeCtx<'_>, mail: Ping) {
        self.ping_total.fetch_add(mail.seq, AtomicOrdering::SeqCst);
    }
}

/// Test chassis fixture so `Builder::<C>::new` has a typed `C` to
/// parameterise on. `NeverDriver` makes the no-driver passive build
/// path the only valid one.
struct TestChassis;
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is built directly via Builder in tests");
    }
}

fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        (registry, mailer)
    }
}

fn push_envelope<K: Kind>(registry: &Registry, recipient: &str, payload: &K) {
    use aether_substrate::mail::registry::MailboxEntry;
    let id: MailboxId = registry.lookup(recipient).expect("mailbox registered");
    let MailboxEntry::Inbox(handler) = registry.entry(id).expect("entry exists") else {
        panic!("expected mailbox entry under {recipient}");
    };
    let bytes = payload.encode_into_bytes();
    handler.enqueue(OwnedDispatch {
        kind: <K as Kind>::ID,
        kind_name: K::NAME.to_owned(),
        origin: None,
        sender: ReplyTo::NONE,
        payload: MailRef::from(bytes),
        count: 1,
        mail_id: MailId::NONE,
        root: MailId::NONE,
        parent_mail: None,
    });
}

fn wait_for(target: u32, counter: &AtomicU32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while counter.load(AtomicOrdering::SeqCst) < target && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    counter.load(AtomicOrdering::SeqCst) >= target
}

#[test]
fn macro_emitted_cap_routes_postcard_kind_through_dispatch() {
    let (registry, mailer) = fresh_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<MacroProbeCap>(ProbeConfig {
                greet_total: Arc::clone(&greet_total),
                ping_total: Arc::clone(&ping_total),
            })
            .build_passive()
            .expect("macro-emitted cap boots");

    push_envelope(&registry, MacroProbeCap::NAMESPACE, &Greet { tag: 7 });
    assert!(
        wait_for(7, &greet_total, Duration::from_millis(500)),
        "macro dispatcher should route Greet → on_greet within budget"
    );
    assert_eq!(ping_total.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);
}

#[test]
fn macro_emitted_cap_routes_cast_kind_through_dispatch() {
    let (registry, mailer) = fresh_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<MacroProbeCap>(ProbeConfig {
                greet_total: Arc::clone(&greet_total),
                ping_total: Arc::clone(&ping_total),
            })
            .build_passive()
            .expect("macro-emitted cap boots");

    push_envelope(&registry, MacroProbeCap::NAMESPACE, &Ping { seq: 42 });
    assert!(
        wait_for(42, &ping_total, Duration::from_millis(500)),
        "macro dispatcher should route Ping → on_ping within budget"
    );
    assert_eq!(greet_total.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);
}

/// Type-level assertion that the macro emits the universal
/// `HandlesKind<K>` impls for each `#[handler]`. Runs at compile
/// time — the body never executes.
#[test]
fn macro_emits_handles_kind_per_handler() {
    fn assert_handles<R: aether_actor::HandlesKind<K>, K: Kind>() {}
    assert_handles::<MacroProbeCap, Greet>();
    assert_handles::<MacroProbeCap, Ping>();
}

/// Cast-shape kind the cap doesn't handle. Used to verify the
/// macro's `__aether_dispatch_envelope` returns `None` for unknown
/// kind ids — chassis-side dispatcher logs `dispatch missed` rather
/// than crashing. Issue 688 follow-up: parity with the
/// `dispatch_returns_none_for_unhandled_kind` test that retired
/// alongside the legacy facade.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.unknown")]
struct Unknown {
    payload: u32,
}

#[test]
fn macro_emitted_cap_drops_unknown_kind_via_dispatch() {
    let (registry, mailer) = fresh_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<MacroProbeCap>(ProbeConfig {
                greet_total: Arc::clone(&greet_total),
                ping_total: Arc::clone(&ping_total),
            })
            .build_passive()
            .expect("macro-emitted cap boots");

    push_envelope(
        &registry,
        MacroProbeCap::NAMESPACE,
        &Unknown { payload: 99 },
    );

    // Settle: give the dispatcher time to observe + drop the envelope.
    // The macro-emitted dispatch returns None; the chassis-side
    // dispatcher logs a warn but doesn't increment any handler counter.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(greet_total.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(ping_total.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);
}
