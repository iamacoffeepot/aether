# Window

> **Governing ADRs:** [ADR-0035](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0035-substrate-chassis-split.md)
> (the substrate/chassis split) and
> [ADR-0164](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0164-window-actor-owns-native-window-integration.md)
> (the application-scoped multi-window manager),
> [ADR-0167](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0167-window-manager-supervises-addressable-window-actors.md)
> (addressable named window children), and
> [ADR-0212](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0212-native-window-chrome.md)
> (native menu bar, cursor icons, application name).

`aether-window` is the bespoke home of window behavior. The application-scoped
`WindowCapability` manager owns global lifecycle and event routing, while each
live named window has an addressable `WindowInstance` child for control mail.
Callers use those two identities, stable window names, and window paths; they
never hold a native window handle or address a desktop- or test-specific
implementation.

The desktop chassis still owns the application thread and the call to winit's
event loop. That is thread ownership, not window-domain ownership:

```text
aether-chassis-desktop
    EventLoop::run_app(...)
        └── DesktopWindowApplication       // aether-window
              ├── winit ApplicationHandler
              ├── DesktopWindowSlot         // WindowCapability, desktop backend
              ├── window path ↔ winit WindowId maps
              ├── monitored pooled WindowInstance children
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
multi-window behavior one owner. The manager uses each child's canonical actor
path as its stable engine identity, maps it to a platform id, retains
per-window cursor/IME/focus state, publishes events with their source path, and
coordinates the matching render target. The chassis composes this application
with render, lifecycle, and shutdown; it does not interpret raw winit events.

## The public surface

Consumers declare `depends(WindowCapability)` and send the list, create, and
subscription kinds to the manager with `ctx.send::<WindowCapability>(..)` and
`ctx.subscribe::<WindowCapability, K>()`. A single-window consumer mails the
per-window operations to the manager too, which re-dispatches them at the sole
window (see below). A window whose proof the caller holds takes them directly:
`ctx.send_to(&window, &SetWindowTitle { .. })`. There is no by-name window
lookup in actor code.

```rust
use aether_kinds::{Key, WindowMode};
use aether_window::{
    CreateWindow, ListWindows, RequestWindowRedraw, SetWindowTitle, WindowCapability, WindowSizeRequest,
    WindowSpec,
};

