// Control-plane → event-loop notifier for
// `aether.control.platform_info`. Mirrors the capture path's waker
// idea but carries the `sender` inline — the whole snapshot happens
// on the event-loop thread inside `user_event`, so there's no
// separate "pending request" queue to hold state across threads.

use aether_hub_protocol::SessionToken;

/// Notifies the event-loop thread that an MCP session has asked for a
/// platform snapshot. The substrate's event loop impls this over an
/// `EventLoopProxy<UserEvent>`; tests pass a no-op impl.
pub trait PlatformInfoNotifier: Send + Sync {
    fn notify(&self, sender: SessionToken);
}

/// No-op notifier for tests and headless contexts where there is no
/// event loop to wake. The control-plane handler still runs and
/// replies with an `Err` when the snapshot can't be produced — but
/// that's the caller's concern, not this trait's.
pub struct NoopPlatformInfoNotifier;

impl PlatformInfoNotifier for NoopPlatformInfoNotifier {
    fn notify(&self, _sender: SessionToken) {}
}
