//! Smoke-test component for the postcard receive path. Receives
//! `demo.postcard_request { tag, payload }` (a struct with a `String`
//! and `Vec<u8>` — postcard-shaped, not bytemuck-castable) via the
//! same bare `#[handler]` attribute that cast-shaped components use,
//! and broadcasts a `demo.postcard_observed` cast-shaped acknowledgement
//! so the receive landed observably.
//!
//! Pairs with the cast-shaped `echoer.rs` example to demonstrate that
//! `#[handlers]` dispatches both wire shapes from the same impl block
//! with no per-handler annotation — wire shape is picked at the kind's
//! `Kind` derive site (cast for `#[repr(C)]` + `Pod`, postcard
//! otherwise) and routed through `Kind::decode_from_bytes`. Compiles
//! to wasm; load via `mcp__aether-hub__load_component` and send
//! `demo.postcard_request` to verify the dispatch.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers};
use aether_data::{Kind, Schema};
use bytemuck::{Pod, Zeroable};
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

/// Cast-shaped acknowledgement: empty payload, broadcast to confirm
/// the postcard request was received and decoded. Stays cast so the
/// observation path doesn't depend on the same code path it's testing.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable, Kind, Schema)]
#[kind(name = "demo.postcard_observed")]
pub struct PostcardObserved {
    pub tag_len: u32,
    pub payload_len: u32,
}

pub struct PostcardEchoer {
    broadcast: Sink<PostcardObserved>,
}

#[handlers]
impl Component for PostcardEchoer {
    fn init(ctx: &mut InitCtx<'_>) -> Self {
        PostcardEchoer {
            broadcast: ctx.resolve_sink::<PostcardObserved>("hub.claude.broadcast"),
        }
    }

    /// Decoded postcard payload arrives as the third parameter. No
    /// per-handler annotation needed — `Kind::decode_from_bytes`
    /// (synthesised by the Kind derive on `PostcardRequest` based on
    /// the absence of `#[repr(C)]`) already knows the wire shape.
    #[handler]
    fn on_request(&mut self, ctx: &mut Ctx<'_>, req: PostcardRequest) {
        ctx.send(
            &self.broadcast,
            &PostcardObserved {
                tag_len: req.tag.len() as u32,
                payload_len: req.payload.len() as u32,
            },
        );
    }
}

aether_component::export!(PostcardEchoer);
