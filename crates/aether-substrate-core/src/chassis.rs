//! The ADR-0035 `Chassis` trait: the one seam between core and
//! whichever peripheral layer is hosting it. Desktop implements it
//! over a winit `EventLoopProxy`; a future headless chassis returns
//! `Unsupported` errors inline for GPU / window operations and drives
//! ticks from a std timer; ADR-0034 Phase 1's hub chassis will
//! implement it over a TCP listener + MCP surface with the same
//! unsupported-return convention for inapplicable operations.
//!
//! Phase 1 pulls together three previously-separate trait boundaries
//! (`CaptureWaker`, `PlatformInfoNotifier`, `WindowModeNotifier`) —
//! they always travelled together and were always implemented by the
//! same proxy wrapper. Collapsing them means one `Arc<dyn Chassis>`
//! handle on `ControlPlane` and `CaptureQueue` instead of three
//! parallel `Arc`s, and the eventual Phase 3 headless chassis only
//! needs to implement one trait.
//!
//! Reply semantics: the chassis owns replies for the operations it
//! handles. `request_platform_info` and `request_set_window_mode`
//! hand control back to the chassis along with the originating
//! `SessionToken`; the chassis runs its work (usually async on the
//! event loop thread) and replies via the `HubOutbound` it was
//! constructed with. Core never blocks waiting for the result.

use std::sync::Arc;

use aether_hub_protocol::SessionToken;
use aether_kinds::WindowMode;

/// The peripheral surface core delegates to. See module docs.
pub trait Chassis: Send + Sync + 'static {
    /// Wake the chassis because a capture request was just enqueued
    /// on the shared `CaptureQueue`. Desktop implementations poke the
    /// event loop proxy so the next `RedrawRequested` picks the
    /// capture up even when the window is occluded. Headless / hub
    /// chassis no-op (captures aren't supported; the request handler
    /// already replied with an error before reaching here).
    fn wake_for_capture(&self);

    /// Snapshot the platform state and reply to `sender`. Desktop
    /// chassis reads winit monitor data + the cached GPU adapter
    /// info; headless replies `PlatformInfoResult::Err { "headless
    /// chassis has no window/GPU" }` or similar.
    fn request_platform_info(&self, sender: SessionToken);

    /// Apply a window mode change and reply to `sender`. Desktop
    /// chassis resolves fullscreen modes against the window's
    /// current monitor and applies via winit; headless / hub
    /// chassis reply with an unsupported error.
    fn request_set_window_mode(
        &self,
        sender: SessionToken,
        mode: WindowMode,
        width: Option<u32>,
        height: Option<u32>,
    );
}

/// No-op chassis for tests and any context without a real peripheral
/// layer. Every method is a silent drop; there is no reply. The
/// control-plane handlers themselves still run and can still
/// validate payloads, surface errors, etc. — this just means nothing
/// listens for the follow-up work.
pub struct NoopChassis;

impl Chassis for NoopChassis {
    fn wake_for_capture(&self) {}
    fn request_platform_info(&self, _sender: SessionToken) {}
    fn request_set_window_mode(
        &self,
        _sender: SessionToken,
        _mode: WindowMode,
        _width: Option<u32>,
        _height: Option<u32>,
    ) {
    }
}

/// Convenience constructor for `Arc<dyn Chassis>` pointing at the
/// no-op. Saves tests and boot paths from spelling the full cast.
pub fn noop_chassis() -> Arc<dyn Chassis> {
    Arc::new(NoopChassis)
}
