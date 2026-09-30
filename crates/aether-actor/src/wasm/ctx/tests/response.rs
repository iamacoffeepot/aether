//! ADR-0243 §10: a `#[handler::response]` takes its stored request context as
//! a fourth parameter, which the generated arm fills before the call.
//!
//! The host build reads no reply correlation — the `reply_correlation` stub
//! panics, so a host test ctx is a local dispatch whose `in_reply_to` is
//! `None` — so these drive the absent path only: a real `#[actor]` arm
//! dispatched with no correlation. The present path runs end to end on the
//! native runtime, whose arm is the same shape.

use alloc::vec::Vec;

use aether_data::Kind;

use super::{NO_INBOUND_SOURCE, Registry, WasmCtx};
use crate::mail::Mail;
use crate::model::ctx::{Erased, Unchecked};
use crate::wasm::{ActorInitError, WasmInitCtx};

#[aether_data::kind(name = "test.response.required_reply")]
struct RequiredReply {
    value: u32,
}

#[aether_data::kind(name = "test.response.optional_reply")]
struct OptionalReply {
    value: u32,
}

#[aether_data::kind(name = "test.response.context")]
struct ReplyContext {
    tag: u32,
}

const ACTOR: u64 = 0x20;

/// Records each response handler's runs: how often the `C` handler ran, and
/// what the `Option<C>` handler received each time.
#[derive(Default)]
struct Responder {
    required_runs: u32,
    optional_runs: Vec<Option<u32>>,
}

#[crate::actor]
impl crate::WasmActor for Responder {
    const NAMESPACE: &'static str = "test.response.responder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::default())
    }

    #[handler::response]
    fn on_required(&mut self, _ctx: &mut WasmCtx<'_>, _reply: RequiredReply, _context: ReplyContext) {
        self.required_runs += 1;
    }

    #[handler::response]
    fn on_optional(&mut self, _ctx: &mut WasmCtx<'_>, _reply: OptionalReply, context: Option<ReplyContext>) {
        self.optional_runs.push(context.map(|c| c.tag));
    }
}

/// Dispatch `mail` of kind `K` to `responder` through its generated arm, with
/// no reply correlation, and return the arm's code.
fn dispatch_uncorrelated<K: Kind>(responder: &mut Responder, mail: &K) -> u32 {
    let registry = Registry::new();
    let payload = mail.encode_into_bytes();
    // SAFETY: `payload` outlives the `Mail` built over it.
    let mail = unsafe { Mail::__from_ptr(K::ID.0, payload.as_ptr().addr(), payload.len() as u32, 1, 0, ACTOR) };
    let mut ctx: WasmCtx<'_, Erased, Unchecked> = WasmCtx::__new_local_dispatch(ACTOR, &registry, NO_INBOUND_SOURCE);

    <Responder as crate::WasmDispatch<Responder>>::dispatch(responder, &mut ctx, mail)
}

/// Catches an arm that runs a `C` handler with no context to give it, or that
/// reports the skipped mail as unknown so it falls through to a `#[fallback]`:
/// the handler does not run and the mail counts as handled.
#[test]
fn uncorrelated_reply_skips_a_required_context_handler() {
    let mut responder = Responder::default();

    let rc = dispatch_uncorrelated(&mut responder, &RequiredReply { value: 1 });

    assert_eq!(rc, crate::DISPATCH_HANDLED_RELEASE);
    assert_eq!(responder.required_runs, 0, "a `C` handler never runs without its context");
}

/// Catches an arm that skips an `Option<C>` handler, or hands it a context it
/// did not take: the handler runs with `None`.
#[test]
fn uncorrelated_reply_runs_an_optional_context_handler_with_none() {
    let mut responder = Responder::default();

    let rc = dispatch_uncorrelated(&mut responder, &OptionalReply { value: 1 });

    assert_eq!(rc, crate::DISPATCH_HANDLED_RELEASE);
    assert_eq!(responder.optional_runs, [None]);
}
