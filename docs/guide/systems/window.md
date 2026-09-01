# Window

> **Governing ADRs:** [ADR-0035](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0035-substrate-chassis-split.md)
> (the substrate/chassis split) and
> [ADR-0164](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0164-window-actor-owns-native-window-integration.md)
> (the application-scoped multi-window manager), and
> [ADR-0167](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0167-window-manager-supervises-addressable-window-actors.md)
> (addressable named window children).

`aether-window` is the bespoke home of window behavior. The application-scoped
`WindowCapability` manager owns global lifecycle and event routing, while each
live named window has an addressable `WindowInstance` child for control mail.
Callers use those platform-neutral identities, stable window names, and
`WindowId` values; they never hold a native window handle or address a
desktop-, headless-, or test-specific implementation.

The desktop chassis still owns the application thread and the call to winit's
event loop. That is thread ownership, not window-domain ownership:

```text
aether-chassis-desktop
    EventLoop::run_app(...)
        └── DesktopWindowApplication       // aether-window
              ├── winit ApplicationHandler
              ├── PumpedSlot<DesktopWindowCapability>
              ├── WindowId ↔ winit WindowId maps
              ├── monitored pooled DesktopWindowInstance children
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
multi-window behavior one owner. The manager uses each child's raw mailbox
identity as its stable engine `WindowId`, maps it to a platform id, retains
per-window cursor/IME/focus state, publishes events with their source id, and
coordinates the matching render target. The chassis composes this application
with render, lifecycle, and shutdown; it does not interpret raw winit events.

## The public surface

Consumers use `WindowCapability` and `WindowManagerMailboxExt` for list,
create, and subscription operations. They resolve a named `WindowInstance`
from that typed manager identity and use `WindowMailboxExt` for id-less control:

```rust
use aether_kinds::{Key, WindowMode};
use aether_window::{
    WindowCapability, WindowInstance, WindowMailboxExt, WindowManagerMailboxExt,
    WindowSelector, WindowSizeRequest, WindowSpec,
};

fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    let windows = ctx.actor::<WindowCapability>();
    let main = windows.resolve::<WindowInstance>("main");

    windows.list();
    windows.create(WindowSpec {
        name: "inspector".to_owned(),
        title: "Inspector".to_owned(),
        mode: WindowMode::Windowed,
        size: Some(WindowSizeRequest { width: 960, height: 540 }),
    });
    windows.subscribe::<Key>(WindowSelector::All);

    main.set_title("Aether");
    main.request_redraw();
}
```

Typed Rust code should resolve through the actor identities rather than copy
the root namespace. At string-addressed boundaries such as MCP and harness
operations, the same live child may be named by either its canonical or
abbreviated recipient:

```text
aether.window/aether.window.instance:main
aether.window://main
```

The request/reply families are:

| Operation | Recipient and input | Successful reply |
|---|---|---|
| `list` | manager; no input | every live `WindowInfo`, ordered by `WindowId` |
| `create` | manager; `WindowSpec` | the attached window's `WindowInfo` |
| subscribe/unsubscribe | manager; selector, kind, and optional explicit mailbox | acknowledgement |
| `unsubscribe_all` | manager; explicit mailbox | normal no-reply settlement |
| `close` | named child; no input | acknowledgement |
| `set_mode` | named child; mode and optional windowed size | resolved mode and size |
| `set_title` | named child; title | applied title |
| `focus` | named child; no input | request acknowledgement |
| `request_redraw` | named child; no input | request acknowledgement |

`WindowSpec::name` is an immutable actor instance segment: it cannot be empty,
contain whitespace or `:`, or duplicate a pending or live window name. The
native actor tombstone also prevents reuse of a closed name during the same
chassis lifetime. The mutable title remains independent of that stable name. A
child control request requires a live, non-closing window endpoint. Mode
changes report the resolved size, which an OS may clamp. Focus success only
acknowledges that Aether issued the platform request: the OS may decline it or
apply it asynchronously, so the reply does not prove observed focus.
There is no implicit focused or current target. The boot window is named
`main` and is simply the first `WindowSpec` realized after winit resumes.

The five child operations may also be addressed to the manager, which
re-dispatches them at the sole window when exactly one is live and answers with
that window's own reply. It is a convenience for the single-window engine, not a
current target: with no window, or with several, the manager replies the
operation's `Err` naming the situation rather than choosing one, and the caller
names the window itself. The headless manager and its endpoints both refuse all
five, so an op that cannot be applied is always answered rather than dropped.

For an MCP `send_mail` request, `mode` remains a field of the
`aether.window.set_mode` params object and the optional windowed dimensions
sit beside it:

```json
{"mode": "Windowed", "width": 1600, "height": 1200}
```

`WindowId` is the raw mailbox identity of the named child, wrapped as a
wire-safe `u64` newtype. That same value keys manager state, render targets,
input events, and `WindowSelector::One`; callers do not maintain a separate
name-to-id mapping for control.

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

All manager variants claim the one shared window namespace, and all child
variants share the neutral `WindowInstance` identity:

- `DesktopWindowCapability` is pumped by `DesktopWindowApplication` and owns
  real winit state. It spawns and monitors a pooled `DesktopWindowInstance`
  for every attached window.
- `HeadlessWindowCapability` is the production headless runtime and fails
  every manager request immediately because there is no window peripheral, so
  it never creates a child.
- `SyntheticWindowCapability`, declared with
  `#[actor(singleton, runtime::synthetic)]`, is test-only. It keeps a
  deterministic in-memory window map, the same selector-aware routing
  behavior, and monitored pooled `SyntheticWindowInstance` children.
