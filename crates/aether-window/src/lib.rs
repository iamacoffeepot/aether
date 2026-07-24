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

use aether_actor::{WasmActorMailbox, actor};
use aether_data::{Kind, MailboxId};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_kinds::MonitorNotice;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeActorMailbox;

const WINDOW_NAMESPACE: &str = "aether.window";

/// Fail-fast headless identity for the `aether.window` actor.
///
/// This default runtime replies that no window peripheral is available.
#[actor(singleton)]
pub struct HeadlessWindowCapability;

/// Platform-neutral compatibility alias for the headless window identity.
///
/// Consumers use `ctx.actor::<WindowCapability>()` regardless of the
/// chassis-specific runtime that owns the shared mailbox namespace.
pub use HeadlessWindowCapability as WindowCapability;

/// Desktop implementation identity for the `aether.window` mailbox.
#[cfg(feature = "desktop")]
#[actor(singleton, runtime::desktop)]
pub struct DesktopWindowCapability;

/// Deterministic in-memory implementation identity for the `aether.window`
/// mailbox.
#[cfg(feature = "synthetic")]
#[actor(singleton, runtime::synthetic)]
pub struct SyntheticWindowCapability;

/// Sender-side convenience methods for the multi-window request surface.
pub trait WindowMailboxExt {
    /// Request every live window in ascending id order.
    fn list(&self);

    /// Request creation of a new window.
    fn create(&self, spec: WindowSpec);

    /// Request closure of one explicit window.
    fn close(&self, window: WindowId);

    /// Change one window's presentation mode.
    fn set_mode(&self, window: WindowId, mode: WindowMode, width: Option<u32>, height: Option<u32>);

    /// Change one window's title.
    fn set_title(&self, window: WindowId, title: &str);

    /// Bring one window to the foreground.
    fn focus(&self, window: WindowId);

    /// Ask the platform to schedule one window for redraw.
    fn request_redraw(&self, window: WindowId);

    /// Subscribe the calling actor to kind `K` for `selector`.
    fn subscribe<K: Kind>(&self, selector: WindowSelector);

    /// Subscribe an explicit mailbox to kind `K` for `selector`.
    fn subscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId);

    /// Remove the calling actor's kind-`K` subscription for `selector`.
    fn unsubscribe<K: Kind>(&self, selector: WindowSelector);

    /// Remove an explicit mailbox's kind-`K` subscription for `selector`.
    fn unsubscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId);

    /// Remove an explicit mailbox from every window-event subscription.
    fn unsubscribe_all(&self, mailbox: MailboxId);
}

impl WindowMailboxExt for WasmActorMailbox<'_, WindowCapability> {
    fn list(&self) {
        self.send(&ListWindows);
    }

    fn create(&self, spec: WindowSpec) {
        self.send(&CreateWindow { spec });
    }

    fn close(&self, window: WindowId) {
        self.send(&CloseWindow { window });
    }

    fn set_mode(&self, window: WindowId, mode: WindowMode, width: Option<u32>, height: Option<u32>) {
        self.send(&SetWindowMode { window, mode, width, height });
    }

    fn set_title(&self, window: WindowId, title: &str) {
        self.send(&SetWindowTitle { window, title: title.to_owned() });
    }

    fn focus(&self, window: WindowId) {
        self.send(&FocusWindow { window });
    }

    fn request_redraw(&self, window: WindowId) {
        self.send(&RequestWindowRedraw { window });
    }

    fn subscribe<K: Kind>(&self, selector: WindowSelector) {
        self.send(&SubscribeWindowSelf { selector, kind: K::ID });
    }

    fn subscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        self.send(&SubscribeWindow { selector, kind: K::ID, mailbox });
    }

    fn unsubscribe<K: Kind>(&self, selector: WindowSelector) {
        self.send(&UnsubscribeWindowSelf { selector, kind: K::ID });
    }

    fn unsubscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        self.send(&UnsubscribeWindow { selector, kind: K::ID, mailbox });
    }

    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAllWindows { mailbox });
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl WindowMailboxExt for NativeActorMailbox<'_, WindowCapability> {
    fn list(&self) {
        self.send(&ListWindows);
    }

    fn create(&self, spec: WindowSpec) {
        self.send(&CreateWindow { spec });
    }

    fn close(&self, window: WindowId) {
        self.send(&CloseWindow { window });
    }

    fn set_mode(&self, window: WindowId, mode: WindowMode, width: Option<u32>, height: Option<u32>) {
        self.send(&SetWindowMode { window, mode, width, height });
    }

    fn set_title(&self, window: WindowId, title: &str) {
        self.send(&SetWindowTitle { window, title: title.to_owned() });
    }

    fn focus(&self, window: WindowId) {
        self.send(&FocusWindow { window });
    }

    fn request_redraw(&self, window: WindowId) {
        self.send(&RequestWindowRedraw { window });
    }

    fn subscribe<K: Kind>(&self, selector: WindowSelector) {
        self.send(&SubscribeWindowSelf { selector, kind: K::ID });
    }

    fn subscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        self.send(&SubscribeWindow { selector, kind: K::ID, mailbox });
    }

    fn unsubscribe<K: Kind>(&self, selector: WindowSelector) {
        self.send(&UnsubscribeWindowSelf { selector, kind: K::ID });
    }

    fn unsubscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        self.send(&UnsubscribeWindow { selector, kind: K::ID, mailbox });
    }

    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAllWindows { mailbox });
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
    use super::{WasmActorMailbox, WindowCapability, WindowMailboxExt};

    fn assert_facade<T: WindowMailboxExt>() {}

    #[test]
    fn neutral_facade_is_available_to_wasm_senders() {
        assert_facade::<WasmActorMailbox<'static, WindowCapability>>();
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn neutral_facade_is_available_to_native_senders() {
        assert_facade::<super::NativeActorMailbox<'static, WindowCapability>>();
    }
}
