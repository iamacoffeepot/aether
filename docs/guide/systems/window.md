# Window

> **Governing ADRs:** [ADR-0035](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0035-substrate-chassis-split.md)
> (the substrate/chassis split) and
> [ADR-0164](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0164-window-actor-owns-native-window-integration.md)
> (the application-scoped multi-window manager), and
> [ADR-0167](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0167-window-manager-supervises-addressable-window-actors.md)
> (named child identities; Proposed and landing incrementally).

`aether-window` is the bespoke home of window behavior. One application-scoped
actor owns every live window, translates native window events, routes the
resulting Aether kinds, and exposes window lifecycle and control over mail.
Callers use the platform-neutral `WindowCapability` manager identity, stable
window names, and explicit `WindowId` values; they never hold a native window
handle or address a desktop-, headless-, or test-specific implementation.

The desktop chassis still owns the application thread and the call to winit's
event loop. That is thread ownership, not window-domain ownership:

```text
aether-chassis-desktop
    EventLoop::run_app(...)
        └── DesktopWindowApplication       // aether-window
              ├── winit ApplicationHandler
              ├── PumpedSlot<DesktopWindowCapability>
              ├── WindowId ↔ winit WindowId maps
              └── DesktopWindowIntegration // semantic chassis seam
                    ├── attach/detach render target
                    ├── mark windows dirty
                    └── request process shutdown
```

## Why it exists

Window toolkits require their event loop and window mutations to remain on one
specific thread. Actors, render surfaces, and callers still need an ordinary
mail boundary. A pumped window actor satisfies both constraints: winit
callbacks enter its state synchronously through same-thread host ingress, while
actor requests arrive through the normal mailbox and settlement graph.

Keeping the whole native application in `aether-window` also gives
multi-window behavior one owner. The manager allocates stable engine
`WindowId`s, maps them to platform ids, retains per-window cursor/IME/focus
state, publishes events with their source id, and coordinates the matching
render target. The chassis composes this application with render, lifecycle,
and shutdown; it does not interpret raw winit events.

## The public surface

Consumers address the neutral manager identity and use
`WindowManagerMailboxExt` for list, create, and subscription operations:

```rust
use aether_kinds::{Key, WindowMode};
use aether_window::{
    WindowCapability, WindowManagerMailboxExt, WindowSelector, WindowSizeRequest,
    WindowSpec,
};

fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    let windows = ctx.actor::<WindowCapability>();

    windows.list();
    windows.create(WindowSpec {
        name: "inspector".to_owned(),
        title: "Inspector".to_owned(),
        mode: WindowMode::Windowed,
        size: Some(WindowSizeRequest { width: 960, height: 540 }),
    });
    windows.subscribe::<Key>(WindowSelector::All);
}
```

The compatibility `WindowMailboxExt` facade continues to expose the existing
id-bearing control methods while named desktop and synthetic child runtimes
are introduced in later increments.

The request/reply families are:

| Operation | Target or input | Successful reply |
|---|---|---|
| `list` | none | every live `WindowInfo`, ordered by `WindowId` |
| `create` | `WindowSpec` | the attached window's `WindowInfo` |
| `close` | `WindowId` | the window whose close began |
| `set_mode` | `WindowId`, mode, optional windowed size | resolved mode and size |
| `set_title` | `WindowId`, title | applied title |
| `focus` | `WindowId` | acknowledgement |
| `request_redraw` | `WindowId` | acknowledgement |

`WindowSpec::name` is an immutable actor instance segment: it cannot be empty,
contain whitespace or `:`, or duplicate a pending or live window name. The
mutable title remains independent of that stable name. Every targeted
operation rejects an unknown or already-closing id. Window requests are
advisory: an OS may clamp a size or decline to focus exactly as asked, so
callers should treat the reply's applied state as authoritative.
There is no implicit focused or current target. The boot window is named
`main` and is simply the first `WindowSpec` realized after winit resumes.

`WindowInfo` reports:

```rust
pub struct WindowInfo {
    pub id: WindowId,
    pub name: String,
    pub title: String,
    pub mode: WindowMode,
    pub width: u32,
    pub height: u32,
    pub focused: bool,
    pub occluded: bool,
}
```

`WindowMode::Windowed` may request a physical-pixel size.
`FullscreenBorderless` follows the current monitor.
`FullscreenExclusive { width, height, refresh_mhz }` must match a supported
video mode exactly and fails instead of silently choosing another one.

## Window-originated streams

The window actor is also the source and router for keyboard, pointer,
resize, text, IME, focus, redraw, opened, and closed events. Every per-window
kind carries a `WindowId`. A subscriber chooses one window or all current and
future windows:

