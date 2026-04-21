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

use aether_hub_protocol::SessionToken;
use aether_kinds::{
    CaptureFrame, CaptureFrameResult, PlatformInfo, SetWindowMode, SetWindowModeResult,
};
use aether_mail::Kind;
use aether_substrate_core::{ChassisControlHandler, HubOutbound};

const UNSUPPORTED: &str = "unsupported on headless chassis — no GPU or window peripherals";

pub fn chassis_control_handler(outbound: Arc<HubOutbound>) -> ChassisControlHandler {
    Arc::new(
        move |kind_id: u64, kind_name: &str, sender: SessionToken, _bytes: &[u8]| {
            if kind_id == CaptureFrame::ID {
                outbound.send_reply(
                    sender,
                    &CaptureFrameResult::Err {
                        error: UNSUPPORTED.to_owned(),
                    },
                );
            } else if kind_id == SetWindowMode::ID {
                outbound.send_reply(
                    sender,
                    &SetWindowModeResult::Err {
                        error: UNSUPPORTED.to_owned(),
                    },
                );
            } else if kind_id == PlatformInfo::ID {
                // PlatformInfoResult::Err also exists — future work
                // could return a partial Ok (OS + engine info, empty
                // GPU/monitors) once headless needs that detail.
                outbound.send_reply(
                    sender,
                    &aether_kinds::PlatformInfoResult::Err {
                        error: UNSUPPORTED.to_owned(),
                    },
                );
            } else {
                tracing::warn!(
                    target: "aether_substrate::chassis",
                    kind = %kind_name,
                    "headless chassis has no handler for control kind — dropping",
                );
            }
        },
    )
}
