// Control-plane → event-loop notifiers for the window/platform kinds.
// Both `aether.control.platform_info` (read) and
// `aether.control.set_window_mode` (write) require touching winit
// state that only lives on the main thread, so the control plane
// hands each request off over an `EventLoopProxy<UserEvent>` backed
// by the trait impls in `main.rs`. Requests carry the whole payload
// inline — no shared queue state — since the event loop processes
// one user event at a time.

use aether_hub_protocol::SessionToken;
use aether_kinds::WindowMode;

/// Notifies the event-loop thread that an MCP session has asked for a
/// platform snapshot. The substrate's event loop impls this over an
/// `EventLoopProxy<UserEvent>`; tests pass a no-op impl.
pub trait PlatformInfoNotifier: Send + Sync {
    fn notify(&self, sender: SessionToken);
}

/// Notifies the event-loop thread that a session wants to change the
/// window mode. The event loop resolves the request, applies the
/// change, and replies via `outbound`. `width` / `height` apply only
/// for `WindowMode::Windowed`.
pub trait WindowModeNotifier: Send + Sync {
    fn request(
        &self,
        sender: SessionToken,
        mode: WindowMode,
        width: Option<u32>,
        height: Option<u32>,
    );
}

/// No-op notifier for tests and headless contexts where there is no
/// event loop to wake. The control-plane handler still runs and
/// replies with an `Err` when the snapshot can't be produced — but
/// that's the caller's concern, not this trait's.
pub struct NoopPlatformInfoNotifier;

impl PlatformInfoNotifier for NoopPlatformInfoNotifier {
    fn notify(&self, _sender: SessionToken) {}
}

/// Companion no-op for `WindowModeNotifier`.
pub struct NoopWindowModeNotifier;

impl WindowModeNotifier for NoopWindowModeNotifier {
    fn request(
        &self,
        _sender: SessionToken,
        _mode: WindowMode,
        _width: Option<u32>,
        _height: Option<u32>,
    ) {
    }
}
