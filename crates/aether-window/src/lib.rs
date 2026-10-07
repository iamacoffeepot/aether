//! `aether.window` actor identity and wire vocabulary.
//!
//! [`WindowCapability`] is the one `aether.window` manager identity. Its
//! runtime runs one of two backends, each compiled in by its feature and
//! chosen at boot by `WindowParams`: `desktop`, a real winit window manager
//! the desktop chassis pumps on its application thread, and `synthetic`, the
//! deterministic in-memory manager harness tests drive. A chassis with no
//! window peripheral composes no window actor. One named window is addressed
//! as a [`WindowInstance`] child of the manager.

// Handler methods take decoded request payloads by value as part of the
// actor dispatch ABI.
#![allow(clippy::needless_pass_by_value)]

// `runtime` is the `#[actor]` runtime gate, and a runtime runs a backend:
// with neither compiled in, `WindowParams` would have no variant to boot.
#[cfg(all(feature = "runtime", not(any(feature = "desktop", feature = "synthetic"))))]
compile_error!(
    "aether-window's `runtime` feature needs a window backend: enable `desktop` (winit) or `synthetic` (in-memory)"
);

/// The kinds the `aether.window` mailbox publishes to its selector-keyed
/// subscribers, each beside the field its typed subscriber set is stored in:
/// the one list the `Publishes` impls, the [`WindowSubscription`] variants,
/// and the runtime's typed sets and kind dispatch are all written from, so a
/// new published kind is added here once.
///
/// `$emit` is a macro taking the list as `$($kind:ident $field:ident),+`.
/// Device events and window lifecycle travel the same machinery, so both are
/// listed. The request/reply vocabulary (`ListWindows`, `SetWindowTitle`,
/// their results) is absent: it is mail *to* the cap, not a broadcast from
/// it.
macro_rules! published_window_kinds {
    ($emit:ident) => {
        $emit! {
            Key key,
            KeyRelease key_release,
            MouseMove mouse_move,
            MouseButton mouse_button,
            MouseButtonRelease mouse_button_release,
            MouseWheel mouse_wheel,
            WindowSize window_size,
            TextInput text_input,
            ImePreedit ime_preedit,
            Modifiers modifiers,
            WindowOpened window_opened,
            WindowClosed window_closed,
            WindowFocus window_focus,
            WindowMenuActivated window_menu_activated,
        }
    };
}

pub mod kinds;

pub use aether_kinds::WindowMode;
// The forwarding command, its reply, and the command vocabulary they carry
// arrive through the glob: the manager identity's always-on `#[actor]` markers
// declare all three, so they cannot ride a runtime gate. `RetireWindow` is
// named by the endpoint's always-on markers too, but it is a `pub` type in a
// private module, because a handled kind must be `pub` (ADR-0231 §10), and
// its `pub(crate)` re-export keeps it out of the glob.
pub(crate) use kinds::RetireWindow;
pub use kinds::*;

use aether_actor::{ActorPath, Publisher, Publishes, actor};
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowSize,
};
/// The one declaration of the `aether.window` mailbox name, which
/// [`WindowCapability`]'s runtime reads as its `NAMESPACE`
/// (iamacoffeepot/aether#5720).
const WINDOW_NAMESPACE: &str = "aether.window";

/// Shared logical namespace for named window child identities.
pub const WINDOW_INSTANCE_NAMESPACE: &str = "aether.window.instance";

/// Stable name assigned to the desktop composer's initial window.
pub const INITIAL_WINDOW_NAME: &str = "main";

/// The `aether.window` window manager.
///
/// One identity whatever backs it: the runtime picks its backend from
/// `WindowParams` at boot — the winit desktop manager under `desktop`, the
/// deterministic in-memory manager harness tests drive under `synthetic`. The
/// identity and its markers compile always-on, so a marker-only wasm guest
/// declares `depends(WindowCapability)` and mails it without the runtime.
#[actor(singleton, root)]
pub struct WindowCapability;

/// One named window endpoint, a child of [`WindowCapability`]. Every command
/// it receives is forwarded to the manager, which applies it at the native
/// (or synthetic) window and answers the endpoint's held reply.
#[actor(instanced, child_of(WindowCapability), depends(WindowCapability), runtime::instance)]
pub struct WindowInstance;

/// Writes one `Publishes` impl per published kind — the compile-time gate on
/// the flat `ctx.subscribe::<WindowCapability, K>()` verb.
///
/// They sit on the `WindowCapability` identity rather than on a backend,
/// because the published vocabulary belongs to the mailbox: a subscriber
/// addresses the identity without knowing whether the desktop or the synthetic
/// backend runs behind it.
macro_rules! publishes {
    ($($kind:ident $field:ident),+ $(,)?) => {
        $(impl Publishes<$kind> for WindowCapability {})+
    };
}

published_window_kinds!(publishes);

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

/// The canonical actor path of the window named `name`:
/// `aether.window/aether.window.instance:<name>`, written from the
/// [`WindowCapability`] and [`WindowInstance`] types. It is the identity every
/// window-originated event, `aether.window.list`, and `capture_frame` carry.
///
/// # Panics
///
/// Never: the path is two steps of valid segments, under the depth and byte
/// caps.
#[must_use]
pub fn window_path(name: &LoadName) -> ErasedActorPath {
    ActorPath::<WindowInstance>::child(&ActorPath::<WindowCapability>::root(), name)
        .expect("a window path is two valid steps, under both caps")
        .as_erased()
        .clone()
}

/// The validated load name of a window spec's `name`.
#[cfg(feature = "runtime")]
fn window_name(name: &str) -> Result<LoadName, String> {
    LoadName::new(name).map_err(|error| format!("invalid window name `{name}`: {error}"))
}

#[cfg(feature = "runtime")]
mod runtime;

#[cfg(feature = "desktop")]
pub use runtime::desktop::{
    DesktopWindowApplication, DesktopWindowBoot, DesktopWindowIntegration, DesktopWindowSlot, DesktopWindowUserEvent,
    WindowHostAction, WindowHostEffect, resolve_fullscreen, set_application_name,
};

#[cfg(feature = "runtime")]
pub use runtime::{WindowCapabilityState, WindowParams};

#[cfg(feature = "synthetic")]
pub use kinds::InjectWindowEvent;

#[cfg(test)]
mod tests {
    use super::{
        CloseWindow, FocusWindow, RequestWindowRedraw, SetWindowCursor, SetWindowMenu, SetWindowMode,
        SetWindowPresentation, SetWindowTitle, WindowCapability, WindowInstance,
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
        assert_handles::<SetWindowPresentation>();
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
        use aether_substrate::testing::registered_ref;

        let typed = WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "main");
        let canonical = "aether.window/aether.window.instance:main";
        let registry = Registry::new();
        registered_ref(&registry, "aether.window", noop_handler());
        let live = registered_ref(&registry, canonical, noop_handler());
        assert_eq!(live.id(), typed, "the fixture stands the route at the typed resolver's position");

        // Tripwire: `window_path` writes the window's path from actor types;
        // its text must be the name the registry gives the spawned child, or
        // every event, list row, and capture names a window nothing resolves.
        let written = super::window_path(&aether_data::LoadName::new("main").expect("a valid window name"));
        assert_eq!(written.as_str(), canonical);
        assert_eq!(registry.resolve_address(&written).expect("the written path resolves").mailbox_id, typed);

        for address in [canonical, "aether.window/:main"] {
            let address = aether_data::ErasedActorPath::new(address).expect("fixture is a well-formed actor path");
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
