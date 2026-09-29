//! Native menu bars, and the id round-trip that routes a click back to the
//! window whose menu owns the item.
//!
//! muda is the platform lowering: on macOS it drives the application menu bar
//! (`init_for_nsapp`), on Windows the per-window bar (`init_for_hwnd`). Every
//! other target has no bar for it to drive, so [`apply_menu`] refuses there
//! and the caller falls back to drawing its own.
//!
//! muda's item ids are opaque strings on a process-wide event channel, while
//! [`WindowMenuActivated`](crate::WindowMenuActivated) is per window and
//! carries the caller's own `u32`. The bridge is the `"<item>:<window path>"`
//! encoding below: every id this module mints carries the pair, the item first
//! because a path holds `:` itself, and a click on anything else — a muda
//! predefined item, a menu some other library installed — parses to `None` or
//! to a window the manager does not hold, and is dropped rather than
//! mis-attributed.

use aether_data::ErasedActorPath;

/// The muda item id for one caller-numbered item in one window's menu.
/// Only the platforms that install a menu build one; the parser below is
/// unconditional because a foreign id can arrive from any menu library.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
pub(super) fn menu_item_id(window: &ErasedActorPath, item: u32) -> String {
    format!("{item}:{window}")
}

/// Recover the window and caller item number from a muda item id.
///
/// `None` for an id that is not an item number and an actor path — muda's
/// channel is process-wide, so a foreign id is expected traffic, not an error.
/// A foreign id that happens to parse names a window the manager does not
/// hold, and the manager drops it there.
pub(super) fn parse_menu_item_id(raw: &str) -> Option<(ErasedActorPath, u32)> {
    let (item, window) = raw.split_once(':')?;
    Some((ErasedActorPath::new(window).ok()?, item.parse().ok()?))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform {
    use std::cell::RefCell;

    use muda::accelerator::Accelerator;
    use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use winit::window::Window;

    use aether_data::ErasedActorPath;

    use super::menu_item_id;
    use crate::WindowMenu;

    thread_local! {
        /// The bar currently installed, kept alive for as long as the platform
        /// shows it. muda's `Menu` is `!Send` and a `NativeActor::State` is
        /// `Send`-bounded, so the live handle parks here rather than on the
        /// window manager's state. Only the winit application thread ever
        /// touches it — the desktop manager is a pumped actor, so every turn
        /// that reaches this module already runs there.
        static INSTALLED: RefCell<Option<Menu>> = const { RefCell::new(None) };
    }

    /// The platform's own application submenu, which macOS draws first in the
    /// bar and names after the application. Windows has no such convention, so
    /// the caller's menus are the whole bar there.
    ///
    /// `app_name` titles this submenu and its About / Quit items, but macOS
    /// overrides the *drawn* title with the application's own name — for an
    /// unbundled binary, the name of the executed file, which is why the fleet
    /// names that file after the application (ADR-0212,
    /// iamacoffeepot/aether#5518).
    #[cfg(target_os = "macos")]
    fn application_submenu(app_name: &str) -> Result<Submenu, String> {
        let app = Submenu::new(app_name, true);
        app.append_items(&[
            &PredefinedMenuItem::about(Some(&format!("About {app_name}")), None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(Some(&format!("Quit {app_name}"))),
        ])
        .map_err(|error| format!("could not build the application menu: {error}"))?;
        Ok(app)
    }

    fn build(app_name: &str, window: &ErasedActorPath, menus: &[WindowMenu]) -> Result<Menu, String> {
        let bar = Menu::new();

        #[cfg(target_os = "macos")]
        bar.append(&application_submenu(app_name)?)
            .map_err(|error| format!("could not install the application menu: {error}"))?;
        #[cfg(not(target_os = "macos"))]
        let _ = app_name;

        for menu in menus {
            let submenu = Submenu::new(&menu.title, true);
            for item in &menu.items {
                // An unparseable shortcut costs its accelerator, not the menu:
                // the label is what the caller mostly wanted, and refusing the
                // whole bar over one typo is a worse trade than a plain item.
                let accelerator = match item.shortcut.trim() {
                    "" => None,
                    shortcut => match shortcut.parse::<Accelerator>() {
                        Ok(accelerator) => Some(accelerator),
                        Err(error) => {
                            tracing::warn!(
                                target: "aether_window::menu",
                                shortcut,
                                %error,
                                "menu item shortcut is not an accelerator; rendering the item without one",
                            );
                            None
                        }
                    },
                };

                submenu
                    .append(&MenuItem::with_id(menu_item_id(window, item.id), &item.label, item.enabled, accelerator))
                    .map_err(|error| format!("could not append menu item `{}`: {error}", item.label))?;
                if item.separator_after {
                    submenu
                        .append(&PredefinedMenuItem::separator())
                        .map_err(|error| format!("could not append a separator after `{}`: {error}", item.label))?;
                }
            }
            bar.append(&submenu).map_err(|error| format!("could not append menu `{}`: {error}", menu.title))?;
        }

        Ok(bar)
    }

    pub(in crate::runtime::desktop) fn apply_menu(
        app_name: &str,
        native: &Window,
        window: &ErasedActorPath,
        menus: &[WindowMenu],
    ) -> Result<(), String> {
        let bar = build(app_name, window, menus)?;

        #[cfg(target_os = "macos")]
        {
            // macOS has one menu bar per *application*, so the last window to
            // install one owns it. The window still rides in every item id, so
            // its activations reach that window's subscribers.
            let _ = native;
            bar.init_for_nsapp();
        }
        #[cfg(target_os = "windows")]
        {
            use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

            let hwnd = match native
                .window_handle()
                .map_err(|error| format!("could not obtain the window handle: {error}"))?
                .as_raw()
            {
                RawWindowHandle::Win32(handle) => handle.hwnd.get(),
                other => return Err(format!("expected a Win32 window handle, got {other:?}")),
            };

            INSTALLED.with_borrow(|installed| {
                if let Some(previous) = installed {
                    // SAFETY: `hwnd` came from the live window we were handed.
                    unsafe { previous.remove_for_hwnd(hwnd) }
                        .unwrap_or_else(|error| tracing::warn!(target: "aether_window::menu", %error, "stale menu"));
                }
            });
            // SAFETY: same live window handle.
            unsafe { bar.init_for_hwnd(hwnd) }.map_err(|error| format!("could not install the menu bar: {error}"))?;
        }

        // The platform now owns the native bar; this handle keeps muda's side
        // of it alive until the next install replaces it.
        INSTALLED.with_borrow_mut(|installed| *installed = Some(bar));
        Ok(())
    }

    /// Take every menu activation muda has queued since the last turn, as raw
    /// item ids for [`super::parse_menu_item_id`].
    pub(in crate::runtime::desktop) fn drain_menu_activations() -> Vec<String> {
        MenuEvent::receiver().try_iter().map(|event| event.id.0).collect()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use aether_data::ErasedActorPath;
    use winit::window::Window;

    use crate::WindowMenu;

    pub(in crate::runtime::desktop) fn apply_menu(
        _app_name: &str,
        _native: &Window,
        _window: &ErasedActorPath,
        _menus: &[WindowMenu],
    ) -> Result<(), String> {
        Err("no native menu bar on this platform — muda drives macOS and Windows only; draw an in-window menu bar \
             instead"
            .to_owned())
    }

    pub(in crate::runtime::desktop) fn drain_menu_activations() -> Vec<String> {
        Vec::new()
    }
}

pub(super) use platform::{apply_menu, drain_menu_activations};

#[cfg(test)]
mod tests {
    use aether_data::LoadName;

    use super::{menu_item_id, parse_menu_item_id};

    /// muda's event channel is process-wide and its ids are opaque strings, so
    /// this encoding is the only thing that tells a click on *our* item from a
    /// click on a predefined item, a menu another library installed, or an
    /// item minted for a different window. The window path holds `:` itself,
    /// so the item leads and the split is at the first `:`; a parse that split
    /// anywhere else would hand back a truncated path or no item at all.
    #[test]
    fn only_ids_this_module_minted_resolve_to_a_window_and_item() {
        for (name, item) in [("main", 0), ("tools", u32::MAX), ("palette", 12)] {
            let window = crate::window_path(&LoadName::new(name).expect("fixture window name"));
            assert_eq!(parse_menu_item_id(&menu_item_id(&window, item)), Some((window, item)));
        }

        for foreign in ["", "quit", "3", "3:", ":aether.window", "-3:aether.window", "x:aether.window", "3:two words"] {
            assert_eq!(parse_menu_item_id(foreign), None, "foreign menu id {foreign:?} must not resolve");
        }
    }
}
