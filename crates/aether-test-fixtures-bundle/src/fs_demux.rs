//! Issue 2791: end-to-end request-correlation fixture.
//!
//! [`RunFsDemux`] fires two `aether.fs.read` requests for the same namespace/path
//! and records the two returned request ids. The two `ReadResult` payloads are
//! intentionally indistinguishable by echoed fields; the fixture only reports
//! success after both replies match via `ctx.in_reply_to()`.
//!
//! [`RunFsContextDemux`] (issue 5508) is a separate additive flow on the same
//! actor: two in-flight reads carry distinct typed contexts, and the same
//! `ReadResult` handler recovers them by trying each context type in turn, A
//! first, so the reply carrying context B crosses a wrong-kind take of A.

use aether_actor::{ActorInitError, Erased, Manual, Reaches, RequestId, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_fs::{FsCapability, NamespaceAddr, Read, ReadResult};
use aether_test_fixtures_kinds::{
    FsContextDemuxReport, FsDemuxReport, RunFsContextDemux, RunFsDemux, SubstrateHarnessObserver,
};

const CONTEXT_A_PAYLOAD: u32 = 11;
const CONTEXT_B_PAYLOAD: u32 = 29;

#[aether_data::kind(name = "aether.test_fixtures.fs_demux_context_a", no_serde)]
struct FsDemuxContextA {
    payload: u32,
}

#[aether_data::kind(name = "aether.test_fixtures.fs_demux_context_b", no_serde)]
struct FsDemuxContextB {
    payload: u32,
}

#[derive(Default)]
pub struct FsDemux {
    first: Option<RequestId>,
    second: Option<RequestId>,
    first_matched: bool,
    second_matched: bool,
    context_first_payload: Option<u32>,
    context_second_payload: Option<u32>,
}

#[actor]
impl WasmActor for FsDemux {
    const NAMESPACE: &'static str = "test.fs_demux";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::default())
    }

    #[handler::single]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, msg: RunFsDemux) {
        *self = Self::default();

        let fs = ctx.actor::<FsCapability>();
        let read = Read { addr: NamespaceAddr::new(msg.namespace, msg.path) };
        self.first = Some(fs.send_tracked(&read));
        self.second = Some(fs.send_tracked(&read));
    }

    #[handler::single]
    fn on_run_context(&mut self, ctx: &mut WasmCtx<'_>, msg: RunFsContextDemux) {
        *self = Self::default();

        let fs = ctx.actor::<FsCapability>();
        let read = Read { addr: NamespaceAddr::new(msg.namespace, msg.path) };
        let _ = fs.send_with_context(&read, &FsDemuxContextA { payload: CONTEXT_A_PAYLOAD });
        let _ = fs.send_with_context(&read, &FsDemuxContextB { payload: CONTEXT_B_PAYLOAD });
    }

    #[handler::manual]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, _reply: ReadResult) {
        if self.handle_typed_context(ctx) {
            return;
        }

        let Some(request) = ctx.in_reply_to() else {
            tracing::warn!(target: "test.fs_demux", "read_result carried no request id");
            return;
        };
        if Some(request) == self.first {
            self.first_matched = true;
        } else if Some(request) == self.second {
            self.second_matched = true;
        } else {
            tracing::warn!(
                target: "test.fs_demux",
                request_id = request.0,
                "read_result request id did not match either pending read",
            );
            return;
        }

        if self.first_matched && self.second_matched {
            tracing::info!(
                target: "test.fs_demux",
                "fs_demux first_matched=true second_matched=true",
            );
            ctx.actor::<SubstrateHarnessObserver>().send(&FsDemuxReport { first_matched: true, second_matched: true });
        }
    }
}

impl FsDemux {
    /// Recover the typed context for [`RunFsContextDemux`] by trying each
    /// context type in turn, A first: a wrong-kind take leaves the context
    /// stored, so the reply carrying context B still recovers it. Returns
    /// whether this reply carried either context.
    fn handle_typed_context<A: Reaches<SubstrateHarnessObserver>>(&mut self, ctx: &mut WasmCtx<'_, A, Manual>) -> bool {
        if let Some(context) = ctx.take_context::<FsDemuxContextA>() {
            if context.payload != CONTEXT_A_PAYLOAD {
                tracing::warn!(
                    target: "test.fs_demux",
                    payload = context.payload,
                    "context A payload mismatch",
                );
                return true;
            }
            self.context_first_payload = Some(context.payload);
        } else if let Some(context) = ctx.take_context::<FsDemuxContextB>() {
            if context.payload != CONTEXT_B_PAYLOAD {
                tracing::warn!(
                    target: "test.fs_demux",
                    payload = context.payload,
                    "context B payload mismatch",
                );
                return true;
            }
            self.context_second_payload = Some(context.payload);
        } else {
            return false;
        }

        if let (Some(first_payload), Some(second_payload)) = (self.context_first_payload, self.context_second_payload) {
            tracing::info!(
                target: "test.fs_demux",
                first_payload,
                second_payload,
                "fs_context_demux recovered both contexts by trying each type in turn",
            );
            ctx.actor::<SubstrateHarnessObserver>().send(&FsContextDemuxReport { first_payload, second_payload });
        }
        true
    }
}