// In an `#[actor(depends(WindowCapability))]` block.
fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    ctx.send::<WindowCapability>(&ListWindows);
    ctx.send::<WindowCapability>(&CreateWindow {
        spec: WindowSpec {
            name: "inspector".to_owned(),
            title: "Inspector".to_owned(),
            mode: WindowMode::Windowed,
            size: Some(WindowSizeRequest { width: 960, height: 540 }),
        },
    });
    ctx.subscribe::<WindowCapability, Key>();

    ctx.send::<WindowCapability>(&SetWindowTitle { title: "Aether".to_owned() });
    ctx.send::<WindowCapability>(&RequestWindowRedraw);
}
```

Typed Rust code should resolve through the actor identities rather than copy
the root namespace. At string-addressed boundaries such as MCP and harness
operations, the same live child may be named by either its canonical or
short recipient:

```text
aether.window/aether.window.instance:main
aether.window/:main
```

The request/reply families are:

| Operation | Recipient and input | Successful reply |
|---|---|---|
| `list` | manager; no input | every live `WindowInfo`, ordered by path (window name order) |
| `create` | manager; `WindowSpec` | the attached window's `WindowInfo` |
| subscribe/unsubscribe | manager; selector and subscription (event kind and subscriber path), or selector and kind for the `_self` forms | acknowledgement |
| `close` | named child; no input | acknowledgement |
| `set_mode` | named child; mode and optional windowed size | resolved mode and size |
| `set_title` | named child; title | applied title |
| `set_menu` | named child; `Vec<WindowMenu>` | acknowledgement, or `Err` where the platform has no bar |
| `set_cursor` | named child; `CursorIcon` | acknowledgement |
| `focus` | named child; no input | request acknowledgement |
| `request_redraw` | named child; no input | request acknowledgement |

A window endpoint that closes while one of its commands is still in flight
answers that command's `Err`. The manager answers a pending create's `Err` the
same way.

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

The seven child operations may also be addressed to the manager, which
re-dispatches them at the sole window when exactly one is live and answers with
that window's own reply. It is a convenience for the single-window engine, not a
current target: with no window, or with several, the manager replies the
operation's `Err` naming the situation rather than choosing one, and the caller
names the window itself. An op a backend cannot apply is answered with its own
`Err` rather than dropped.

For an MCP `send_mail` request, `mode` remains a field of the
`aether.window.set_mode` params object and the optional windowed dimensions
sit beside it:

```json
{"mode": "Windowed", "width": 1600, "height": 1200}
```

A window is named by the canonical actor path of its child,
`aether.window/aether.window.instance:<name>`, an `ErasedActorPath` that
`aether_window::window_path` writes from the actor types. That same path keys
manager state, render targets, input events, and `WindowSelector::One`; no
field carries the child's mailbox position (ADR-0230). `capture_frame` also
accepts the short form `aether.window/:<name>`, which the render capability
proves and canonicalizes when the request arrives.

`WindowInfo` reports:

```rust
pub struct WindowInfo {
    pub path: ErasedActorPath,
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
kind carries its window's path in `window`. A subscriber chooses one window or
all current and future windows:

```rust
// In an `#[actor(depends(WindowCapability))]` block.
fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    ctx.subscribe::<WindowCapability, WindowOpened>();
    ctx.subscribe::<WindowCapability, WindowSize>();
    ctx.send::<WindowCapability>(&SubscribeWindowSelf {
        selector: WindowSelector::One(self.viewport),
        kind: MouseMove::ID,
    });
}
```

The flat verb selects `WindowSelector::All`, which is prospective. If the same
actor subscribes through both `All` and `One(id)`, recipient lookup unions the
sets and sends one copy. Both forms subscribe the sending actor, which must
handle the kind silently or manually; the manager types it as a
`Subscriber<K>` with the guard cast (ADR-0231 §4), refuses a sender whose
published rows lack that handler, monitors it, and removes all of its rows
when it departs.

An operator or a test subscribes another actor with `aether.window.subscribe`,
whose `subscription` names the kind and the subscriber's canonical path, as in
`{"Key": "ui.root"}`: the path `load_component`
returns. The path decodes only when the live actor there handles the kind
silently (ADR-0231 §3), and the manager proves it live at receipt.

Publication sends each event through the kind's typed subscriber set, which
holds a `ProtocolRef<Subscriber<K>>` per subscriber, so a fan-out of any other
kind does not compile. There is no registry lookup or relay actor. See
[Input streams](input.md) for the event vocabulary and text/IME semantics.

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
surface/configuration bundle per window path; the window manager owns native
window lifecycle and asks the integration to attach or detach the
corresponding render target. The native `Arc<Window>` remains same-thread host
state and never becomes a wire payload.

## Backends

`WindowCapability` is one identity with one receive surface, written once in
`runtime/mod.rs`; `WindowInstance` is one endpoint, written once in
`runtime/instance.rs`. The manager's runtime runs one of two backends, each
compiled in by its crate feature and chosen at boot by `WindowParams`:

- **Desktop** (`desktop` feature, `runtime/desktop/`) owns real winit state. It
  boots only through `DesktopWindowSlot::boot`, which boots the manager pumped
  on the application thread with `WindowParams::Desktop`; the boot value has no
  public constructor, so the desktop backend never runs pooled, where nothing
  would realize its host actions. It spawns and monitors a pooled
  `WindowInstance` for every attached window.
- **Synthetic** (`synthetic` feature, `runtime/synthetic/`) is test-only. The
  harness chassis compose it with `with_actor::<WindowCapability>(WindowParams::Synthetic)`.
  It keeps a deterministic in-memory window map, the same selector-aware
  routing, and monitored pooled `WindowInstance` children.

`Params`, not the feature, picks the backend because cargo unifies features
across a build: a workspace build compiles both backends into one crate, so
only the composer knows which it wants. The crate's default features are
empty, so a wasm guest names `WindowCapability` without the runtime, and
`runtime` without a backend is a compile error.

A chassis with no window peripheral — headless, the hub — composes no window
actor, so a component that depends on `WindowCapability` is refused at load
there. The endpoint forwards every control to the manager as an
`ApplyWindowCommand` and holds the caller's reply until the manager answers;
native, winit, and render ownership stay with the manager.

The initial desktop window still reads `AETHER_WINDOW_MODE` and
`AETHER_WINDOW_TITLE`. `AETHER_WINDOW_MODE` accepts `windowed`,
`windowed:WxH`, `fullscreen-borderless`, or `exclusive:WxH@HZ`; invalid input
warns and falls back to windowed mode. `AETHER_APP_NAME` / `--app-name`
(default `Aether`) names the product; see [Native chrome](#native-chrome)
below and [Configuration](configuration.md).

## Native chrome

Three surfaces cover what the platform draws around the client area, and one
input rule keeps the client area behaving the way the platform does.

### The menu bar

`set_menu` installs a real menu bar for the addressed window:

```rust
main.set_menu(vec![WindowMenu {
    title: "File".to_owned(),
    items: vec![
        WindowMenuItem {
            id: 1,
            label: "Save".to_owned(),
            shortcut: "Cmd+S".to_owned(),
            enabled: true,
            separator_after: true,
        },
        WindowMenuItem {
            id: 2,
            label: "Close".to_owned(),
            shortcut: "Cmd+W".to_owned(),
            enabled: true,
            separator_after: false,
        },
    ],
}]);
```

**Send window ops detached from a frame's chain.** The desktop window is
a pumped actor: it runs when the winit loop turns, which happens once the
frame advances, and the frame advances once the tick's causal chain has
settled. A `set_menu`, `set_title`, or `set_cursor` sent from a tick
handler with the chained `send` therefore puts the window's own mail
inside the chain the frame is waiting on, and the frame loop wedges
(`gate desktop.frame_advance wedged`, then a fatal abort). From a
component, address the `aether.window` root and send detached:

```rust
ctx.send_detached::<WindowCapability>(&SetWindowMenu { menus });
```

The root routes a per-window command to the sole live window, and replies
the op's `Err` when there are zero or several. A string-addressed caller
(MCP, a harness) names one window by its short path, such as
`aether.window/:main`. The reply (`set_menu_result` and its siblings) still
reaches the sender.

`id` is the caller's own opaque number. It rides back verbatim on
`aether.window.menu_activated { window, id }`, which reaches subscribers
through the same selector-aware family every window-originated kind uses:

```rust
ctx.subscribe::<WindowCapability, WindowMenuActivated>();
```

`shortcut` is accelerator text in muda's grammar — `"Cmd+S"`,
`"Ctrl+Shift+P"`, `""` for none. The platform renders it and, where it can,
honours it: on macOS a matching keystroke fires the item rather than reaching
the window, which is the native behaviour. A shortcut the platform cannot
parse costs that item its accelerator and logs a warning; it does not fail the
menu.

Platform coverage is [muda](https://crates.io/crates/muda)'s: the macOS
application menu bar and the Windows per-window bar. Every other target replies
`Err` naming the situation rather than hanging — draw an in-window menu bar
there.
Two further asymmetries follow from the platforms themselves: macOS has one
menu bar per *application*, so the last window to install one owns it (its
activations still reach that window's own subscribers), and macOS prepends an
application submenu carrying About / Hide / Show All / Quit, titled with the
application name.

### Cursor icons

`set_cursor` sets the addressed window's pointer shape, so a hovered element
can say what the gesture does before it starts:

```rust
main.set_cursor(CursorIcon::ResizeHorizontal);
```

The vocabulary names the movement, not the platform shape: `Default`,
`Pointer`, `Text`, `Move`, `ResizeHorizontal`, `ResizeVertical`,
`ResizeDiagonalRising` (bottom-left to top-right), `ResizeDiagonalFalling`,
`Grab`, `Grabbing`, `NotAllowed`, `Wait`.

### The application name

`AETHER_APP_NAME` / `--app-name` (default `Aether`) is the product's name as
the platform shows it. It titles the macOS application menu's About and Quit
items, names the macOS process (`ps`, Activity Monitor, the standard About
panel), and supplies the boot window's title when `AETHER_WINDOW_TITLE` is
unset. It resolves through the ordinary derive-`Config` path alongside the
window knobs.

One macOS surface it cannot reach from inside the process is the bold first
title in the menu bar. macOS draws that from the *application's* name, and an
executable outside an `.app` bundle has no bundle name, so the platform uses
the file it executed — fixed at launch, moved by neither
`NSProcessInfo.setProcessName` nor a `CFBundleName` written into the main
bundle's info dictionary. The fleet therefore materializes a spawned engine's
binary under the name the spawn's own `--app-name` gives (an application name
that is not a plain file name — a separator, a control character, `.`, `..`, or
over 64 characters — falls back to the neutral `substrate`), so a hub-spawned
desktop engine is named from launch. A binary executed directly is named after
that file: run `aether-desktop` and the bar reads `aether-desktop`, so ship a
product under a copy or symlink named for it, or inside a real `.app` bundle
with a `CFBundleName`.

### Key repeat

A held key repeats at the platform's own rate, and every repeat publishes an
ordinary `aether.key` press. No `aether.key_release` separates them, so a
consumer pairing press with release reads a held key as one press-and-hold,
while a consumer acting per press acts per repeat — which is what makes a held
Backspace delete repeatedly in a text field.

## Testing and extension

`SubstrateHarness` composes the synthetic manager and its supervised children.
Send manager operations through the manager's reference, controls through a
child's reference looked up beneath it, and events with
`HarnessOp::window_event`:

```rust
let synthetic = harness.actor_ref::<WindowCapability>();
let subscribe = HarnessOp::send_and_settle(
    &synthetic,
    &SubscribeWindow {
        selector: WindowSelector::One(window),
        subscription: WindowSubscription::Key(ActorPath::<Relay>::root().narrow()),
    },
);

