//! Test-support kinds + actors for the DAG-executor scenario tests
//! (iamacoffeepot/aether#976). Mirrors `rpc::test_echo`: the kinds live
//! at module root so the `Kind` derive's inventory submission registers
//! them in `aether_kinds::descriptors::all()` for the test substrate's
//! registry walk, and the actors are real `NativeActor`s booted through
//! the same `Builder` the production caps use.
//!
//! - [`TestSourceActor`] answers a [`TestSourceRequest`] with a
//!   [`TestReadResult`] (`Ok`/`Err`) — stands in for `aether.fs` /
//!   any effectful source.
//! - [`TestObserverActor`] / [`TestParallelObserverActor`] /
//!   [`TestBundleObserverActor`] consume `Ref<...>` slots and record
//!   the resolved values into a shared buffer the test asserts against.
//! - [`TestCallActor`] is the mid-graph `Call` target: configurable to
//!   reply once, N times, or never; the never-reply mode exercises the
//!   per-`Call` settlement timeout.
//! - [`TestDeferredCallActor`] answers off-thread through the ADR-0093
//!   hold-until-resolve dispatch (`TaskQueue` over `ctx.dispatch_blocking`
//!   — spawned worker + framework-held settlement hold), exercising the
//!   exact-settlement-through-the-hold path.

use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use aether_data::Ref;
use aether_data::transform;
use aether_kinds::Bundle;

/// Source request — the DAG `Source` node's opaque payload. `fail`
/// makes the source reply an `Err` variant (the
/// `propagates_source_err` fixture).
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.dag.test.source_request")]
pub struct TestSourceRequest {
    pub value: u64,
    pub fail: bool,
}

/// Source reply kind — an `Ok`/`Err` enum so the
/// `propagates_source_err` fixture can assert the `Err` variant
/// resolves into the observer's `Ref<TestReadResult>` slot inline.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.test.read_result")]
pub enum TestReadResult {
    Ok { value: u64 },
    Err { message: String },
}

/// Observer request — one `Ref<TestReadResult>` input slot.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.test.observed")]
pub struct TestObserved {
    pub input: Ref<TestReadResult>,
}

/// Two-slot observer request — for the parallel-sources fixture.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.test.observed2")]
pub struct TestObserved2 {
    pub a: Ref<TestReadResult>,
    pub b: Ref<TestReadResult>,
}

/// Bundle-consuming observer request — for the `Call`-output fixtures.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.test.bundle_observed")]
pub struct TestBundleObserved {
    pub input: Ref<Bundle>,
}

/// `Call` request — the mid-graph effectful dispatch's payload. Like an
/// observer it carries a `Ref<TestReadResult>` input slot fed by an
/// upstream edge; the cap reads the resolved value to seed its replies.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.test.call_request")]
pub struct TestCallRequest {
    pub input: Ref<TestReadResult>,
}

/// Read the `value` out of a resolved [`TestReadResult`] ref slot, or
/// `0` for an `Err` / unresolved slot. Used by the `Call` test caps to
/// seed their replies from the upstream source.
#[must_use]
pub fn ref_value(input: &Ref<TestReadResult>) -> u64 {
    match input {
        Ref::Inline(TestReadResult::Ok { value }) => *value,
        _ => 0,
    }
}

/// `Call` reply — one element per emission.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.dag.test.call_reply")]
pub struct TestCallReply {
    pub index: u64,
}

/// Shared recording buffer the observer caps push their resolved
/// payloads into, so the test thread can assert what the observer saw.
pub type Recorder<T> = Arc<Mutex<Vec<T>>>;

/// Config for [`TestCallActor`]: how many correlated replies to emit
/// before the handler returns (so the chain settles), or `never` to
/// spin a worker thread that holds the chain open forever (the
/// settlement-timeout fixture).
#[derive(Clone)]
pub struct TestCallConfig {
    pub replies: u64,
    pub never: bool,
}

#[aether_actor::bridge(singleton)]
mod test_source {
    use super::{TestReadResult, TestSourceRequest};
    use aether_actor::{OutboundReply, actor};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct TestSourceActor;

