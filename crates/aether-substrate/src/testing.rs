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
    reason = "the canonical test fixture is a deliberate embedder — it builds a bare `TestChassis` via `Builder::new` rather than the `composed` boot seam production chassis route through"
)]

use std::env::temp_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_actor::Root;
use aether_data::{Kind, SessionToken, Source, SourceAddr, Uuid};
use aether_kinds::descriptors;

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::{NativeActor, TaskCompletionWake};
use crate::chassis::Chassis;
use crate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use crate::chassis::error::BootError;
use crate::config::ConfigMember;
use crate::mail::mailer::Mailer;
use crate::mail::outbound::{EgressEvent, HubOutbound};
use crate::mail::registry::Registry;

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

/// Build the `(Arc<Registry>, Arc<Mailer>)` seed substrate-internal tests
/// feed to `Builder::<...>::new` and `NativeBinding::new_for_test`.
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
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
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
/// `new_for_test` that mailbox is unregistered, so the wake bubbles to the
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
    // `Manual` ctx — build it via `new_dispatching`, not `new`.
    let mut ctx =
        NativeCtx::new_dispatching(binding, Source::NONE, aether_data::MailId::NONE, aether_data::MailId::NONE);
    A::dispatch(state, &mut ctx, TaskCompletionWake::ID, &payload)
        .expect("test: task completion routes to a #[handler(task)] arm");
}

/// Drain egress until a `ToSession` reply of kind `K` arrives, decoding
/// it via the kind codec. Skips non-`ToSession` events and replies of other
/// kinds — the content-gen caps spawn a real ephemeral dispatch thread
/// whose loopback mail (to an unregistered stand-in mailbox in
/// `new_for_test`) bubbles up as a non-`ToSession` egress, so a cap
/// test that drives the actual re-reply via `on_*_result` reads past
/// the bubble-up to the `ToSession` re-reply. Shared by the
/// `aether.anthropic` / `aether.gemini` test modules.
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

/// `Source` for a correlated no-address reply — the shape a task result
/// (e.g. an `aether.fs` read) carries back into a cap's result handler.
pub fn fs_reply_source(correlation_id: u64) -> Source {
    Source::with_correlation(SourceAddr::None, correlation_id)
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
