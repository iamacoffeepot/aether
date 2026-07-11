//! Minimal multi-reply probe component.
//!
//! One `#[handler::multi]` handler answers a `ProbeRequest` by emitting three
//! values of one declared result kind, in order:
//!   1. `Ok  { step: 1, bytes: [] }`
//!   2. `Err { message: "expected probe error" }`
//!   3. `Ok  { step: 2, bytes: <~24 KiB> }`
//!
//! Used to exercise the `send_mail` reply projection (terminal / none / all),
//! recognized decoded-`Err` retention, and the large-`Bytes` leaf spill.

use aether_actor::{ActorInitError, Emit, Multi, WasmActor, WasmCtx, WasmInitCtx, actor};
use serde::{Deserialize, Serialize};

/// Trigger kind: mail this to the probe to make it emit its three replies.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "probe.request")]
pub struct ProbeRequest {}

/// The single declared result kind. `Ok` carries a step index and an optional
/// payload of bytes; `Err` carries a message. The projector recognizes the
/// `Err` arm as an error by its decoded shape.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "probe.result")]
pub enum ProbeResult {
    Ok { step: u32, bytes: Vec<u8> },
    Err { message: String },
}

/// Per-instance probe state (none needed).
pub struct Probe {}

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "multi_reply_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe {})
    }

    /// Emits three `ProbeResult` values in order: `Ok{step:1}`, an
    /// `Err`, then `Ok{step:2}` with ~24 KiB of bytes.
    #[handler::multi]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_, Multi<ProbeResult>>, _req: ProbeRequest) {
        ctx.emit(&ProbeResult::Ok { step: 1, bytes: Vec::new() });
        ctx.emit(&ProbeResult::Err { message: "expected probe error".to_string() });
        ctx.emit(&ProbeResult::Ok { step: 2, bytes: vec![0xAB; 24 * 1024] });
    }
}

aether_actor::export!(Probe);
