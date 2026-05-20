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
//! - [`TestDeferredCallActor`] answers off-thread through the
//!   content-gen `InFlightDispatch` (spawn-and-die worker + settlement
//!   hold), exercising the exact-settlement-through-the-hold path.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use aether_data::Ref;
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
    use aether_actor::{MailCtx, actor};
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
    use aether_actor::{MailCtx, actor};
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
                let hold = ctx
                    .mailer()
                    .acquire_settlement_hold(ctx.in_flight_root());
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
    use crate::contentgen::dispatch::{BlockingCall, InFlightDispatch};
    use aether_actor::{actor, actor::ctx::OutboundReply};
    use aether_data::{Kind, KindId, MailboxId, ReplyTo};
    use aether_substrate::Mailer;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// `Call` target that answers off-thread through the content-gen
    /// [`InFlightDispatch`] — spawn-and-die worker + a [`SettlementHold`]
    /// that keeps `call_root` open across the async reply
    /// (iamacoffeepot/aether#1031). Exercises that the executor's bundle
    /// stays open until the worker's deferred reply lands.
    pub struct TestDeferredCallActor {
        dispatch: InFlightDispatch,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    }

    #[actor]
    impl NativeActor for TestDeferredCallActor {
        type Config = ();
        const NAMESPACE: &'static str = "aether.dag.test.deferred_call";

        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                dispatch: InFlightDispatch::new(4),
                mailer: ctx.mailer(),
                self_mailbox: ctx.self_id(),
            })
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_call(&mut self, ctx: &mut NativeCtx<'_>, mail: TestCallRequest) {
            let value = ref_value(&mail.input);
            let request_id = value;
            let reply_to = OutboundReply::reply_target(ctx).unwrap_or(ReplyTo::NONE);
            let root = ctx.in_flight_root();
            let call: BlockingCall = Box::new(move || {
                thread::sleep(Duration::from_millis(50));
                let reply = TestCallReply { index: value };
                (
                    KindId(<TestCallReply as Kind>::ID.0),
                    reply.encode_into_bytes(),
                )
            });
            self.dispatch.submit(
                &self.mailer,
                self.self_mailbox,
                root,
                request_id,
                reply_to,
                call,
            );
        }

        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_result(&mut self, ctx: &mut NativeCtx<'_>, mail: TestCallReply) {
            if let Some(landed) = self.dispatch.take_landed(mail.index) {
                OutboundReply::reply_to(ctx, landed.reply_to, &mail);
                drop(landed);
            }
            let _ = self
                .dispatch
                .on_reply_landed(&self.mailer, self.self_mailbox);
        }
    }
}

// The actor structs are re-exported at this module's root by the
// `#[bridge]` macro itself — no explicit `pub use` needed.
