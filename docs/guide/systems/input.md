# Input streams

> **Governing ADRs:** [ADR-0021](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0021-input-stream-subscriptions.md)
> (publish/subscribe routing),
> [ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md)
> (subscribers keyed by `KindId`),
> [ADR-0164](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0164-window-actor-owns-native-window-integration.md)
> (window-owned input publication), and
> [ADR-0248](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0248-lineage-is-an-ordered-tree.md)
> §9 (key focus).

Keyboard, pointer, resize, text, and IME events originate at windows, so the
window actor owns their translation and routing. There is no generic input
actor or extra relay mailbox. A consumer subscribes through
`WindowCapability`, chooses which windows it cares about, and receives the
event kind directly from the window actor.

`Tick` is not input. It is a frame-lifecycle stage subscribed through
`LifecycleCapability`; see [Frame lifecycle](lifecycle.md).

## Why the window actor owns these streams

Several actors can observe the same event. Gameplay, an editor overlay, and a
debug console may all need a key press, while render and layout consumers may
both need a resize. Selector-aware publish/subscribe keeps those consumers
independent without losing the source window.

The window manager already owns the facts needed to translate native events:
the engine/platform id mapping, per-window cursor position, modifiers, IME
composition, focus, occlusion, and lifecycle. Publishing there avoids an
encode/decode/encode relay and keeps multi-window routing in one place.

Streams remain keyed by `KindId`, the same schema-derived identifier used by
mail and dispatch. The manager publishes `K::ID` directly. It does not resolve
or cache a hand-maintained list of recognized input kinds.

## Event vocabulary

Every event below carries its source window's actor path in `window`:

| Kind | Additional data |
|---|---|
| `Key` | physical key `code` on press |
| `KeyRelease` | the matching physical key `code` |
| `MouseMove` | cursor `x`, `y` in physical-pixel window coordinates |
| `MouseButton` | button plus cursor position at press |
| `MouseButtonRelease` | button plus cursor position at release |
| `MouseWheel` | normalized deltas plus cursor position |
| `WindowSize` | physical-pixel width and height, plus the display `scale_factor` |
| `TextInput` | committed, layout-resolved text |
| `ImePreedit` | in-flight composition text and optional byte-offset span |
| `Modifiers` | Shift, Ctrl, Alt, and Meta state |

The window actor also publishes `WindowOpened`, `WindowClosed`, and
`WindowFocus` (`aether.window.focus_changed`: the window's path and whether it
now has focus, once per change) through the same selector machinery. A
component that tracks held keys or buttons subscribes to `WindowFocus` and
releases them when `focused` is `false`, because the release the operating
system delivers to another window never reaches this one. Lifecycle/control events and user input therefore
share one source identity without pretending they are all one generic
peripheral.

## Pixels on the wire are physical

Every pixel quantity the engine publishes is a **physical** pixel, and they all
agree with each other:

- the cursor position on `MouseMove`, `MouseButton`, `MouseButtonRelease`, and
  `MouseWheel` (the desktop chassis forwards winit's `PhysicalPosition`
  unconverted),
- `WindowSize.width` / `height`,
- `QuadSpace::Screen` — the space solid quads, textured quads, and
  `aether.render.draw_text` address.

So pointer-against-screen-space math is a direct comparison:

```rust
// Correct: both sides are already physical pixels.
let hovered = cursor.x >= rect.x && cursor.x < rect.x + rect.width;
```

Do **not** scale the cursor before that comparison. Multiplying by
`scale_factor` here misses the target by exactly the scale factor — on a 2x
display, by 2x — and the bug is invisible on a standard-density display because
the factor is then `1.0`.

`WindowSize.scale_factor` exists for the conversions that genuinely cross into
logical space, where a *measure* rather than a coordinate lives:

```text
physical = logical × scale_factor
```

Multiply to size a logical measure into physical pixels — a 16-logical-pixel
label, a 44-logical-pixel touch target — so it keeps its apparent size on any
display. Divide to hand logical coordinates to a consumer that wants them. The
desktop chassis publishes a fresh `WindowSize` on `ScaleFactorChanged` as well
as on resize, so a subscriber that caches the latest value never carries a stale
factor across a drag between displays; a synthetic window publishes `1.0`. A
component created after the window opened reads the factor from the
`aether.window.list` reply, and a later window's from `WindowOpened`.