let main = harness.child::<WindowCapability, WindowInstance>(&synthetic, LoadName::new("main")?)?;
let title = HarnessOp::send_and_await_reply(&main, &SetWindowTitle { title: "Inspector".to_owned() });

let press = HarnessOp::window_event(
    &synthetic,
    window,
    &Key { window, code: keycode::KEY_ENTER },
);
```

The child lookup assumes the named window has already been created and its
creation operation has settled: it proves only a `Live` child. Actor code
addresses the sole window through the manager, or a window whose proof it
holds; there is no by-name window lookup in actor code.

Synthetic injection is not a production API: `aether.window.inject_event`
exists only in a build that compiles the `synthetic` feature in, which no
production chassis enables.

To add a window-originated event, define the kind with a `window:
ErasedActorPath` field, add it to the `published_window_kinds!` list in
`crates/aether-window/src/lib.rs`, which writes its `Publishes` impl,
`WindowSubscription` variant, typed subscriber set, and dispatch arms, and emit
it from window state. Do not add a chassis kind cache or a generic input
relay. A future non-window device such
as a gamepad or raw HID source should have its own concrete source actor.

## Where to read more

- The full ownership and threading decision —
  [ADR-0164](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0164-window-actor-owns-native-window-integration.md).
- Window-originated event routing — [Input streams](input.md).
- Pumped actors, external roots, and settlement —
  [Tracing & settlement](tracing-and-settlement.md).
- The deterministic runtime and typed injection —
  [SubstrateHarness and FleetHarness](../testing/substrateharness-and-fleetharness.md).
