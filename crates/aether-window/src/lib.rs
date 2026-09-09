//! `aether.window` actor identity, wire vocabulary, and sender facade.
//!
//! [`WindowCapability`] is the neutral alias callers address; every chassis
//! installs a runtime that claims the same `aether.window` mailbox. The
//! default is the fail-fast headless one, `desktop` swaps in
//! [`DesktopWindowCapability`] over a real winit window, and `synthetic` swaps
//! in [`SyntheticWindowCapability`], the deterministic in-memory manager that
//! harness tests drive. One named window is addressed as a [`WindowInstance`]
//! child of the manager.

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
use aether_actor::{MailboxForward, Publishes, actor};
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowSize,
};
/// The one declaration of the `aether.window` mailbox name.
///
/// Every implementation identity — headless, `desktop`, `synthetic` — reads
/// its `NAMESPACE` from this const rather than repeating the literal, so the
/// shared mailbox has a single naming authority (iamacoffeepot/aether#5720).
/// Each of those three reads is `#[cfg]`-gated on a runtime feature, which is
/// why a grep of this file alone makes the const look unreferenced.
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

// The kinds the `aether.window` mailbox fans out to its selector-keyed
// subscriber set, one `Publishes` impl each — the compile-time gate on
// `WindowManagerMailboxExt::subscribe`. Device events and window
// lifecycle both travel that one machinery, so both are listed.
//
// These sit on the neutral `WindowCapability` identity rather than on
// a runtime, because the published vocabulary belongs to the mailbox:
// desktop, synthetic, and headless all claim `aether.window`, and a
// subscriber addresses the identity without knowing which is installed.
// A runtime with no window peripheral emits none of them — that is a
// deployment fact the marker cannot and should not encode.
//
// The request/reply vocabulary (`ListWindows`, `SetWindowTitle`, their
// results) is absent: it is mail *to* the cap, not a broadcast from it.
impl Publishes<Key> for WindowCapability {}
impl Publishes<KeyRelease> for WindowCapability {}
impl Publishes<MouseMove> for WindowCapability {}
impl Publishes<MouseButton> for WindowCapability {}
impl Publishes<MouseButtonRelease> for WindowCapability {}
impl Publishes<MouseWheel> for WindowCapability {}
impl Publishes<WindowSize> for WindowCapability {}
impl Publishes<TextInput> for WindowCapability {}
impl Publishes<ImePreedit> for WindowCapability {}
impl Publishes<Modifiers> for WindowCapability {}
impl Publishes<WindowOpened> for WindowCapability {}
impl Publishes<WindowClosed> for WindowCapability {}
impl Publishes<WindowMenuActivated> for WindowCapability {}

/// Sender-side convenience methods for manager-owned window operations.
pub trait WindowManagerMailboxExt: MailboxForward<WindowCapability> + Sized {
    /// Request every live window in ascending id order.
    fn list(&self) {
        self.forward(&ListWindows);
    }

    /// Request creation of a new window.
    fn create(&self, spec: WindowSpec) {
        self.forward(&CreateWindow { spec });
    }

    /// Subscribe the calling actor to kind `K` for `selector`.
    ///
    /// `K` is gated on `WindowCapability: Publishes<K>`, so a kind this
    /// cap never emits — a lifecycle stage, say — is a compile error
    /// naming the capability that does publish it.
    fn subscribe<K: Kind>(&self, selector: WindowSelector)
    where
        WindowCapability: Publishes<K>,
    {
        self.forward(&SubscribeWindowSelf { selector, kind: K::ID });
    }

    /// Subscribe an explicit mailbox to kind `K` for `selector`.
    fn subscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId)
    where
        WindowCapability: Publishes<K>,
    {
        self.forward(&SubscribeWindow { selector, kind: K::ID, mailbox });
    }

    /// Remove the calling actor's kind-`K` subscription for `selector`.
    fn unsubscribe<K: Kind>(&self, selector: WindowSelector)
    where
        WindowCapability: Publishes<K>,
    {
        self.forward(&UnsubscribeWindowSelf { selector, kind: K::ID });
    }

    /// Remove an explicit mailbox's kind-`K` subscription for `selector`.
    fn unsubscribe_for<K: Kind>(&self, selector: WindowSelector, mailbox: MailboxId)
    where
        WindowCapability: Publishes<K>,
    {
        self.forward(&UnsubscribeWindow { selector, kind: K::ID, mailbox });
    }

    /// Remove an explicit mailbox from every window-event subscription.
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.forward(&UnsubscribeAllWindows { mailbox });
    }
}

impl<T: MailboxForward<WindowCapability>> WindowManagerMailboxExt for T {}

/// Sender-side convenience methods for one resolved window endpoint.
pub trait WindowMailboxExt: MailboxForward<WindowInstance> + Sized {
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

    /// Install this window's native menu bar.
    fn set_menu(&self, menus: Vec<WindowMenu>) {
        self.forward(&SetWindowMenu { menus });
    }

    /// Set this window's pointer shape.
    fn set_cursor(&self, icon: CursorIcon) {
        self.forward(&SetWindowCursor { icon });
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

impl<T: MailboxForward<WindowInstance>> WindowMailboxExt for T {}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
fn validate_window_name(name: &str) -> Result<(), String> {
    validate_namespace_segment(name).map_err(|reason| format!("invalid window name `{name}`: {reason:?}"))
}

#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "desktop")]
pub use runtime::desktop::{
    DesktopWindowApplication, DesktopWindowIntegration, DesktopWindowParams, DesktopWindowUserEvent, WindowHostAction,
    WindowHostEffect, resolve_fullscreen, set_application_name,
};

#[cfg(feature = "synthetic")]
pub use kinds::InjectWindowEvent;

#[cfg(test)]
mod tests {
    use super::{
        CloseWindow, FocusWindow, ListWindows, RequestWindowRedraw, SetWindowCursor, SetWindowMenu, SetWindowMode,
        SetWindowTitle, WindowCapability, WindowInstance, WindowMailboxExt, WindowManagerMailboxExt,
    };
    use aether_actor::{Addressable, HandlesKind, WasmActorMailbox, WasmActorMailboxWithContext};
    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    use aether_substrate::actor::native::{NativeActorMailbox, NativeActorMailboxWithContext};

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
        assert_facade::<NativeActorMailbox<'static, WindowInstance>>();
        assert_facade::<NativeActorMailboxWithContext<'static, 'static, WindowInstance, ListWindows>>();
        assert_manager_facade::<NativeActorMailbox<'static, WindowCapability>>();
        assert_manager_facade::<NativeActorMailboxWithContext<'static, 'static, WindowCapability, ListWindows>>();
    }

    #[test]
    fn neutral_window_instance_has_the_exact_control_handler_facts() {
        assert_handles::<CloseWindow>();
        assert_handles::<SetWindowMode>();
        assert_handles::<SetWindowTitle>();
        assert_handles::<SetWindowMenu>();
        assert_handles::<SetWindowCursor>();
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
