//! Canonical cross-crate test fixture for booting a chassis in tests.
//!
//! Every crate that exercises a native cap's `init` / handlers boots a
//! real [`Builder`] against a no-op [`TestChassis`] declaration. This
//! module is the single home for that fixture plus the seed / decode /
//! drive helpers, so downstream crates reach it through the
//! `test-support` dev-dependency feature instead of re-declaring their
//! own copy. Substrate's own `#[cfg(test)]` modules reach it through the
//! `test` arm of the module's cfg gate.
//!
//! Two seeds live here because substrate-internal tests and cap tests
//! want different starting states: [`bare_substrate`] registers no kind
//! descriptors and wires no outbound (substrate's own tests exercise
//! neither descriptor lookup nor the ADR-0037 bubble-up path), while
//! [`fresh_substrate`] / [`fresh_substrate_and_rx`] pre-populate the kind
//! descriptors and wire a loopback outbound for the cap tests that do.

#![allow(
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    reason = "test-support fixtures: the returned seeds are always consumed by the caller, and the `expect`s on setup are themselves the test assertions"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "the canonical test fixture is a deliberate embedder — it builds a bare `TestChassis` via `Builder::new` rather than the `composed` boot seam production chassis route through, and `registered_ref` applies the ADR-0099 lineage fold the registry's own name lookup applies" // aether-suppression-request: the existing module allow's reason now also names the lineage fold registered_ref applies; no new allow
)]

use std::env::temp_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_actor::{ErasedActorRef, Manual, Root};
use aether_data::{Kind, KindId, MailId, MailboxId, SessionToken, Source, SourceAddr, Uuid};
use aether_kinds::descriptors;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::{NativeActor, TaskCompletionWake};
use crate::chassis::Chassis;
use crate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use crate::chassis::error::BootError;
use crate::config::ConfigMember;
use crate::mail::MailRef;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::{BootAuthority, InboxHandler, NameConflict, Registry, lineage_mailbox_id};
use crate::mail::registry::{DispatchParts, OwnedDispatch};

/// Canonical test chassis. `build()` is unreachable — every consumer
/// drives the chassis through `Builder::<TestChassis>::new(...)` directly
/// rather than going through `TestChassis::build(())`.
pub struct TestChassis;

impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is driven by Builder::new directly in unit tests")
    }
}

/// Mint a [`BootAuthority`] for a fixture that drives the registry's direct
/// write path without a real chassis boot behind it.
///
/// The token's production constructor is crate-private so only
/// `aether-substrate`'s own boot path can authorize a direct registry write
/// (iamacoffeepot/aether#4156). A test fixture *is* the boot path for the
/// registry it just constructed, so it gets an explicit door here rather
/// than the production one being widened: this module compiles only under
/// `cfg(test)` or the `test-support` feature, so no shipping binary can
/// reach it.
pub fn boot_authority() -> BootAuthority {
    BootAuthority::new()
}

/// Build the `(Arc<Registry>, Arc<Mailer>)` seed substrate-internal tests
/// feed to `Builder::<...>::new` and [`unrouted_binding`].
/// Intentionally narrower than [`fresh_substrate`]: no kind descriptors
/// registered, no outbound wired — substrate-internal tests don't exercise
/// descriptor lookup or the unknown-mailbox bubble-up path (ADR-0037).
pub fn bare_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    (registry, mailer)
}

/// Build the seeded `(Arc<Registry>, Arc<Mailer>, Receiver<EgressEvent>)`
/// triple every cap test feeds to `Builder::<TestChassis>::new`, exposing
/// the egress receiver so callers can drain `SourceAddr::Session` sends.
/// The registry is pre-populated with the substrate kind descriptors so
/// tests can address built-in kinds by id without re-registering; the
/// mailer carries a loopback `HubOutbound` so the unknown-mailbox
/// bubble-up path (ADR-0037) is wired but inert — tests that never hit it
/// (audio, fs, http handler paths) see no behavioural difference, and
/// tests that do hit it (rpc, engine proxy) get the connected backend
/// they need.
pub fn fresh_substrate_and_rx() -> (Arc<Registry>, Arc<Mailer>, Receiver<EgressEvent>) {
    let registry = Arc::new(Registry::new());
    let authority = boot_authority();
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&authority, d);
    }
    let (outbound, rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    (registry, mailer, rx)
}

/// The seeded `(Arc<Registry>, Arc<Mailer>)` seed for cap tests that don't
/// observe egress. A thin wrapper over [`fresh_substrate_and_rx`] that
/// drops the receiver.
pub fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    let (registry, mailer, _rx) = fresh_substrate_and_rx();
    (registry, mailer)
}

