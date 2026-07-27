//! Public `aether.window` actor identity, wire vocabulary, and sender facade.
//!
//! [`WindowCapability`] is the neutral alias callers use. A chassis installs
//! its concrete fail-fast headless runtime, the
//! [`DesktopWindowCapability`] implementation behind `desktop`, or the
//! [`SyntheticWindowCapability`] test implementation behind `synthetic`.
//! Every implementation claims the same `aether.window` mailbox.

// Handler methods take decoded request payloads by value as part of the
// actor dispatch ABI; the facade also consumes owned request values.
#![allow(clippy::needless_pass_by_value)]

pub mod kinds;

pub use aether_kinds::{WindowId, WindowMode};
pub use kinds::*;

use aether_actor::{HandlesKind, WasmActorMailbox, WasmActorMailboxWithContext, actor};
use aether_data::{Kind, MailboxId};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_kinds::MonitorNotice;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::{NativeActorMailbox, NativeActorMailboxWithContext};

const WINDOW_NAMESPACE: &str = "aether.window";

/// Fail-fast headless identity for the `aether.window` actor.
///
/// This default runtime replies that no window peripheral is available.
#[actor(singleton, root)]
pub struct HeadlessWindowCapability;

/// Platform-neutral compatibility alias for the headless window identity.
///
/// Consumers use `ctx.actor::<WindowCapability>()` regardless of the
/// chassis-specific runtime that owns the shared mailbox namespace.
pub use HeadlessWindowCapability as WindowCapability;

/// Desktop implementation identity for the `aether.window` mailbox.
#[cfg(feature = "desktop")]
#[actor(singleton, root, runtime::desktop)]
pub struct DesktopWindowCapability;

/// Deterministic in-memory implementation identity for the `aether.window`
/// mailbox.
#[cfg(feature = "synthetic")]
#[actor(singleton, root, runtime::synthetic)]
pub struct SyntheticWindowCapability;

trait WindowMailboxForward {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind;
}

/// Sender-side convenience methods for the multi-window request surface.
#[allow(private_bounds)]
pub trait WindowMailboxExt: WindowMailboxForward {
    /// Request every live window in ascending id order.
    fn list(&self) {
        self.forward(&ListWindows);
    }

    /// Request creation of a new window.
    fn create(&self, spec: WindowSpec) {
        self.forward(&CreateWindow { spec });
    }

    /// Request closure of one explicit window.
    fn close(&self, window: WindowId) {
        self.forward(&CloseWindow { window });
    }

    /// Change one window's presentation mode.
    fn set_mode(&self, window: WindowId, mode: WindowMode, width: Option<u32>, height: Option<u32>) {
        self.forward(&SetWindowMode { window, mode, width, height });
    }

    /// Change one window's title.
    fn set_title(&self, window: WindowId, title: &str) {
        self.forward(&SetWindowTitle { window, title: title.to_owned() });
    }

    /// Bring one window to the foreground.
    fn focus(&self, window: WindowId) {
        self.forward(&FocusWindow { window });
    }

    /// Ask the platform to schedule one window for redraw.
    fn request_redraw(&self, window: WindowId) {
        self.forward(&RequestWindowRedraw { window });
    }

    /// Subscribe the calling actor to kind `K` for `selector`.
    fn subscribe<K: Kind>(&self, selector: WindowSelector) {
        self.forward(&SubscribeWindowSelf { selector, kind: K::ID });
    }

    /// Subscribe an explicit mailbox to kind `K` for `selector`.
    fn subscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        self.forward(&SubscribeWindow { selector, kind: K::ID, mailbox });
    }

    /// Remove the calling actor's kind-`K` subscription for `selector`.
    fn unsubscribe<K: Kind>(&self, selector: WindowSelector) {
        self.forward(&UnsubscribeWindowSelf { selector, kind: K::ID });
    }

    /// Remove an explicit mailbox's kind-`K` subscription for `selector`.
    fn unsubscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        self.forward(&UnsubscribeWindow { selector, kind: K::ID, mailbox });
    }

    /// Remove an explicit mailbox from every window-event subscription.
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.forward(&UnsubscribeAllWindows { mailbox });
    }
}

impl<T: WindowMailboxForward> WindowMailboxExt for T {}

impl WindowMailboxForward for WasmActorMailbox<'_, WindowCapability> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

impl<C: Kind> WindowMailboxForward for WasmActorMailboxWithContext<'_, '_, WindowCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl WindowMailboxForward for NativeActorMailbox<'_, WindowCapability> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl<C: Kind> WindowMailboxForward for NativeActorMailboxWithContext<'_, '_, WindowCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "desktop")]
pub use runtime::desktop::{
    DesktopWindowApplication, DesktopWindowIntegration, DesktopWindowParams, DesktopWindowUserEvent, WindowHostAction,
    WindowHostEffect, resolve_fullscreen,
};

#[cfg(feature = "synthetic")]
pub use kinds::InjectWindowEvent;

#[cfg(test)]
mod tests {
    use super::{ListWindows, WasmActorMailbox, WasmActorMailboxWithContext, WindowCapability, WindowMailboxExt};

    fn assert_facade<T: WindowMailboxExt>() {}

    #[test]
    fn neutral_facade_is_available_to_wasm_senders() {
        assert_facade::<WasmActorMailbox<'static, WindowCapability>>();
        assert_facade::<WasmActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>();
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn neutral_facade_is_available_to_native_senders() {
        assert_facade::<super::NativeActorMailbox<'static, WindowCapability>>();
        assert_facade::<super::NativeActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>();
    }
}
