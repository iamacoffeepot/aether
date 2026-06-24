//! Shared `TestChassis` fixture for unit tests across `aether-capabilities`.
//!
//! Every cap's `#[cfg(test)] mod tests` exercises its `init` / handlers
//! by booting a real [`Builder`] against a no-op chassis declaration. Pre-extraction every site copied
//! the same 8-line `impl Chassis for TestChassis` block; this module is
//! the single canonical declaration so test modules just
//! `use crate::test_chassis::TestChassis;` and address it by the typename
//! the builder expects.
//!
//! Filed by issue 785. The `fresh_substrate` helper extracted by issue
//! 786 lives here too — same six sites all wanted the same
//! `(Arc<Registry>, Arc<Mailer>)` seed for `Builder::new`.

use std::env::temp_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aether_data::{Kind, Source};
use aether_kinds::descriptors;
use aether_substrate::actor::native::binding::NativeBinding;
use aether_substrate::actor::native::ctx::NativeCtx;
use aether_substrate::actor::native::{NativeActor, TaskCompletionWake};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
use aether_substrate::mail::registry::Registry;

/// Canonical test chassis. `build()` is unreachable — every consumer
/// drives the chassis through `Builder::<TestChassis>::new(...)` directly
/// rather than going through `TestChassis::build(())`.
pub struct TestChassis;

//noinspection DuplicatedCode
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is driven by Builder::new directly in unit tests")
    }
}

/// Build the `(Arc<Registry>, Arc<Mailer>)` seed every cap test feeds to
/// `Builder::<TestChassis>::new`. The registry is pre-populated with the
/// substrate kind descriptors so tests can address built-in kinds by id
/// without re-registering; the mailer carries a loopback `HubOutbound`
/// (rx dropped) so the unknown-mailbox bubble-up path (ADR-0037) is
/// wired but inert — tests that never hit it (audio, fs, http handler
/// paths) see no behavioural difference, and tests that do hit it
/// (rpc, engine proxy) get the connected backend they need.
pub fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    (registry, mailer)
}

/// Boot a `TestChassis` carrying exactly one cap `A` with `config`.
/// The minimal-boot path every single-cap cap test reaches for:
///
/// ```ignore
/// let (registry, mailer) = fresh_substrate();
/// let chassis = boot_test_chassis_with::<MyCap>(&registry, &mailer, config);
/// ```
///
/// Multi-cap tests (e.g. `RpcServer` + `TraceObserver` + `TestEcho`) keep
/// their own inline `Builder::<TestChassis>::new(...)` chain because
/// the cap list is the load-bearing part of the scenario.
pub fn boot_test_chassis_with<A>(
    registry: &Arc<Registry>,
    mailer: &Arc<Mailer>,
    config: A::Config,
) -> PassiveChassis<TestChassis>
where
    A: NativeActor,
{
    Builder::<TestChassis>::new(Arc::clone(registry), Arc::clone(mailer))
        .with_actor::<A>(config)
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
pub fn drive_task_completion<A>(
    cap: &mut A,
    binding: &Arc<NativeBinding>,
    rx: &Receiver<EgressEvent>,
) where
    // Spike fold: dispatch is a `NativeActor` assoc fn over `&mut Self::State`.
    // This helper drives un-split caps, so `State = A`.
    A: NativeActor<State = A>,
{
    let payload = loop {
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: dispatch completion wake arrives within deadline");
        if let EgressEvent::UnresolvedMail {
            kind_id, payload, ..
        } = event
            && kind_id == TaskCompletionWake::ID
        {
            break payload;
        }
    };
    // ADR-0112: route through the macro dispatch seam, which carries the
    // `Manual` ctx — build it via `new_dispatching`, not `new`.
    let mut ctx = NativeCtx::new_dispatching(
        binding,
        Source::NONE,
        aether_data::MailId::NONE,
        aether_data::MailId::NONE,
    );
    A::dispatch(cap, &mut ctx, TaskCompletionWake::ID, &payload)
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
        let event = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test: egress event arrives within deadline");
        if let EgressEvent::ToSession {
            kind_name, payload, ..
        } = event
            && kind_name == K::NAME
        {
            return K::decode_from_bytes(&payload).expect("test: reply payload decodes");
        }
    }
}

/// Decode the *next* egress as a `ToSession` reply of kind `K`. Strict
/// sibling of [`decode_session_reply`]: it asserts the immediately
/// following egress is a `ToSession` carrying `K`, rather than reading
/// past bubble-ups. For cap tests that drive via the full dispatcher and
/// egress channel (e.g. `render-native`) rather than direct `-> R` calls.
// Used by render.rs tests (feature = "render-native"); dead_code fires in
// the default build without that feature.
#[allow(dead_code)]
pub fn decode_reply<K>(rx: &Receiver<EgressEvent>) -> K
where
    K: Kind,
{
    let event = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("test: egress event arrives within 1s deadline");
    let EgressEvent::ToSession {
        kind_name, payload, ..
    } = event
    else {
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
