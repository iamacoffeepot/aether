# ADR-0212: Native window chrome

- **Status:** Provisional
- **Date:** 2026-09-02

## Context

A desktop engine window has more chrome than a title and a size. The `aether.window` cap (ADR-0164's application-scoped manager, ADR-0167's addressable named children) owned four per-window controls — `set_mode`, `set_title`, `focus`, `request_redraw` — and one root-level create/list/subscribe surface. Everything a platform draws *around* the client area was missing, and the gaps showed up as four separate complaints from the first real application built on the engine:

- **No menu bar.** An application expecting a File / Edit / View / Help bar had nowhere to ask for one, so the only option was drawing a menu strip inside the client area — which on macOS reads as a mistake, because the platform has a bar and the application is visibly not using it.
- **No cursor control.** A resizable splitter or a draggable handle could not tell the pointer what the gesture does. Every hover looked identical, so the affordance was invisible until the drag started.
- **The wrong application name.** macOS draws the first menu-bar item from the running application's name, and for a binary outside an `.app` bundle that is `NSProcessInfo.processName` — the basename of the executed file. `aether-fleet`'s spawn path materializes the content-addressed binary as `<fleet store>/<engine id>/substrate` before fork+exec (`prepare_fork` in `crates/aether-fleet/src/server/runtime.rs`), so every hub-spawned desktop engine announced itself as **`substrate`**. Nothing chose that name; it is the storage layout leaking into the user interface.
- **No key repeat.** The desktop input translation dropped every winit `KeyEvent` with `repeat: true`, so a held key produced exactly one `aether.key`. Holding Backspace in a text field deleted one character.

The four share a shape: each is a fact about the *platform's* presentation of a window or of the process, and each was either absent from the cap's vocabulary or silently discarded on the way through it.

## Decision

**Where the platform has native chrome, application commands go to the platform.** `aether.window.set_menu { menus }` installs a real menu bar and `aether.window.menu_activated { window, id }` publishes a chosen item back through the cap's existing selector-aware subscription family — the same route `aether.key` takes. The lowering is [muda](https://crates.io/crates/muda): the macOS application menu bar (`Menu::init_for_nsapp`) and the Windows per-window bar (`Menu::init_for_hwnd`).

**Where it does not, the caller draws its own, and the cap says so rather than pretending.** On every other target — and on the headless chassis, at the root and at a window endpoint alike — `set_menu` replies `Err` naming the situation, the fail-fast contract every other window op already follows. An in-window menu bar in the widget kit is the intended fallback for those chassis; it does not exist yet, and this ADR does not build it. What this ADR fixes is that an application on a platform *with* a bar no longer has to draw one.

**Cursor icon and application name are window-cap facts.** `aether.window.set_cursor { icon }` is per-window like `set_title`, over a twelve-value `CursorIcon` vocabulary that names the *gesture* (`ResizeHorizontal`, `ResizeDiagonalRising`, `Grab`, …) rather than a platform shape, lowered onto winit's `CursorIcon`. The application name is per-chassis: `AETHER_APP_NAME` / `--app-name` (default `"Aether"`) joins `WindowConfig` through the ADR-0090 derive-`Config` path, and the desktop chassis threads it to the macOS process name before the event loop opens, to the muda application submenu's title and its Quit item, and to the window title when `AETHER_WINDOW_TITLE` is unset.

**A key repeat is an ordinary press.** The desktop translation publishes `aether.key` for every winit press including repeats, and no `aether.key_release` between them.

## Consequences

### Positive

- An application on macOS or Windows gets the bar its users expect, with the platform's own rendering, accelerator display, and keyboard navigation — none of which an in-window strip reproduces.
- Hover affordances become expressible: a splitter says "drag me left and right" before the drag starts.
- A shipped product is named once and the name reaches every surface the platform shows it in. The `substrate` leak is closed at its cause rather than by renaming the fleet's scratch file.
- Holding a key repeats. Text editing behaves the way every other application on the machine does.
- `set_menu` / `set_cursor` join the existing per-window command plumbing unchanged — endpoint forwarding, root-routes-to-the-sole-window, headless refusal — so they inherit its correlation, settlement, and refusal semantics rather than inventing parallel ones.

### Negative

- **Platform coverage is muda's, and it is not universal.** macOS and Windows have bars; Linux does not have one muda can drive without gtk, which this workspace does not build. The dependency is scoped to the two platforms that work and `set_menu` refuses elsewhere — honest, but it means a Linux desktop chassis has no menu until the widget-kit fallback exists.
- **macOS has one menu bar per application, not per window.** `set_menu` is addressed per window because Windows is per-window and the rest of the command family is; on macOS the last window to install a bar owns it. Each item id still carries its installing window, so activations reach that window's subscribers, but two windows cannot show different bars on macOS.
- **muda's `Menu` is `!Send` and `NativeActor::State` is `Send`-bounded**, so the live menu handle parks in a thread-local owned by the winit application thread rather than on the manager's state. Correct — the desktop manager is a pumped actor and every turn that touches it runs on that thread — but it is a second place window state lives.
- **muda delivers activations on its own process-wide channel**, not through winit's event loop, so the application drains it once per turn. A foreign id (a predefined Quit, another library's menu) parses to no window and is dropped.
- **A key repeat is now indistinguishable from a fast retap** to a consumer that only counts `aether.key`. Consumers that must tell them apart pair presses with `aether.key_release`, which repeats do not emit. The widget kit's button already suppresses repeats on its own side, so this change is invisible there.
- `shortcut` is installed as a real accelerator where muda can parse it, so on macOS the platform *consumes* the matching keystroke and fires the item instead of delivering the key. That is the native behaviour, and the component's own key handling remains the path on chassis with no bar.

### Neutral

- An unparseable `shortcut` costs that item its accelerator and logs a warning; it does not fail the menu. Refusing a whole bar over one typo is the worse trade.
- `AETHER_WINDOW_TITLE` still wins over the application name when set, so nothing that pinned a title changes.

## Alternatives considered

- **Draw the menu bar in the widget kit on every platform.** One implementation, total control, no new dependency — and wrong on macOS, where the platform bar exists and an in-window strip reads as a broken application. Kept as the fallback for chassis with no native bar, not as the default.
- **Ship `shortcut` as inert display text with no accelerator installed.** Would keep every keystroke flowing to the component, but muda's menu items have no display-only shortcut field: text and accelerator are the same thing to the platform. Installing it is what makes the platform render it.
- **Rename the fleet's materialized binary from `substrate` to the product name.** Fixes the macOS menu for fleet-spawned engines and nothing else — a directly-executed `aether-desktop` would still be named `aether-desktop`, and the name would be a property of the hub's storage layout rather than of the application.
- **A `Menu` handle on the manager's state behind a host action.** Would avoid the thread-local, at the cost of routing `set_menu` through the `WindowHostAction` / `WindowHostEffect` round trip and its deferred reply, when the apply already runs on the winit thread with the native window in hand.