    #[actor]
    impl NativeActor for TestSourceActor {
        type Config = ();
        const NAMESPACE: &'static str = "aether.dag.test.source";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
        #[handler]
        fn on_source(&mut self, ctx: &mut NativeCtx<'_>, mail: TestSourceRequest) {
            let reply = if mail.fail {
                TestReadResult::Err {
                    message: format!("source {} failed", mail.value),
                }
            } else {
                TestReadResult::Ok { value: mail.value }
            };
            ctx.reply(&reply);
        }
    }
}

#[aether_actor::bridge(singleton)]
mod test_observer {
    use super::{Recorder, TestObserved};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct TestObserverActor {
        recorder: Recorder<TestObserved>,
    }

    #[actor]
    impl NativeActor for TestObserverActor {
        type Config = Recorder<TestObserved>;
        const NAMESPACE: &'static str = "aether.dag.test.observer";

        fn init(
            recorder: Recorder<TestObserved>,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { recorder })
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_observed(&mut self, _ctx: &mut NativeCtx<'_>, mail: TestObserved) {
            self.recorder
                .lock()
                .expect("recorder mutex poisoned")
                .push(mail);
        }
    }
}

#[aether_actor::bridge(singleton)]
mod test_parallel_observer {
    use super::{Recorder, TestObserved2};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct TestParallelObserverActor {
        recorder: Recorder<TestObserved2>,
    }

    #[actor]
    impl NativeActor for TestParallelObserverActor {
        type Config = Recorder<TestObserved2>;
        const NAMESPACE: &'static str = "aether.dag.test.parallel_observer";

        fn init(
            recorder: Recorder<TestObserved2>,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { recorder })
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_observed2(&mut self, _ctx: &mut NativeCtx<'_>, mail: TestObserved2) {
            self.recorder
                .lock()
                .expect("recorder mutex poisoned")
                .push(mail);
        }
    }
}

#[aether_actor::bridge(singleton)]
mod test_bundle_observer {
    use super::{Recorder, TestBundleObserved};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct TestBundleObserverActor {
        recorder: Recorder<TestBundleObserved>,
    }

    #[actor]
    impl NativeActor for TestBundleObserverActor {
        type Config = Recorder<TestBundleObserved>;
        const NAMESPACE: &'static str = "aether.dag.test.bundle_observer";

        fn init(
            recorder: Recorder<TestBundleObserved>,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { recorder })
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_bundle_observed(&mut self, _ctx: &mut NativeCtx<'_>, mail: TestBundleObserved) {
            self.recorder
                .lock()
                .expect("recorder mutex poisoned")
                .push(mail);
        }
    }
}

#[aether_actor::bridge(singleton)]
mod test_call {
    use super::{TestCallConfig, TestCallReply, TestCallRequest};
    use aether_actor::{OutboundReply, actor};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::runtime::trace::SettlementHold;

    /// Mid-graph `Call` target. Synchronous-reply mode: emit `replies`
    /// correlated [`TestCallReply`]s in the handler (so all `Sent`
    /// events precede the handler's `Finished`, and the call's chain
    /// settles when the handler returns). `never` mode acquires a
    /// [`SettlementHold`] on `call_root` and never releases it (stashed
    /// in actor state), so the chain genuinely never reaches
    /// `(in_flight == 0 && held_open == 0)` and `Settled` never fires —
    /// the per-`Call` timeout fixture pairs it with a tiny timeout to
    /// assert node failure (a never-settling producer is a node failure,
    /// not a partial bundle).
    pub struct TestCallActor {
        config: TestCallConfig,
        /// Held-forever guards for `never` mode — never dropped, so the
        /// chain never settles. Lives in single-threaded actor state.
        held: Vec<SettlementHold>,
    }

    #[actor]
    impl NativeActor for TestCallActor {
        type Config = TestCallConfig;
        const NAMESPACE: &'static str = "aether.dag.test.call";

        fn init(config: TestCallConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                config,
                held: Vec::new(),
            })
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_call(&mut self, ctx: &mut NativeCtx<'_>, mail: TestCallRequest) {
            let _ = &mail.input;
            if self.config.never {
                // Acquire a hold on the call's chain root and never
                // release it: the chain never settles, so `Settled`
                // never fires and only the per-`Call` timeout can close
                // the node (as a failure). Acquired before the handler
                // returns so `HoldOpen` precedes `Finished`.
                let hold = ctx.mailer().acquire_settlement_hold(ctx.in_flight_root());
                self.held.push(hold);
                return;
            }
            for index in 0..self.config.replies {
                ctx.reply(&TestCallReply { index });
            }
        }
    }
}