```rust
fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    let windows = ctx.actor::<WindowCapability>();
    windows.subscribe::<WindowOpened>(WindowSelector::All);
    windows.subscribe::<WindowSize>(WindowSelector::All);
    windows.subscribe::<MouseMove>(WindowSelector::One(self.viewport));
}
```

`WindowSelector::All` is prospective. If the same mailbox subscribes through
both `All` and `One(id)`, recipient lookup unions the sets and sends one copy.
The common reflexive methods subscribe the sending actor. Explicit
`subscribe_for`/`unsubscribe_for` forms exist for forwarding to another local
mailbox; the manager validates and monitors that mailbox, and removes all of
its rows when it departs.

Publication uses each kind's compile-time `K::ID`. There is no registry lookup,
central input-kind list, or relay actor to update when a window event kind is
added. See [Input streams](input.md) for the event vocabulary and text/IME
semantics.

## Desktop threading

`DesktopWindowApplication<I>` implements winit's `ApplicationHandler` in the
window crate. A callback runs in this order:

```text
drain actor mail on the owning thread
    → host_turn(|state, ctx| state.window_event(...))
    → apply queued native WindowHostAction values
    → apply semantic WindowHostEffect values through I
    → pump render while settlement requires progress
```

`host_turn` does not move the actor or create another thread. It is available
only through the `!Send` pumped slot on its owner thread, is non-reentrant, and
starts fresh external-event roots for outbound mail. Native operations that
need winit's `ActiveEventLoop`, such as window creation, are returned as host
actions and applied after the actor turn.

Render receives only semantic attachment and dirty-window calls. It owns one
surface/configuration bundle per `WindowId`; the window manager owns native
window lifecycle and asks the integration to attach or detach the
corresponding render target. The native `Arc<Window>` remains same-thread host
state and never becomes a wire payload.

## Runtime variants

All manager variants claim the one shared window namespace:

- `DesktopWindowCapability` is pumped by `DesktopWindowApplication` and owns
  real winit state.
- `HeadlessWindowCapability` is the production headless runtime and fails
  every request immediately because there is no window peripheral;
  `WindowCapability` remains its neutral compatibility alias.
- `SyntheticWindowCapability`, declared with
  `#[actor(singleton, runtime::synthetic)]`, is test-only. It keeps a
  deterministic in-memory window map and the same selector-aware routing
  behavior.
- `WindowInstance` is the neutral named child identity. Its headless runtime
  has only the five typed control handlers and fails them immediately; desktop
  and synthetic managers do not spawn child actors in this increment.
- The hub installs no window actor.

The neutral `WindowCapability` and `WindowInstance` aliases are the identities
consumer code should name. Concrete manager runtime identities share one
namespace constant inside `aether-window`; variants do not repeat a namespace
literal. The default headless manager implementation is `runtime/mod.rs`, the
headless named endpoint is `runtime/instance.rs`, and the desktop and synthetic
manager implementations are keyed at `runtime/desktop/` and
`runtime/synthetic.rs`.

The initial desktop window still reads `AETHER_WINDOW_MODE` and
`AETHER_WINDOW_TITLE`. `AETHER_WINDOW_MODE` accepts `windowed`,
`windowed:WxH`, `fullscreen-borderless`, or `exclusive:WxH@HZ`; invalid input
warns and falls back to windowed mode. See [Configuration](configuration.md).

## Testing and extension

`SubstrateHarness` composes the synthetic runtime. Use typed actor operations
for controls and `HarnessOp::window_event` for an event:

```rust
let subscribe = HarnessOp::actor::<SyntheticWindowCapability>().send(
    &SubscribeWindow {
        selector: WindowSelector::One(window),
        kind: Key::ID,
        mailbox: observer,
    },
);

let press = HarnessOp::window_event(
    window,
    &Key { window, code: keycode::KEY_ENTER },
);
```

Synthetic injection is not a headless production API. The headless runtime
stays fail-fast so tests cannot accidentally turn unsupported production
behavior into an implicit mock.

To add a window-originated event, define the kind with a `WindowId`, emit it
from window state, and let selector routing publish `K::ID`. Do not add a
chassis kind cache or a generic input relay. A future non-window device such
as a gamepad or raw HID source should have its own concrete source actor.

## Where to read more

- The full ownership and threading decision —
  [ADR-0164](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0164-window-actor-owns-native-window-integration.md).
- Window-originated event routing — [Input streams](input.md).
- Pumped actors, external roots, and settlement —
  [Tracing & settlement](tracing-and-settlement.md).
- The deterministic runtime and typed injection —
  [SubstrateHarness and FleetHarness](../testing/substrateharness-and-fleetharness.md).