/// A spawner-less binding over `mailer` whose own mailbox is never
/// registered — the stand-in caller a cap test hands a `NativeCtx` when it
/// drives a handler directly over its `State`.
///
/// The binding's own id is the one the registry refuses to register, so
/// mail addressed to it — an ADR-0093 completion wake, a self-send — never
/// lands in an inbox: it bubbles to the mailer's outbound, where
/// [`drive_task_completion`] and the egress drains read it. Reach for
/// [`registered_binding`] when that mail must reach a handler instead.
pub fn unrouted_binding(mailer: &Arc<Mailer>) -> Arc<NativeBinding> {
    Arc::new(NativeBinding::new_for_test(Arc::clone(mailer), MailboxId(0)))
}

/// A spawner-less binding over `mailer` whose own mailbox is `handler`,
/// registered in `registry` under `name`, returned beside the proven
/// reference to that mailbox.
///
/// Mail to the binding's own mailbox — a worker's completion wake, a
/// coalesced self-wake — lands in `handler`, and the binding stamps the
/// registered mailbox as the source of what it sends, so a test that
/// asserts that source compares the stamped sender against the returned
/// reference. The reference is proven by the registry's own liveness read,
/// the same one a handler's `ctx.resolve_live` takes. `registry` must be
/// the one `mailer` routes through; it is taken explicitly, the way
/// [`boot_test_chassis_with`] takes it.
///
/// # Panics
/// Panics if `name` is already registered.
pub fn registered_binding(
    registry: &Registry,
    mailer: &Arc<Mailer>,
    name: &str,
    handler: Arc<dyn InboxHandler>,
) -> (Arc<NativeBinding>, ErasedActorRef) {
    let mailbox = registry.register_inbox(&boot_authority(), name, handler);
    let reference = registry.resolve_live(mailbox).expect("a freshly registered inbox proves");

    (Arc::new(NativeBinding::new_for_test(Arc::clone(mailer), mailbox)), reference)
}

/// Register `handler` in `registry` under `name` and return the proven
/// reference to it: the peer a cap test hands its code under test as a
/// subscriber, a sender, or a shard, or a route a test stands at a nested
/// position so a spawn there collides.
///
/// `name` is a root name (`test.render.observer`) or a `/`-rendered ADR-0166
/// lineage path (`aether.http.server/aether.http.server.shard:shard-0`). The
/// route stands where the registry's own name lookup looks for that name,
/// the registry's ADR-0099 lineage fold, so `Registry::lookup`,
/// `Registry::resolve_address`, and a spawn claiming the same path all meet
/// it. A root name folds to its name hash, the position it always had.
///
/// The reference is proven by the registry's own liveness read, the same
/// one a handler's `ctx.resolve_live` takes, so a fixture proof and a
/// production proof come from the same code.
///
/// # Panics
/// Panics if `name` is already registered.
pub fn registered_ref(registry: &Registry, name: &str, handler: Arc<dyn InboxHandler>) -> ErasedActorRef {
    try_registered_ref(registry, name, handler).expect("the fixture name is free")
}

/// [`registered_ref`] for a fixture that needs the refusal: `Err` when `name`
/// is already registered, where `registered_ref` panics.
pub fn try_registered_ref(
    registry: &Registry,
    name: &str,
    handler: Arc<dyn InboxHandler>,
) -> Result<ErasedActorRef, NameConflict> {
    registry
        .try_register_inbox_with_id(&boot_authority(), lineage_mailbox_id(name), name, handler)
        .map(|id| registry.resolve_live(id).expect("a freshly registered inbox proves"))
}

/// Retire the route `reference` proves the way `Registry::drop_mailbox`
/// retires one: it goes `Dropped` and keeps its name. A test that stood a
/// collision route with [`registered_ref`] removes it through this, so none
/// of them spells the position it reads.
///
/// # Panics
/// Panics if the route is not live.
pub fn drop_ref(registry: &Registry, reference: ErasedActorRef) {
    registry.drop_mailbox(&boot_authority(), reference.id()).expect("a live registered route drops");
}

/// Boot a `TestChassis` carrying exactly one cap `A` with `config`. The
/// minimal-boot path a cap's dispatcher-thread test reaches for.
pub fn boot_test_chassis_with<A>(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    config: A::Config,
    params: A::Params,
) -> PassiveChassis<TestChassis>
where
    A: Root + NativeActor,
    A::Config: ConfigMember + 'static,
{
    // ADR-0156 §5: compose the cap and stage its explicit `config` in one
    // paired call — the params and the config value bound to `A` together.
    Builder::<TestChassis>::new(Arc::clone(registry), Arc::clone(mailer))
        .with_actor_configured::<A>(params, config)
        .build_passive()
        .expect("test chassis boots")
}