#[aether_actor::bridge(singleton)]
mod test_deferred_call {
    use super::{TestCallReply, TestCallRequest, ref_value};
    use crate::contentgen::task_queue::TaskQueue;
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
    use aether_substrate::chassis::error::BootError;
    use std::thread;
    use std::time::Duration;

    /// `Call` target that answers off-thread through the ADR-0093
    /// hold-until-resolve dispatch (`TaskQueue` over
    /// `ctx.dispatch_blocking`) — the framework holds a `SettlementHold`
    /// that keeps `call_root` open across the async reply
    /// (iamacoffeepot/aether#1031). Exercises that the executor's bundle
    /// stays open until the worker's deferred reply lands.
    pub struct TestDeferredCallActor {
        tasks: TaskQueue,
    }

    #[actor]
    impl NativeActor for TestDeferredCallActor {
        type Config = ();
        const NAMESPACE: &'static str = "aether.dag.test.deferred_call";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                tasks: TaskQueue::new(4),
            })
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_call(&mut self, ctx: &mut NativeCtx<'_>, mail: TestCallRequest) {
            let value = ref_value(&mail.input);
            self.tasks.submit(ctx, move || {
                thread::sleep(Duration::from_millis(50));
                TestCallReply { index: value }
            });
        }

        #[handler(task)]
        fn on_call_done(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<TestCallReply>) {
            done.resolve(ctx);
            self.tasks.on_complete(ctx);
        }
    }
}

/// A postcard-shape number kind — the transform fixtures' input +
/// output (ADR-0048 §3, iamacoffeepot/aether#1012). Postcard (serde)
/// rather than cast because `ctx.reply` requires `Serialize`; the
/// transform's decode / encode picks postcard automatically from the
/// non-`#[repr(C)]` shape, so source bytes and transform input bytes
/// agree.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.dag.test.number")]
pub struct TestNumber {
    pub value: u64,
    // A structurally-distinguishing second field. Canonical schema
    // bytes are positional-only (no field names), so a bare `{ value:
    // u64 }` would collide with every other single-`u64` kind in the
    // test vocabulary (`TestNumberRequest`, `TestCallReply`, …) and the
    // observer's `Ref<TestNumber>` slot would resolve to the wrong kind
    // id. The extra `u32` makes the `{ u64, u32 }` shape unique.
    pub tag: u32,
}

/// Source request for the number source — `value` seeds the reply.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.dag.test.number_request")]
pub struct TestNumberRequest {
    pub value: u64,
}

/// Observer request consuming one `Ref<TestNumber>` slot — the
/// transform fixtures wire a transform's output into this so the test
/// asserts the resolved value.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.test.number_observed")]
pub struct TestNumberObserved {
    pub input: Ref<TestNumber>,
}

/// Variable-length output kind for the `big_output` fixture.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.dag.test.bytes")]
pub struct TestBytes {
    pub bytes: Vec<u8>,
}

/// Gate the `slow` transform spins on. The timeout / off-thread fixtures
/// open it (set `true`) after asserting the executor behaviour, so the
/// orphaned worker can finish and the pool joins cleanly on shutdown.
/// A transform fn references this `static` directly (a path expression
/// to a static is not on the deny-list); the busy-spin keeps the body
/// free of any `std::time` / `core::time` path the purity scan rejects.
pub static SLOW_TRANSFORM_GATE: AtomicBool = AtomicBool::new(false);

/// Pure transform: double the wrapped value. The headline happy-path
/// fixture's compute.
#[transform]
fn double(x: TestNumber) -> TestNumber {
    TestNumber {
        value: x.value.wrapping_mul(2),
        tag: x.tag,
    }
}

/// Panicking transform — exercises ADR-0048 §6 panic = failure.
#[transform]
fn boom(_x: TestNumber) -> TestNumber {
    panic!("boom");
}

