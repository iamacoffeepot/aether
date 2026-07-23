//! Issue 2791: end-to-end request-correlation fixture.
//!
//! The trigger fires two `aether.fs.read` requests for the same namespace/path
//! and records the two returned request ids. The two `ReadResult` payloads are
//! intentionally indistinguishable by echoed fields; the fixture only reports
//! success after both replies match via `ctx.in_reply_to()`.

use aether_actor::{ActorInitError, MailSender, Manual, RequestId, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_fs::{FsCapability, Read, ReadResult};
use aether_test_fixtures_kinds::{FsDemuxReport, RunFsDemux, SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME};

#[derive(Default)]
pub struct FsDemux {
    first: Option<RequestId>,
    second: Option<RequestId>,
    first_matched: bool,
    second_matched: bool,
}

#[actor]
impl WasmActor for FsDemux {
    const NAMESPACE: &'static str = "test.fs_demux";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::default())
    }

    #[handler::single]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, msg: RunFsDemux) {
        self.first = None;
        self.second = None;
        self.first_matched = false;
        self.second_matched = false;

        let fs = ctx.actor::<FsCapability>();
        let read = Read { namespace: msg.namespace, path: msg.path };
        self.first = Some(fs.send_tracked(&read));
        self.second = Some(fs.send_tracked(&read));
    }

    #[handler::manual]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_, Manual>, _reply: ReadResult) {
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
            ctx.send_to_named::<FsDemuxReport>(
                SUBSTRATE_HARNESS_OBSERVER_MAILBOX_NAME,
                &FsDemuxReport { first_matched: true, second_matched: true },
            );
        }
    }
}