/// Build a `(Arc<Mailer>, Receiver<EgressEvent>)` pair where the
/// mailer's outbound is wired to a loopback channel whose receiver
/// the caller can drain. Mirrors [`fresh_substrate`] but exposes the
/// egress side for tests that need to observe `SourceAddr::Session`
/// sends (the cap-level reply path used by `aether.fs` / `aether.http`
/// / `aether.audio`). The registry is bare — no kind descriptors —
/// so tests can register only what they exercise.
pub fn test_mailer_and_rx() -> (Arc<Mailer>, Receiver<EgressEvent>) {
    let (outbound, rx) = HubOutbound::attached_loopback();
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(registry).with_outbound(outbound));
    (mailer, rx)
}

/// Drive an ADR-0093 dispatch completion through `cap`'s `#[handler(task)]`
/// arm the way the chassis trampoline would.
///
/// A content-gen cap's generate handler now calls
/// `TaskQueue::submit` → `ctx.dispatch_blocking`, which spawns a real
/// worker thread that runs the closure (the stub adapter + staging) and
/// pushes a [`TaskCompletionWake`] at the cap's own mailbox. Under
/// [`unrouted_binding`] that mailbox is unregistered, so the wake bubbles to the
/// loopback outbound as an [`EgressEvent::UnresolvedMail`]. This helper
/// drains egress until that wake lands, then routes it through
/// `cap.__aether_dispatch_envelope(TaskCompletionWake::ID, payload)` — the
/// same entry the chassis dispatcher uses — so the cap's task handler
/// runs `done.resolve(ctx)` (re-replying the worker's staged result to the
/// original caller through the framework-held reply target) and
/// `tasks.on_complete(ctx)`.
///
/// The driving `NativeCtx` carries no inbound reply target ([`Source::NONE`]):
/// the completion's reply routes through the reply target captured at
/// dispatch and parked in the framework's in-flight ledger, not this ctx.
pub fn drive_task_completion<A>(state: &mut A::State, binding: &Arc<NativeBinding>, rx: &Receiver<EgressEvent>)
where
    // Dispatch is a `NativeActor` assoc fn over `&mut Self::State` (ADR-0122
    // identity/runtime split). The param is the associated projection, so `A`
    // is not inferable from it — every call site names it via turbofish.
    A: NativeActor,
{
    let payload = loop {
        let event =
            rx.recv_timeout(Duration::from_secs(2)).expect("test: dispatch completion wake arrives within deadline");
        if let EgressEvent::UnresolvedMail { kind_id, payload, .. } = event
            && kind_id == TaskCompletionWake::ID
        {
            break payload;
        }
    };
    // ADR-0112: route through the macro dispatch seam, which carries the
    // `Manual` ctx. Issue 4158: that seam is also typed by the actor, so build
    // it via `new_for_actor` — `new_dispatching` names none.
    let mut ctx = NativeCtx::new_for_actor(binding, Source::NONE, None, None);
    A::dispatch(state, &mut ctx, TaskCompletionWake::ID, &payload)
        .expect("test: task completion routes to a #[handler(task)] arm");
}

/// Drain egress until a `ToSession` reply of kind `K` arrives, decoding
/// it via the kind codec. Skips non-`ToSession` events and replies of other
/// kinds — a task-backed cap spawns a real ephemeral dispatch thread
/// whose loopback mail (to the unregistered own mailbox of an
/// [`unrouted_binding`]) bubbles up as a non-`ToSession` egress, so a cap
/// test that drives the actual re-reply via `on_*_result` reads past
/// the bubble-up to the `ToSession` re-reply. Shared by the cap test
/// modules.
pub fn decode_session_reply<K>(rx: &Receiver<EgressEvent>) -> K
where
    K: Kind,
{
    loop {
        let event = rx.recv_timeout(Duration::from_secs(2)).expect("test: egress event arrives within deadline");
        if let EgressEvent::ToSession { kind_name, payload, .. } = event
            && kind_name == K::NAME
        {
            return K::decode_from_bytes(&payload).expect("test: reply payload decodes");
        }
    }
}

/// Sibling of [`decode_session_reply`] that also returns which session
/// the reply targeted, for cap tests that fan replies across sessions.
pub fn decode_session_reply_with_session<K>(rx: &Receiver<EgressEvent>) -> (SessionToken, K)
where
    K: Kind,
{
    loop {
        let event = rx.recv_timeout(Duration::from_secs(2)).expect("test: egress event arrives within deadline");
        if let EgressEvent::ToSession { session, kind_name, payload, .. } = event
            && kind_name == K::NAME
        {
            return (session, K::decode_from_bytes(&payload).expect("test: reply payload decodes"));
        }
    }
}

