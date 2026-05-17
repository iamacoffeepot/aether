//! Smoke-test component for the postcard receive path. Receives
//! `demo.postcard_request { tag, payload }` (a struct with a `String`
//! and `Vec<u8>` — postcard-shaped, not bytemuck-castable) via the
//! same bare `#[handler]` attribute that cast-shaped components use.
//!
//! Pre-issue-775 the example broadcast a cast-shaped
//! `demo.postcard_observed` acknowledgement to `hub.claude.broadcast`
//! so the receive landed observably. With `BroadcastCapability`
//! retired the broadcast goes away; the handler still decodes the
//! postcard payload (the dispatch path being smoke-tested) but no
//! observation kind ships.
//!
//! Pairs with the cast-shaped `echoer.rs` example to demonstrate that
//! `#[actor]` dispatches both wire shapes from the same impl block
//! with no per-handler annotation — wire shape is picked at the kind's
//! `Kind` derive site (cast for `#[repr(C)]` + `Pod`, postcard
//! otherwise) and routed through `Kind::decode_from_bytes`. Compiles
//! to wasm; load via `mcp__aether-hub__load_component` and send
//! `demo.postcard_request` to verify the dispatch.

// `#[handler]` keeps `&mut self` per the ADR-0033 / ADR-0038 dispatch
// ABI; the echo body decodes the payload and replies without touching
// component state.
#![allow(clippy::unused_self)]

use aether_actor::{BootError, FfiActor, FfiCtx, Resolver, actor};
use aether_data::{Kind, Schema};
use serde::{Deserialize, Serialize};

/// Postcard-shaped request: contains a `String` and `Vec<u8>` so the
/// derive can't generate a `bytemuck::Pod` impl. Decoding goes through
/// `Mail::decode_postcard`, selected by `#[handler(postcard)]`.
#[derive(Debug, Clone, Kind, Schema, Serialize, Deserialize)]
#[kind(name = "demo.postcard_request")]
pub struct PostcardRequest {
    pub tag: String,
    pub payload: Vec<u8>,
}

pub struct PostcardEchoer;

#[actor]
impl FfiActor for PostcardEchoer {
    const NAMESPACE: &'static str = "postcard_echoer";

    fn init<C>(_ctx: &mut C) -> Result<Self, BootError>
    where
        C: Resolver,
    {
        Ok(PostcardEchoer)
    }

    /// Decoded postcard payload arrives as the third parameter. No
    /// per-handler annotation needed — `Kind::decode_from_bytes`
    /// (synthesised by the Kind derive on `PostcardRequest` based on
    /// the absence of `#[repr(C)]`) already knows the wire shape.
    #[handler]
    fn on_request(&mut self, _ctx: &mut FfiCtx<'_>, _req: PostcardRequest) {
        // Pre-#775 the handler broadcast a `PostcardObserved` ack to
        // the hub fan-out mailbox. Broadcast retired with the cap.
    }
}

aether_actor::export!(PostcardEchoer);
