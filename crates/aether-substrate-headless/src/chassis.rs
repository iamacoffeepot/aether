//! Headless chassis-registered control-plane handler.
//!
//! Headless has no GPU and no window, so `capture_frame`,
//! `set_window_mode`, and `platform_info` have nothing to answer
//! with. Rather than letting core's `ControlPlane` warn-drop the mail
//! (which would leave the sender hanging on its await-reply slot
//! until the hub's timeout fires), this closure replies inline with
//! an explicit `Err { error: ... }` so MCP tool calls fail fast and
//! with a diagnosable message. See ADR-0035 § Consequences (neutral):
//! "A headless chassis receiving set_window_mode replies with an
//! unsupported error".

use std::sync::Arc;

use aether_data::{Kind, KindId};
use aether_kinds::{Advance, CaptureFrame, PlatformInfo, SetWindowMode, SetWindowTitle};
use aether_substrate_core::{
    ChassisControlHandler, HubOutbound, ReplyTo,
    capture::{
        reply_unsupported_advance, reply_unsupported_capture_frame,
        reply_unsupported_platform_info, reply_unsupported_window_mode,
        reply_unsupported_window_title,
    },
};

const UNSUPPORTED: &str = "unsupported on headless chassis — no GPU or window peripherals";
const UNSUPPORTED_ADVANCE: &str =
    "unsupported on headless chassis — aether.test_bench.advance is test-bench-only (ADR-0067)";

pub fn chassis_control_handler(outbound: Arc<HubOutbound>) -> ChassisControlHandler {
    Arc::new(
        move |kind: KindId, kind_name: &str, sender: ReplyTo, _bytes: &[u8]| match kind {
            CaptureFrame::ID => {
                reply_unsupported_capture_frame(&outbound, sender, UNSUPPORTED);
            }
            SetWindowMode::ID => {
                reply_unsupported_window_mode(&outbound, sender, UNSUPPORTED);
            }
            SetWindowTitle::ID => {
                reply_unsupported_window_title(&outbound, sender, UNSUPPORTED);
            }
            Advance::ID => {
                reply_unsupported_advance(&outbound, sender, UNSUPPORTED_ADVANCE);
            }
            PlatformInfo::ID => {
                // PlatformInfoResult::Err also exists — future work
                // could return a partial Ok (OS + engine info, empty
                // GPU/monitors) once headless needs that detail.
                reply_unsupported_platform_info(&outbound, sender, UNSUPPORTED);
            }
            _ => {
                tracing::warn!(
                    target: "aether_substrate::chassis",
                    kind = %kind_name,
                    "headless chassis has no handler for control kind — dropping",
                );
            }
        },
    )
}