- `WindowInstance` is the neutral named child facade. Its desktop and synthetic
  runtimes forward the five id-less controls to their manager. A matching
  headless runtime defines fail-fast handlers, but the headless manager does not
  spawn child endpoints.
- The hub installs no window actor.

The neutral `WindowCapability` and `WindowInstance` aliases are the identities
consumer code should name. Concrete manager runtime identities share one
namespace constant inside `aether-window`; variants do not repeat a namespace
literal. Native, winit, and render ownership does not move to the children:
desktop host work stays on the pumped manager/driver boundary, while child
actors only forward control. The default headless manager implementation is
`runtime/mod.rs`, the headless named endpoint is `runtime/instance.rs`, and the
desktop and synthetic implementations live under their matching runtime
modules.

The initial desktop window still reads `AETHER_WINDOW_MODE` and
`AETHER_WINDOW_TITLE`. `AETHER_WINDOW_MODE` accepts `windowed`,
`windowed:WxH`, `fullscreen-borderless`, or `exclusive:WxH@HZ`; invalid input
warns and falls back to windowed mode. See [Configuration](configuration.md).

## Testing and extension

`SubstrateHarness` composes the synthetic manager and its supervised children.
Use the root typed actor sender for manager operations, an addressed child for
controls, and `HarnessOp::window_event` for an event:

```rust
use aether_actor::Addressable;

let subscribe = HarnessOp::actor::<SyntheticWindowCapability>().send(
    &SubscribeWindow {
        selector: WindowSelector::One(window),
        kind: Key::ID,
        mailbox: observer,
    },
);

let main = format!("{}://main", WindowCapability::NAMESPACE);
let title = HarnessOp::send_and_await_reply(
    main,
    &SetWindowTitle { title: "Inspector".to_owned() },
);

let press = HarnessOp::window_event(
    window,
    &Key { window, code: keycode::KEY_ENTER },
);
```

The child-control operation assumes the named window has already been created
and its creation operation has settled. Derive string recipients from
`WindowCapability::NAMESPACE` when Rust genuinely needs a boundary address;
ordinary actor code should use typed `resolve::<WindowInstance>(name)` instead.

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
