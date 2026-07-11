//! A tiny TCP "echo" consumer component.
//!
//! In `wire` it binds an `aether.tcp` listener and names *itself* as the
//! bound consumer (`BindListener.consumer`). Every accepted session then
//! delivers each reassembled length-prefix frame to this component as a
//! `SessionData`. The handler re-frames the body (4-byte LE length prefix)
//! and writes it straight back to the originating session via `SessionWrite`.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_capabilities::TcpCapability;
use aether_capabilities::tcp::{SessionData, TcpWasmExt};

/// Listener subname we bind under `aether.tcp.listener:<name>`.
const LISTENER: &str = "echo";
/// Fixed bind address so an external raw client knows where to dial.
const BIND_ADDR: &str = "127.0.0.1:7777";

pub struct EchoTcp;

#[actor]
impl WasmActor for EchoTcp {
    const NAMESPACE: &'static str = "aether.echo";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(EchoTcp)
    }

    fn wire(&mut self, ctx: &mut WasmCtx<'_>) {
        // Bind a listener and name ourselves as the consumer. Our own
        // registered mailbox name is NAMESPACE rendered into the ADR-0099
        // lineage: `aether.component/aether.embedded:<NAMESPACE>`.
        let consumer = "aether.component/aether.embedded:aether.echo";
        tracing::info!(consumer, listener = LISTENER, addr = BIND_ADDR, "echo wire: binding listener");
        ctx.actor::<TcpCapability>().bind_listener(BIND_ADDR, Some(LISTENER), Some(consumer));
    }

    #[fallback]
    fn on_any(&mut self, _ctx: &mut WasmCtx<'_>, mail: aether_actor::Mail<'_>) {
        tracing::warn!(kind = ?mail.kind(), "echo fallback: unexpected kind");
    }

    #[handler::single]
    fn on_data(&mut self, ctx: &mut WasmCtx<'_>, data: SessionData) {
        tracing::info!(session = %data.session_name, peer = %data.peer, len = data.bytes.len(), "echo on_data");
        // The transport already reassembled the inbound length-prefix frame,
        // so `data.bytes` is the raw body. `SessionWrite` writes raw bytes to
        // the stream (no framing added), so re-frame the body: 4-byte LE length
        // prefix + body, matching the inbound wire format.
        let mut framed = Vec::with_capacity(4 + data.bytes.len());
        framed.extend_from_slice(&(data.bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(&data.bytes);

        ctx.actor::<TcpCapability>().session_write(LISTENER, &data.session_name, &framed);
    }
}

aether_actor::export!(EchoTcp);