/// Flush the cap's buffered sends, then drain egress asserting the next
/// `UnresolvedMail` carries kind `K`, returning its correlation id. The
/// bare registry has no `aether.render` / `aether.fs`, so a forwarded
/// send bubbles to the loopback outbound; `flush_outbound` is what
/// `NativeCtx::Drop` would otherwise do at the end of a real dispatch
/// turn.
pub fn assert_next_send_kind<K: Kind>(binding: &NativeBinding, rx: &Receiver<EgressEvent>) -> u64 {
    binding.flush_outbound();
    loop {
        let event = rx.recv_timeout(Duration::from_secs(2)).expect("test: egress event arrives within deadline");
        if let EgressEvent::UnresolvedMail { kind_id, correlation_id, .. } = event {
            assert_eq!(kind_id, K::ID, "unexpected bubbled kind");
            return correlation_id;
        }
    }
}

/// `Source` for a session-origin dispatch (id 0) — the shape a cap sees
/// when a session client sends it mail.
pub fn session_sender() -> Source {
    session_sender_with(0)
}

/// [`session_sender`] with an explicit session id, for tests that tell
/// two sessions apart.
pub fn session_sender_with(id: u128) -> Source {
    Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(id))))
}

/// A chassis-rooted chain token for a test that needs a root and no chain
/// behind it: `correlation` under the chassis sender, the shape every real
/// chassis root has. A test outside the substrate reaches for this rather
/// than naming a mailbox to build one.
pub fn token_root(correlation: u64) -> MailId {
    MailId::new(MailboxId::CHASSIS_MAILBOX_ID, correlation)
}

/// `Source` for a correlated no-address reply — the shape a task result
/// (e.g. an `aether.fs` read) carries back into a cap's result handler.
pub fn fs_reply_source(correlation_id: u64) -> Source {
    Source::with_correlation(SourceAddr::None, correlation_id)
}

/// A [`Manual`] dispatch ctx whose inbound is *armed*, so a
/// `#[handler::manual]` cap test can exercise the
/// [`take_inbound`](NativeCtx::take_inbound) reply edge.
///
/// [`NativeCtx::new_dispatching`] carries no envelope, so a handler that
/// retains its inbound panics under it — which leaves the guard edge
/// reachable only through a real dispatcher. Since that edge is the one
/// that answers a `SourceAddr::Component` sender (the hub outbound does
/// not), a cap test without this fixture can only assert the session
/// shape and passes while the wire shape is broken.
pub fn manual_dispatch_ctx<A>(binding: &Arc<NativeBinding>, sender: Source) -> NativeCtx<'_, A, Manual> {
    NativeCtx::with_inbound(
        binding,
        sender,
        None,
        None,
        OwnedDispatch::disarmed_at(
            DispatchParts { sender, ..DispatchParts::new(KindId(0), MailRef::from(Vec::new())) },
            binding.self_mailbox(),
        ),
    )
}

/// Decode the *next* egress as a `ToSession` reply of kind `K`. Strict
/// sibling of [`decode_session_reply`]: it asserts the immediately
/// following egress is a `ToSession` carrying `K`, rather than reading
/// past bubble-ups. For cap tests that drive via the full dispatcher and
/// egress channel (e.g. `render-runtime`) rather than direct `-> R` calls.
pub fn decode_reply<K>(rx: &Receiver<EgressEvent>) -> K
where
    K: Kind,
{
    let event = rx.recv_timeout(Duration::from_secs(1)).expect("test: egress event arrives within 1s deadline");
    let EgressEvent::ToSession { kind_name, payload, .. } = event else {
        panic!("expected ToSession egress, got {event:?}");
    };
    assert_eq!(kind_name, K::NAME);
    K::decode_from_bytes(&payload).expect("test: reply payload decodes")
}

/// Manual tempdir under the system temp root, namespaced by `prefix` and
/// `tag` plus the pid and a nanosecond nonce so concurrent tests never
/// collide. Avoids pulling in the `tempfile` crate; the caller cleans up
/// via [`cleanup`] after asserting.
pub fn scratch_dir(prefix: &str, tag: &str) -> PathBuf {
    let pid = process::id();
    let nonce: u64 = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
        // Nanosecond clock fits comfortably in u64 for the next ~584 years.
        #[allow(clippy::cast_possible_truncation)]
        let nanos = d.as_nanos() as u64;
        nanos
    });
    let path = temp_dir().join(format!("{prefix}-{tag}-{pid}-{nonce}"));
    fs::create_dir_all(&path).expect("test setup: scratch dir creates");
    path
}

/// Remove a [`scratch_dir`] tree, ignoring errors (best-effort teardown).
pub fn cleanup(path: &Path) {
    let _ = fs::remove_dir_all(path);
}