/// Counts actual `seed` invocations — the cache-hit fixture asserts it
/// stays at 1 across two identical DAGs (a re-invocation would bump it).
/// Incrementing a `static` atomic from a transform body is permitted by
/// the deny-list (no host fn / context / time path); it makes the
/// invoke count directly observable from the test thread, which the
/// cap-owned executor's `transform_call_count` is not.
pub static SEED_INVOKE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Zero-input transform — produces a constant. Two DAGs each containing
/// just this node share the same content-address (`f(transform_id,
/// [])`), so the second hits the cache: the
/// `transform_skips_invoke_on_cache_hit` fixture (ADR-0048 §4,
/// iamacoffeepot/aether#982) asserts [`SEED_INVOKE_COUNT`] stays at 1.
#[transform]
fn seed() -> TestNumber {
    SEED_INVOKE_COUNT.fetch_add(1, Ordering::AcqRel);
    TestNumber { value: 7, tag: 0 }
}

/// Spinning transform — busy-waits on `SLOW_TRANSFORM_GATE` so the
/// timeout / off-thread fixtures can hold it open. No `std::time` path
/// (the deny-list forbids it); a bare spin loop over a `static` flag.
#[transform]
fn slow(x: TestNumber) -> TestNumber {
    while !SLOW_TRANSFORM_GATE.load(Ordering::Acquire) {
        spin_loop();
    }
    x
}

/// Transform that produces a large output — exercises the ADR-0048 §6
/// output-byte cap when paired with a tiny
/// `AETHER_TRANSFORM_MAX_OUTPUT_BYTES`.
#[transform]
fn big_output(x: TestNumber) -> TestBytes {
    let len = usize::try_from(x.value).unwrap_or(usize::MAX).min(1 << 20);
    TestBytes {
        bytes: vec![0u8; len],
    }
}

#[aether_actor::bridge(singleton)]
mod test_number_source {
    use super::{TestNumber, TestNumberRequest};
    use aether_actor::{OutboundReply, actor};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    /// Source replying a [`TestNumber`] — feeds a transform's input
    /// handle with the cast-shape bytes the transform decodes.
    pub struct TestNumberSourceActor;

    #[actor]
    impl NativeActor for TestNumberSourceActor {
        type Config = ();
        const NAMESPACE: &'static str = "aether.dag.test.number_source";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
        #[handler]
        fn on_request(&mut self, ctx: &mut NativeCtx<'_>, mail: TestNumberRequest) {
            ctx.reply(&TestNumber {
                value: mail.value,
                tag: 0,
            });
        }
    }
}

#[aether_actor::bridge(singleton)]
mod test_number_observer {
    use super::{Recorder, TestNumberObserved};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    /// Observer recording the resolved `Ref<TestNumber>` it receives —
    /// the transform fixtures assert against the recorded value.
    pub struct TestNumberObserverActor {
        recorder: Recorder<TestNumberObserved>,
    }

    #[actor]
    impl NativeActor for TestNumberObserverActor {
        type Config = Recorder<TestNumberObserved>;
        const NAMESPACE: &'static str = "aether.dag.test.number_observer";

        fn init(
            recorder: Recorder<TestNumberObserved>,
            _ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self { recorder })
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_number_observed(&mut self, _ctx: &mut NativeCtx<'_>, mail: TestNumberObserved) {
            self.recorder
                .lock()
                .expect("recorder mutex poisoned")
                .push(mail);
        }
    }
}

/// Resolve the `double` transform's global id from the link-time
/// inventory, for descriptor construction in the fixtures.
#[must_use]
pub fn double_transform_id() -> aether_data::TransformId {
    transform_id_by_name("double")
}

/// Resolve the `boom` transform's id.
#[must_use]
pub fn boom_transform_id() -> aether_data::TransformId {
    transform_id_by_name("boom")
}

/// Resolve the `slow` transform's id.
#[must_use]
pub fn slow_transform_id() -> aether_data::TransformId {
    transform_id_by_name("slow")
}

/// Resolve the `big_output` transform's id.
#[must_use]
pub fn big_output_transform_id() -> aether_data::TransformId {
    transform_id_by_name("big_output")
}

/// Resolve the zero-input `seed` transform's id.
#[must_use]
pub fn seed_transform_id() -> aether_data::TransformId {
    transform_id_by_name("seed")
}

/// Look up a registered transform's id by its fn-name tail.
fn transform_id_by_name(tail: &str) -> aether_data::TransformId {
    let Some(entry) = aether_data::transforms().find(|t| t.name.ends_with(&format!("::{tail}")))
    else {
        panic!("transform `{tail}` not registered in link-time inventory");
    };
    entry.transform_id
}

// The actor structs are re-exported at this module's root by the
// `#[bridge]` macro itself — no explicit `pub use` needed.
