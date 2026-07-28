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

use aether_actor::{HandlesKind, WasmActorMailbox, WasmActorMailboxWithContext, actor, validate_namespace_segment};
use aether_data::{Kind, MailboxId};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_kinds::MonitorNotice;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::{NativeActorMailbox, NativeActorMailboxWithContext};

const WINDOW_NAMESPACE: &str = "aether.window";

/// Shared logical namespace for named window child identities.
pub const WINDOW_INSTANCE_NAMESPACE: &str = "aether.window.instance";

/// Stable name assigned to the desktop composer's initial window.
pub const INITIAL_WINDOW_NAME: &str = "main";

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

/// Fail-fast headless identity for one named window endpoint.
#[actor(instanced, child_of(WindowCapability), runtime::instance)]
pub struct HeadlessWindowInstance;

/// Platform-neutral identity for one named window endpoint.
pub use HeadlessWindowInstance as WindowInstance;

/// Desktop implementation identity for the `aether.window` mailbox.
#[cfg(feature = "desktop")]
#[actor(singleton, root, runtime::desktop)]
pub struct DesktopWindowCapability;

/// Deterministic in-memory implementation identity for the `aether.window`
/// mailbox.
#[cfg(feature = "synthetic")]
#[actor(singleton, root, runtime::synthetic)]
pub struct SyntheticWindowCapability;

trait WindowManagerMailboxForward {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind;
}

/// Sender-side convenience methods for manager-owned window operations.
#[allow(private_bounds)]
pub trait WindowManagerMailboxExt: WindowManagerMailboxForward + Sized {
    /// Request every live window in ascending id order.
    fn list(&self) {
        self.forward(&ListWindows);
    }

    /// Request creation of a new window.
    fn create(&self, spec: WindowSpec) {
        self.forward(&CreateWindow { spec });
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

impl<T: WindowManagerMailboxForward> WindowManagerMailboxExt for T {}

/// Compatibility facade for the existing manager-addressed window surface.
///
/// New manager-only call sites should prefer [`WindowManagerMailboxExt`].
/// The id-bearing control wrappers remain here until per-window runtimes take
/// ownership of the control transport.
#[allow(private_bounds)]
pub trait WindowMailboxExt: WindowManagerMailboxForward + Sized {
    /// Request every live window in ascending id order.
    fn list(&self) {
        WindowManagerMailboxExt::list(self);
    }

    /// Request creation of a new window.
    fn create(&self, spec: WindowSpec) {
        WindowManagerMailboxExt::create(self, spec);
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
        WindowManagerMailboxExt::subscribe::<K>(self, selector);
    }

    /// Subscribe an explicit mailbox to kind `K` for `selector`.
    fn subscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        WindowManagerMailboxExt::subscribe_for::<K>(self, selector, mailbox);
    }

    /// Remove the calling actor's kind-`K` subscription for `selector`.
    fn unsubscribe<K: Kind>(&self, selector: WindowSelector) {
        WindowManagerMailboxExt::unsubscribe::<K>(self, selector);
    }

    /// Remove an explicit mailbox's kind-`K` subscription for `selector`.
    fn unsubscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId) {
        WindowManagerMailboxExt::unsubscribe_for::<K>(self, selector, mailbox);
    }

    /// Remove an explicit mailbox from every window-event subscription.
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        WindowManagerMailboxExt::unsubscribe_all(self, mailbox);
    }
}

impl<T: WindowManagerMailboxForward> WindowMailboxExt for T {}

impl WindowManagerMailboxForward for WasmActorMailbox<'_, WindowCapability> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

impl<C: Kind> WindowManagerMailboxForward for WasmActorMailboxWithContext<'_, '_, WindowCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl WindowManagerMailboxForward for NativeActorMailbox<'_, WindowCapability> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl<C: Kind> WindowManagerMailboxForward for NativeActorMailboxWithContext<'_, '_, WindowCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        WindowCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

fn validate_window_name(name: &str) -> Result<(), String> {
    validate_namespace_segment(name).map_err(|reason| format!("invalid window name `{name}`: {reason:?}"))
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
    use super::{
        CloseWindow, FocusWindow, ListWindows, RequestWindowRedraw, SetWindowMode, SetWindowTitle, WasmActorMailbox,
        WasmActorMailboxWithContext, WindowCapability, WindowInstance, WindowMailboxExt, WindowManagerMailboxExt,
    };
    use aether_actor::{Addressable, HandlesKind};

    fn assert_facade<T: WindowMailboxExt>() {}
    fn assert_manager_facade<T: WindowManagerMailboxExt>() {}
    fn assert_handles<K>()
    where
        K: aether_data::Kind,
        WindowInstance: HandlesKind<K>,
    {
    }

    #[test]
    fn neutral_facade_is_available_to_wasm_senders() {
        assert_facade::<WasmActorMailbox<'static, WindowCapability>>();
        assert_facade::<WasmActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>();
        assert_manager_facade::<WasmActorMailbox<'static, WindowCapability>>();
        assert_manager_facade::<WasmActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>();
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn neutral_facade_is_available_to_native_senders() {
        assert_facade::<super::NativeActorMailbox<'static, WindowCapability>>();
        assert_facade::<super::NativeActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>();
        assert_manager_facade::<super::NativeActorMailbox<'static, WindowCapability>>();
        assert_manager_facade::<super::NativeActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>(
        );
    }

    #[test]
    fn neutral_window_instance_has_the_exact_control_handler_facts() {
        assert_handles::<CloseWindow>();
        assert_handles::<SetWindowMode>();
        assert_handles::<SetWindowTitle>();
        assert_handles::<FocusWindow>();
        assert_handles::<RequestWindowRedraw>();
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn typed_and_external_window_instance_addresses_resolve_to_one_live_mailbox() {
        use aether_actor::wasm::inline::Registry as InlineRegistry;
        use aether_data::name_inventory::{child_entries, name_entries, template_entries};
        use aether_substrate::Registry;
        use aether_substrate::mail::registry::noop_handler;

        let inline = InlineRegistry::new();
        let manager_id = WindowCapability::resolve(0, ());
        let manager = WasmActorMailbox::<WindowCapability>::__new(manager_id.0, 0, &inline);
        let typed = manager.resolve::<WindowInstance>("main").mailbox_id();
        let canonical = "aether.window/aether.window.instance:main";
        let registry = Registry::new();
        registry
            .try_register_inbox_with_id(typed, canonical, noop_handler())
            .expect("register canonical live window mailbox");

        for address in [canonical, "aether.window://main", "aether.window://aether.window.instance:main"] {
            assert_eq!(registry.resolve_address(address).expect("resolve live window address").mailbox_id, typed);
        }
        assert_eq!(
            template_entries()
                .filter(|entry| entry.domain == aether_data::MAILBOX_DOMAIN)
                .filter(|entry| entry.prefix == super::WINDOW_INSTANCE_NAMESPACE)
                .count(),
            1,
        );
        assert_eq!(
            child_entries()
                .filter(|entry| {
                    entry.parent_namespace == WindowCapability::NAMESPACE
                        && entry.child_namespace == WindowInstance::NAMESPACE
                })
                .count(),
            1,
        );
        assert!(name_entries().all(|entry| entry.name != canonical && !entry.name.contains("aether.window://")));
    }
}
