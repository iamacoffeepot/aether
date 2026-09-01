# Input streams

> **Governing ADRs:** [ADR-0021](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0021-input-stream-subscriptions.md)
> (publish/subscribe routing),
> [ADR-0068](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0068-input-subscribers-keyed-by-kindid.md)
> (subscribers keyed by `KindId`), and
> [ADR-0164](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0164-window-actor-owns-native-window-integration.md)
> (window-owned input publication).

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

Every event below carries its source `WindowId`:

| Kind | Additional data |
|---|---|
| `Key` | physical key `code` on press |
| `KeyRelease` | the matching physical key `code` |
| `MouseMove` | cursor `x`, `y` in window coordinates |
| `MouseButton` | button plus cursor position at press |
| `MouseButtonRelease` | button plus cursor position at release |
| `MouseWheel` | normalized deltas plus cursor position |
| `WindowSize` | physical-pixel width and height, plus the display `scale_factor` |
| `TextInput` | committed, layout-resolved text |
| `ImePreedit` | in-flight composition text and optional byte-offset span |
| `Modifiers` | Shift, Ctrl, Alt, and Meta state |

The window actor also publishes `WindowOpened` and `WindowClosed` through the
same selector machinery. Lifecycle/control events and user input therefore
share one source identity without pretending they are all one generic
peripheral.

## Two pixel spaces

The mouse kinds report the cursor in **logical** pixels. `WindowSize.width` /
`height` and `QuadSpace::Screen` — the space solid quads, textured quads, and
`aether.text.draw` address — are **physical** pixels. `WindowSize.scale_factor`
is what relates them:

```text
physical = logical × scale_factor
```

So hit-testing the cursor against screen-space geometry, or sizing HUD text for
the display it lands on, scales through the cached `scale_factor` rather than
assuming the two spaces coincide. They do coincide at `1.0`, which is why the
mismatch is invisible on a standard-density display and off by exactly 2x on a
2x one. The desktop chassis publishes a fresh `WindowSize` on
`ScaleFactorChanged` as well as on resize, so a subscriber that caches the
latest value never carries a stale factor across a drag between displays; a
synthetic window publishes `1.0`.

## Subscribe by kind and window

Subscribe in `wire`, where mail is allowed, through the neutral window
identity:

```rust
use aether_kinds::{Key, MouseMove, WindowSize};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};

fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
    let windows = ctx.actor::<WindowCapability>();

    windows.subscribe::<Key>(WindowSelector::All);
    windows.subscribe::<WindowSize>(WindowSelector::All);
    windows.subscribe::<MouseMove>(WindowSelector::One(self.editor_window));
}
```

`WindowSelector::One(id)` receives the kind only from that window.
`WindowSelector::All` includes all current windows and windows created later.
If one mailbox matches both selectors, it receives one copy.

Then handle the event as ordinary mail and inspect its source id:

```rust
#[handler::single]
fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, key: Key) {
    if key.window == self.editor_window {
        self.handle_editor_key(key.code);
    }
}
```

The reflexive `subscribe`/`unsubscribe` facade methods use the sending actor's
host-stamped mailbox and are the normal component API. The rare
`subscribe_for`/`unsubscribe_for` methods name another local mailbox. The
runtime validates and monitors explicit subscribers; when a monitored mailbox
departs, all of its selector rows are removed. Replacing a component preserves
its mailbox id and therefore its subscriptions.

If a kind has no matching subscribers, the event is dropped at its source. A
fan-out copy retains the external event's lineage, so settlement and traces
include every subscribed descendant.

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

Editing commands still use the stable `aether_kinds::keycode` constants:
`KEY_BACKSPACE`, `KEY_DELETE`, the arrow keys, `KEY_HOME`, `KEY_END`,
`KEY_PAGE_UP`, `KEY_PAGE_DOWN`, and `KEY_ENTER`.

Pointer button and wheel kinds include the cursor coordinates captured for that
same event, so click, drag, and zoom behavior does not need to correlate a
separate `MouseMove`.

## Synthetic events in tests

Production headless has no window peripheral and publishes no window events.
`SubstrateHarness` deliberately composes the test-only
`SyntheticWindowCapability`, which models windows and uses the same
selector-aware fan-out as desktop:

```rust
let event = Key { window, code: keycode::KEY_W };
let op = HarnessOp::window_event(window, &event);
```

`window_event` accepts any `K: Kind`, encodes it once, and wraps it for the
synthetic runtime with `K::ID`. Neither the harness nor the window actor
declares a list of injectable kinds. The event's embedded `WindowId` should
match the source passed to `window_event`.

This is test injection, not a production headless fallback and not a route for
MCP clients to invent native input. Tests that need window behavior opt into
the deterministic runtime; unsupported production profiles fail fast.

## Extending input

For another window-originated stream:

```text
define Kind { window: WindowId, ... }
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
