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
#[cfg(feature = "runtime")]
pub(crate) use kinds::WindowCommand;
pub use kinds::*;
pub(crate) use kinds::{ApplyWindowCommand, ApplyWindowCommandResult};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub(crate) use kinds::{RetireWindow, WindowForwardContext};

#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_actor::validate_namespace_segment;
use aether_actor::{HandlesKind, WasmActorMailbox, WasmActorMailboxWithContext, actor};
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

/// Desktop runtime identity for one named window endpoint.
#[cfg(feature = "desktop")]
#[actor(instanced, child_of(DesktopWindowCapability), runtime::desktop::instance)]
pub struct DesktopWindowInstance;

/// Deterministic in-memory implementation identity for the `aether.window`
/// mailbox.
#[cfg(feature = "synthetic")]
#[actor(singleton, root, runtime::synthetic)]
pub struct SyntheticWindowCapability;

/// Deterministic in-memory runtime identity for one named window endpoint.
#[cfg(feature = "synthetic")]
#[actor(instanced, child_of(SyntheticWindowCapability), runtime::synthetic::instance)]
pub struct SyntheticWindowInstance;

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

trait WindowMailboxForward {
    fn forward<K>(&self, payload: &K)
    where
        WindowInstance: HandlesKind<K>,
        K: Kind;
}

/// Sender-side convenience methods for one resolved window endpoint.
#[allow(private_bounds)]
pub trait WindowMailboxExt: WindowMailboxForward + Sized {
    /// Request closure of this window.
    fn close(&self) {
        self.forward(&CloseWindow);
    }

    /// Change this window's presentation mode.
    fn set_mode(&self, mode: WindowMode, width: Option<u32>, height: Option<u32>) {
        self.forward(&SetWindowMode { mode, width, height });
    }

    /// Change this window's title.
    fn set_title(&self, title: &str) {
        self.forward(&SetWindowTitle { title: title.to_owned() });
    }

    /// Bring this window to the foreground.
    fn focus(&self) {
        self.forward(&FocusWindow);
    }

    /// Ask the platform to schedule this window for redraw.
    fn request_redraw(&self) {
        self.forward(&RequestWindowRedraw);
    }
}

impl<T: WindowMailboxForward> WindowMailboxExt for T {}

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

impl WindowMailboxForward for WasmActorMailbox<'_, WindowInstance> {
    fn forward<K>(&self, payload: &K)
    where
        WindowInstance: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

impl<C: Kind> WindowMailboxForward for WasmActorMailboxWithContext<'_, '_, WindowInstance, C> {
    fn forward<K>(&self, payload: &K)
    where
        WindowInstance: HandlesKind<K>,
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

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl WindowMailboxForward for NativeActorMailbox<'_, WindowInstance> {
    fn forward<K>(&self, payload: &K)
    where
        WindowInstance: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl<C: Kind> WindowMailboxForward for NativeActorMailboxWithContext<'_, '_, WindowInstance, C> {
    fn forward<K>(&self, payload: &K)
    where
        WindowInstance: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
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
        assert_facade::<WasmActorMailbox<'static, WindowInstance>>();
        assert_facade::<WasmActorMailboxWithContext<'static, 'static, WindowInstance, ListWindows>>();
        assert_manager_facade::<WasmActorMailbox<'static, WindowCapability>>();
        assert_manager_facade::<WasmActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>();
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn neutral_facade_is_available_to_native_senders() {
        assert_facade::<super::NativeActorMailbox<'static, WindowInstance>>();
        assert_facade::<super::NativeActorMailboxWithContext<'static, 'static, WindowInstance, ListWindows>>();
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
        use std::collections::BTreeSet;

        use aether_actor::wasm::inline::Registry as InlineRegistry;
        use aether_data::name_inventory::{ParamKind, child_entries, template_entries};
        use aether_substrate::Registry;
        use aether_substrate::mail::registry::noop_handler;
        use aether_substrate::testing::boot_authority;

        let inline = InlineRegistry::new();
        let manager_id = WindowCapability::resolve(0, ());
        let manager = WasmActorMailbox::<WindowCapability>::__new(manager_id.0, 0, &inline);
        let typed = manager.resolve::<WindowInstance>("main").mailbox_id();
        let canonical = "aether.window/aether.window.instance:main";
        let registry = Registry::new();
        registry
            .try_register_inbox_with_id(&boot_authority(), typed, canonical, noop_handler())
            .expect("register canonical live window mailbox");

        for address in [canonical, "aether.window://main", "aether.window://aether.window.instance:main"] {
            let resolved = registry.resolve_address(address).expect("resolve live window address");
            assert_eq!(resolved.mailbox_id, typed);
            assert_eq!(resolved.canonical_path, canonical);
        }
        assert_eq!(registry.mailbox_name(typed).as_deref(), Some(canonical));
        assert!(registry.list_mailbox_descriptors().iter().all(|descriptor| !descriptor.name.contains("://")));
        let template_facts = template_entries()
            .filter(|entry| entry.prefix == super::WINDOW_INSTANCE_NAMESPACE)
            .map(|entry| (entry.domain, entry.template, matches!(&entry.param, ParamKind::Dynamic)))
            .collect::<BTreeSet<_>>();
        assert_eq!(template_facts, BTreeSet::from([(aether_data::MAILBOX_DOMAIN, ":{subname}", true)]),);

        let child_facts = child_entries()
            .filter(|entry| entry.child_namespace == WindowInstance::NAMESPACE)
            .map(|entry| (entry.parent_namespace, entry.child_namespace))
            .collect::<BTreeSet<_>>();
        assert_eq!(child_facts, BTreeSet::from([(WindowCapability::NAMESPACE, WindowInstance::NAMESPACE)]));
    }
}