## Subscribe by kind and window

Subscribe in `wire`, where mail is allowed. Declare the neutral window identity
as a dependency, spell your actor on the `wire` context, and name the publisher
and the kind:

```rust
use aether_window::{Key, WindowCapability, WindowSize};

#[actor(root, depends(WindowCapability))]
impl WasmActor for Editor {
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_, Self>) -> Result<(), ActorInitError> {
        ctx.subscribe::<WindowCapability, Key>();
        ctx.subscribe::<WindowCapability, WindowSize>();
        Ok(())
    }
}
```

A window subscribe covers every window: all current windows and windows created
later. `ctx.unsubscribe::<WindowCapability, K>()` is the teardown twin.

The verb is checked at compile time three ways. `WindowCapability` implements
`aether_actor::Publishes<K>`, the send-side mirror of `HandlesKind`, for the event
vocabulary above, `LifecycleCapability` for the stage kinds, and a kind neither
publishes has no impl at all. So subscribing at the wrong capability —
`ctx.subscribe::<WindowCapability, Tick>()` — is an `E0277` at the `wire` call
site whose message names the capability that does publish the kind, instead of a
stored subscription row that never fires. The failure it replaces is entirely
silent: the row is accepted, the event is dropped at its source for want of a
matching subscriber, and the component just looks dead. The actor must also
declare `depends(WindowCapability)`, and its handler for `K` must not declare a
reply (a `-> ()` handler, or an unchecked one), because a broadcast event has no one
waiting for a reply. `unsubscribe` carries the first two checks: a kind that
cannot be subscribed cannot be unsubscribed either.

The flat verb always selects every window. A per-window filter is the
subscribe request kind itself, sent with a `WindowSelector::One` selector:

```rust
ctx.send::<WindowCapability>(&SubscribeWindowSelf {
    selector: WindowSelector::One(self.editor_window),
    kind: MouseMove::ID,
});
```

That row receives the kind only from that window; `WindowSelector::All` matches
every window, as the flat verb does. If one mailbox matches both selectors, it
receives one copy. `UnsubscribeWindowSelf` with the same selector removes the
row. The kind send carries a bare `KindId`, so it skips the `Publishes<K>`
check the flat verb makes. The window checks the handler bound at run time
instead: the subscriber, of either form, must handle the kind with a silent or
unchecked handler, and the window refuses a sender whose published rows lack that
handler, since its events could never be handled (ADR-0231 §4).

Then handle the event as ordinary mail and inspect its source id:

```rust
#[handler::event]
fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, key: Key) {
    if key.window == self.editor_window {
        self.handle_editor_key(key.code);
    }
}
```

A ctx that omits its actor is typed by it: the macro reads `WasmCtx<'_>` as
`WasmCtx<'_, Self>`, so the ctx reaches only the actors the component declares
with `depends(R)`. The ctx's type arguments are receiver, sender, mode: the actor first, the
sender the handler requires second (`Anyone` when it states none), and the
reply mode third (`WasmCtx<'_, Self, Anyone, Unchecked>`); spell
`WasmCtx<'_, Erased>` for the untyped view.

The flat `ctx.subscribe` / `ctx.unsubscribe` verbs use the sending actor's
host-stamped mailbox and are the normal component API. The runtime monitors
each subscriber; when a monitored mailbox departs, all of its selector rows are
removed. Replacing a component preserves
its mailbox id and therefore its subscriptions.

If a kind has no matching subscribers, the event is dropped at its source. A
fan-out copy retains the external event's lineage, so settlement and traces
include every subscribed descendant.

## Key focus

Every key subscriber of a window is sent that window's keys, so a console that
wants to be typed into must keep those keys from the camera controller beside
it. It does that by taking **key focus**
([ADR-0248 §9](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0248-lineage-is-an-ordered-tree.md)).
Key focus is not operating-system window focus: `aether.window.focus` asks the
platform to bring a window forward and `aether.window.focus_changed` reports
it, and taking key focus does neither.

Each window has one key focus slot. It is empty, or it holds one actor and a
scope. Taking it is ordinary mail to `aether.window`, and the sender is the
holder:

```rust
// In an `#[actor(depends(WindowCapability))]` block that also handles
// `KeyFocusGained` and `KeyFocusLost`. `self.window` is the
// `ActorPath<WindowInstance>` the actor's config named.
#[handler::event]
fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
    let in_window = key.window == self.window;
    let opens = key.code == keycode::KEY_BACKQUOTE;

    if in_window && opens {
        ctx.send::<WindowCapability>(&TakeKeyFocus { window: self.window.clone(), scope: KeyFocusScope::Actor });
    }
}
```

| Kind | Name | Fields |
|---|---|---|
| `TakeKeyFocus` | `aether.window.take_key_focus` | `window`, `scope` |
| `ReleaseKeyFocus` | `aether.window.release_key_focus` | `window` |
| `KeyFocusGained` | `aether.window.key_focus_gained` | `window` |
| `KeyFocusLost` | `aether.window.key_focus_lost` | `window` |

`window` is an `ActorPath<WindowInstance>`: the window's canonical path, the
text every window event carries in its own `window` field, `aether.window.list`
reports, and `WindowSelector::One` takes, typed as a window's. A path whose
leaf is not `aether.window.instance` does not decode, so a take that names
something that cannot be a window never reaches the window manager. An actor
holds the typed path from its config, as the camera controller does, or writes
it from the window's name with `WindowInstance::path(&name)`. Every window event
carries the same typed path, so a holder takes for the window an event names by
passing `key.window.clone()`, or compares the two paths directly. `scope` is `KeyFocusScope::Actor`, the holder alone, or
`KeyFocusScope::Subtree`, the holder and every actor beneath it in lineage.

The rules of one window's slot:

- **The latest take wins.** The actor it replaces is sent `KeyFocusLost` and
  the sender `KeyFocusGained`, both naming the window. A take by the window's
  holder changes its scope and sends nothing.
- **A release empties the slot** and the holder is sent `KeyFocusLost`. Nothing
  is handed back and there is no stack: an actor that wants key focus again
  takes it again. A release from an actor that is not the holder changes
  nothing.
- **The holder's close empties every slot it holds**, and it is sent nothing.
- **The window's close removes its slot**, and the holder is sent
  `KeyFocusLost` after `WindowClosed` is published.
- **A take cannot fail and has no reply.** The typed path says what the path
  names, never that a window stands there: any window path is accepted, so a
  boot component may take for `main` before the window opens.
- **A window that loses operating-system focus keeps its slot**, so returning
  to it finds its holder unchanged.

An actor may hold key focus in several windows; each is taken, released and
lost on its own, which is why every kind names its window. A slot belongs to
one window: a holder in a tool panel leaves the keys of a document window where
they were. An all-window subscriber is filtered event by event, by the slot of
the window each event came from.

While a window's slot is held, its four key kinds are sent only to the key
subscribers inside the holder's scope. Key focus narrows who is sent an event;
it subscribes nobody, so a holder still subscribes to the kinds it wants. A
window whose slot is empty sends them to every subscriber.

| Event | Sent to |
|---|---|
| `Key` for a code that is not down | the subscribers the slot admits; the window records the slot's holder and scope for that code |
| `Key` for a code already down (a platform repeat) | the subscribers the press's record admits |
| `KeyRelease` | the subscribers the press's record admits, and the record is removed; with no record, the slot as it is now |
| `TextInput`, `ImePreedit` | the subscribers the slot admits as it is now |
| every other published kind | every subscriber |

So a key press owns its repeats and its release. A W held when a console takes
key focus keeps repeating to the camera and its release reaches the camera. An
Enter that makes a console release key focus on its press still sends its
release to the console alone. A window that loses operating-system focus
forgets which keys are down, because those releases never arrive.

**Who may take.** The manager's take and release handlers state the
`KeyFocusHolder` protocol as their sender (ADR-0231 §11): the two notices,
each handled silently. `ctx.send::<WindowCapability>(&TakeKeyFocus { .. })`
does not build for an actor that lacks a handler for either notice, and mail
that arrives by any other route from a sender that does not cover the protocol,
or with no actor sender, is refused before the handler runs. An MCP `send_mail`
reaches the window with the RPC server as its sender, so **a session cannot
take key focus**: mail the actor that should hold the keys, and let it take.

**Across a republish.** A republish keeps the holder's mailbox, so the slot
stands and its notices reach the successor. The successor is built fresh and
does not remember holding key focus: carry the windows it holds through
`on_dehydrate` and `on_rehydrate`, or send `ReleaseKeyFocus` for each from
`on_rehydrate`, which changes nothing where it is not the holder.

Limits:

- A take is mail. It lands behind the events already queued at the window, so
  the keystroke that opens a console delivers its own text to every `TextInput`
  subscriber before the console's take arrives.
- Text follows the slot as it is now. `TextInput` carries no key code, so a key
  held while a console takes key focus keeps sending its `Key` repeats to the
  camera and types its repeated text into the console.
- A composition in progress is not handled. When a slot changes
  mid-composition the old holder is sent `KeyFocusLost` and drops its own
  composition; the platform's composition state is untouched.
- A subscriber that joins a scope mid-press is sent a release without a press.
- A window is named by its typed canonical path. A path that is not a
  window's is refused when the mail is decoded. A window path no window stands
  at takes a slot no key event is routed by, held until its holder releases it
  or closes.
- Key focus does not move the operating system's focus. An actor that takes
  key focus in a window that is not the focused one holds that slot and is sent
  nothing until the window is focused.
- The pointer is unaffected, and clicking away does not release key focus.

## Text and IME

`Key` is a physical scancode edge, not a character. Text fields should consume
the platform's layout- and IME-resolved streams:

- `TextInput { window, text }` contains committed characters. It forwards key
  repeats. The desktop runtime deduplicates plain key text and IME commits
  behind its per-window composition state.
- `ImePreedit { window, text, cursor_begin, cursor_end }` contains the
  not-yet-committed composition. The cursor values are optional byte offsets;
  empty text clears the preedit.
- `Modifiers { window, shift, ctrl, alt, meta }` is latest-wins state. Cache it
  per window and combine it with `Key`; `meta` is Command on macOS and the
  Windows/super key elsewhere.

Editing commands still use the stable `aether_window::keycode` constants:
`KEY_BACKSPACE`, `KEY_DELETE`, the arrow keys, `KEY_HOME`, `KEY_END`,
`KEY_PAGE_UP`, `KEY_PAGE_DOWN`, and `KEY_ENTER`.

Pointer button and wheel kinds include the cursor coordinates captured for that
same event, so click, drag, and zoom behavior does not need to correlate a
separate `MouseMove`.

## Synthetic events in tests

Production headless has no window peripheral and composes no window actor.
`SubstrateHarness` deliberately composes `WindowCapability` with its test-only
synthetic backend (`WindowParams::Synthetic`), which models windows and uses
the same selector-aware fan-out as desktop:

```rust
let synthetic = harness.actor_ref::<WindowCapability>();
let window = WindowInstance::path(&LoadName::new("main")?);
let event = Key { window: window.clone(), code: keycode::KEY_W };
let op = HarnessOp::window_event(&synthetic, window, &event);
```

`window_event` accepts any `K: Kind`, encodes it once, and wraps it for the
synthetic runtime with `K::ID`, sent through the synthetic window capability's
reference. Neither the harness nor the window actor
declares a list of injectable kinds. The event's embedded `window` path should
match the source passed to `window_event`.

This is test injection, not a production headless fallback and not a route for
MCP clients to invent native input. Tests that need window behavior opt into
the deterministic backend; a production profile without a window composes no
window actor.

## Extending input

For another window-originated stream:

```text
define Kind { window: ActorPath<WindowInstance>, ... }
    → translate the native event in aether-window
    → publish K::ID through WindowSelector routing
```

No chassis cache, registry lookup, central input enum, or relay actor is part of
that path.

Do not force unrelated devices into the window actor. A gamepad, raw HID
device, or network controller should introduce a concrete source actor with
its own ownership and subscription policy. The rule is that source actors
publish their own events, not that every possible input belongs to windows.

## Where to read more

- Window lifecycle, control, runtime variants, and thread ownership —
  [Window](window.md).
- Publish/subscribe and kind-id decisions —
  [ADR-0021](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0021-input-stream-subscriptions.md)
  and
  [ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md).
- `wire`, handlers, and component replacement —
  [Components & lifecycle](components.md).
- Mail lineage and settlement —
  [Mail, kinds & scheduling](mail-and-kinds.md).
