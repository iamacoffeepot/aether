//! The native HTTP handler — an instanced native actor that answers every
//! `aether.http.server.request` with a fixed `200` body. It carries no logic
//! beyond the reply, so a request's latency measures the server + mail
//! round-trip alone (accept → parse → dispatch mail → handler → reply mail →
//! socket write), with no wasm guest call in the path. This is the *floor* the
//! wasm-handler mode is compared against.
//!
//! Hand-written (not via `#[actor]`) so it needs no per-crate `runtime`
//! feature: the trait impls are the same set the mail-perf harness's `Relay`
//! spells out. `Many` resolver ⇒ instanced ⇒ the server can `spawn_actor` it
//! onto a stock `HeadlessChassis`; its mailbox is `"{NAMESPACE}:{subname}"`,
//! which the server points `handler_mailbox` at.

use aether_actor::{Addressable, HandlesKind, Lifecycle, Many, OutboundReply};
use aether_capabilities::http::kinds::{HttpServerRequest, HttpServerResponse};
use aether_data::{Kind, KindId};
use aether_substrate::{BootError, Dispatch, Manual, NativeActor, NativeCtx, NativeInitCtx};

/// The instanced native handler actor.
pub struct NativeHandler;

impl Addressable for NativeHandler {
    const NAMESPACE: &'static str = "httpstress.native";
    type Resolver = Many;
}

impl HandlesKind<HttpServerRequest> for NativeHandler {}

impl Lifecycle<Self> for NativeHandler {
    type Config = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a>;

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(NativeHandler)
    }
}

impl NativeActor for NativeHandler {
    type State = Self;
}

impl Dispatch<Self> for NativeHandler {
    fn dispatch(
        _state: &mut Self,
        ctx: &mut NativeCtx<'_, Manual>,
        kind: KindId,
        _payload: &[u8],
    ) -> Option<()> {
        if kind.0 != HttpServerRequest::ID.0 {
            return None;
        }
        ctx.reply(&HttpServerResponse {
            status: 200,
            headers: Vec::new(),
            body: crate::RESPONSE_BODY.to_vec(),
        });
        Some(())
    }
}
