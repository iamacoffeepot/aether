//! `aether.window` actor identity and wire vocabulary.
//!
//! [`WindowCapability`] is the neutral alias callers address; every chassis
//! installs a runtime that claims the same `aether.window` mailbox. The
//! default is the fail-fast headless one, `desktop` swaps in
//! [`DesktopWindowCapability`] over a real winit window, and `synthetic` swaps
//! in [`SyntheticWindowCapability`], the deterministic in-memory manager that
//! harness tests drive. One named window is addressed as a [`WindowInstance`]
//! child of the manager.

// Handler methods take decoded request payloads by value as part of the
// actor dispatch ABI.
#![allow(clippy::needless_pass_by_value)]

pub mod kinds;

pub use aether_kinds::{WindowId, WindowMode};
// The forwarding command, its reply, and the command vocabulary they carry
// arrive through the glob: the manager identity's always-on `#[actor]` markers
// declare all three, so they cannot ride a runtime gate. The two below can —
// nothing outside a window-bearing runtime names them.
pub use kinds::*;
#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub(crate) use kinds::{RetireWindow, WindowForwardContext};

#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_actor::validate_namespace_segment;
use aether_actor::{Publisher, Publishes, actor};
use aether_data::Kind;
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
/// Consumers declare `depends(WindowCapability)` and send with
/// `ctx.send::<WindowCapability>(..)` regardless of the chassis-specific
/// runtime that owns the shared mailbox namespace.
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
#[actor(instanced, child_of(DesktopWindowCapability), depends(WindowCapability), runtime::desktop::instance)]
pub struct DesktopWindowInstance;

/// Deterministic in-memory implementation identity for the `aether.window`
/// mailbox.
#[cfg(feature = "synthetic")]
#[actor(singleton, root, runtime::synthetic)]
pub struct SyntheticWindowCapability;

/// Deterministic in-memory runtime identity for one named window endpoint.
#[cfg(feature = "synthetic")]
#[actor(instanced, child_of(SyntheticWindowCapability), depends(WindowCapability), runtime::synthetic::instance)]
pub struct SyntheticWindowInstance;

// The kinds the `aether.window` mailbox fans out to its selector-keyed
// subscriber set, one `Publishes` impl each — the compile-time gate on
// the flat `ctx.subscribe::<WindowCapability, K>()` verb. Device events
// and window lifecycle both travel that one machinery, so both are listed.
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

/// The flat subscribe verbs send these self-addressed requests, selecting
/// every window.
impl Publisher for WindowCapability {
    type Subscribe = SubscribeWindowSelf;
    type Unsubscribe = UnsubscribeWindowSelf;

    fn subscribe_request<K: Kind>() -> SubscribeWindowSelf
    where
        Self: Publishes<K>,
    {
        SubscribeWindowSelf { selector: WindowSelector::All, kind: K::ID }
    }

    fn unsubscribe_request<K: Kind>() -> UnsubscribeWindowSelf
    where
        Self: Publishes<K>,
    {
        UnsubscribeWindowSelf { selector: WindowSelector::All, kind: K::ID }
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
    WindowHostEffect, resolve_fullscreen, set_application_name,
};

#[cfg(feature = "synthetic")]
pub use kinds::InjectWindowEvent;

#[cfg(test)]
mod tests {
    use super::{
        CloseWindow, FocusWindow, RequestWindowRedraw, SetWindowCursor, SetWindowMenu, SetWindowMode, SetWindowTitle,
        WindowCapability, WindowInstance,
    };
    use aether_actor::{Addressable, HandlesKind};

    fn assert_handles<K>()
    where
        K: aether_data::Kind,
        WindowInstance: HandlesKind<K>,
    {
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

        use aether_data::name_inventory::{ParamKind, child_entries, template_entries};
        use aether_substrate::Registry;
        use aether_substrate::mail::registry::noop_handler;
        use aether_substrate::testing::boot_authority;

        let typed = WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "main");
        let canonical = "aether.window/aether.window.instance:main";
        let registry = Registry::new();
        registry
            .try_register_inbox_with_id(&boot_authority(), typed, canonical, noop_handler())
            .expect("register canonical live window mailbox");

        for address in [canonical, "aether.window/:main"] {
            let address = aether_data::ActorPath::new(address).expect("fixture is a well-formed actor path");
            let resolved = registry.resolve_address(&address).expect("resolve live window address");
            assert_eq!(resolved.mailbox_id, typed);
            assert_eq!(resolved.canonical_path, canonical);
        }
        assert_eq!(registry.mailbox_name(typed).as_deref(), Some(canonical));
        assert!(registry.list_mailbox_descriptors().iter().all(|descriptor| !descriptor.name.contains("/:")));
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
