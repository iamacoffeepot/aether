# The widget set & focus model

`aether-kit` ships a set of guest-side widgets — a slider, a text field, a
radio group, a button, and a label — as ordinary `#[actor(instanced)]` types.
A panel root spawns them as inline children (ADR-0114) and drives them entirely
by mail, so composing an editor panel is a matter of laying out widgets and
translating their value events, never re-deriving hit rects, focus, or per-row
layout for each new knob.

The widgets build on two foundations documented alongside them: the
draw-compositing protocol (ADR-0117 — `Collect` down, `WidgetDrawList` up, the
`Composite` helper, widget-local clipping, and the one-render-sender-per-cluster
emit) and the theme
(`Theme` tokens plus `Theme::fill`, carried on each widget's config). This page
covers the layer above those: the state and interaction mail every widget
speaks and the focus/hover/input model the root owns.

## State and interaction mail

A widget reacts to config, style, layout, external state, and root-owned
interaction data-down, then reports values and state changes events-up. The
events-up kinds carry **no widget-identity field** — the root attributes a reply
by the sender's `MailboxId` (`ctx.source_mailbox()`) against the children it
recorded at spawn, so a widget's identity is its inline subname and nothing a
widget sends can misreport it.

- **Config, down.** One `Config` kind per widget — `SliderConfig`,
  `TextFieldConfig`, `RadioConfig`, `ButtonConfig`, `LabelConfig` — each
  embedding a `theme: Theme`. The config is both the value
  `spawn_inline_child::<W>(subname, &config)` boots the widget with and a
  re-sendable mail: send a widget its config kind again to reconfigure it in
  place (a slider's range, a field's cap, a button's label).
- **Style, down.** `SetTheme { theme }` re-fans a live restyle. A widget adopts
  the new tokens and the next immediate-mode frame draws with them — one frame
  of latency, no invalidation bookkeeping. There is no cascade and no
  selectors; a one-off look is an explicit `theme` override on that widget's
  own config.
- **Layout, down.** The root assigns each child a `WidgetFrame { x, y, width,
  height }` in window pixels. The child caches it to lay out its local draw and
  to map a forwarded pointer position into its own space; the root keeps the
  same rect to offset the child's draws (through `Composite`) and to hit-test
  pointer input (through `Focus`). A generic `WidgetChildSpec.clip` is
  parent-local; the reference panel derives that slot clip from the assigned
  `WidgetFrame` so oversized child content cannot escape its row.
- **State, down and up.** Every stock config carries a defaulted
  `WidgetControlState { visible, enabled, read_only, validation }`.
  `SetWidgetState` replaces that external state without resetting the widget's
  value; a changed config or state mail emits source-attributed
  `WidgetStateChanged` so panel routing cannot drift. Hidden widgets keep their
  slot and answer `Collect` with an empty `WidgetDrawList`; disabled widgets
  draw muted but leave input routing; read-only Slider, Radio, and TextField
  remain focusable but reject mutation. Button and Label ignore read-only and
  validation.
- **Interaction, down.** The root sends `FocusGained` / `FocusLost` and
  `HoverGained` / `HoverLost`. Hover comes from explicit edges, not from a child
  inferring absence from raw motion. Pressed and hover select an exclusive fill
  (Disabled → Pressed → Hover → Normal); focus and validation are separate
  outlines, so both remain visible together.
- **Value, up.** `SliderChanged { value, committed }`, `TextCommitted { text }`,
  `RadioSelected { index }`, and `ButtonClicked` flow to the parent through
  `ctx.parent()`. A slider streams `committed: false` values through a drag and
  a final `committed: true` on release, so a consumer previews the drag and
  commits the expensive work once.

## Root-owned focus and input

Widgets never subscribe to input. The panel root subscribes the pointer and
keyboard streams once (the input cap) and the frame stage once (the lifecycle
cap), then routes every event through a `Focus` helper it embeds — the
input-side counterpart to `Composite`. `Focus` holds child hit rects in layout
order, static pointer/focus eligibility, dynamic visible/enabled availability,
the hovered and focused children, and the drag-captured child. It answers four
questions:

- **Where does a pointer event go?** To the drag-captured child if one holds
  capture — so a drag that leaves a widget's rect still reaches it — otherwise
  to the topmost child under the cursor.
- **Where does a keyboard event go?** To the focused child.
- **Where does hover live?** Independent hit testing yields a named
  `HoverTransition`; sibling crossings send lost before gained even while
  capture routes raw drag motion elsewhere.
- **What moves focus?** A left press on a focusable child, Tab forward, or
  Shift+Tab backward. Traversal wraps and skips hidden/disabled/static entries.
  A live availability change moves focus forward when its holder disappears
  and clears hover/capture through named transition effects.

Drag capture is the kit's own policy over the raw button vocabulary: a left
press that hits a widget sets capture on that child, moves route to it while
capture holds, and the matching release clears it. That is what lets a slider
track the cursor past the end of its track and a button cancel when the release
drifts off it. `MouseButton.button` and `MouseButtonRelease.button` use the
engine constants in `aether_kinds::mouse_button`; `LEFT` is `0`, including for a
synthetic `aether.mouse_button` sent over MCP.

A focused Button activates once on Enter press (repeat presses are suppressed
until release) and once on Space release after a matching Space press. Focus
loss or unavailability cancels the keyboard arm.

## The reference panel

`WidgetPanel` (export `aether.kit.widget.panel`) is the worked example — the
test vehicle and the template a real editor forks. It embeds `Composite` and
`Focus`, spawns the vertical stack its `PanelConfig` declares on its first
frame (each `WidgetChildSpec` names a `WidgetKind` and carries that widget's
pre-encoded config; an empty child list falls back to the built-in reference
stack of every widget), loads a font through `aether.text` and stamps the
session `font_id` into its theme when `load_font_result` arrives, drives the
collect/emit loop each frame, and routes input through `Focus`. Row height,
initial state, and static eligibility derive from each child's decoded config.
A `WidgetKind::BehaviorHost` derives the same metadata from both its `wrapped`
discriminator and opaque `wrapped_config`; wrapping does not make every child
focusable. The vertical order follows the declared order, so what a panel
contains is config data. Its
value-up handlers are the seam: each attributes the event by
`ctx.source_mailbox()` and is where a map editor translates a widget change
into world-knob driver mail. Fork it by handing it your own `children` and
filling in those handlers.

Inline children are externally addressable by lineage. Keep the exact root
`name` returned by `load_component`, then append
`/aether.embedded:<subname>`. For example, a panel loaded as `panel` returns
`aether.component/aether.embedded:panel`, and its built-in Button is
`aether.component/aether.embedded:panel/aether.embedded:button`. The empty
`children` fallback uses the stable subnames `label`, `slider`, `radio`,
`text_field`, and `button`; a declared `WidgetChildSpec` uses its own `subname`
in the same position. These aliases are ordinary mailbox names, so MCP
`send_mail` can target a child directly. This disables the built-in Button
without resetting its label or any sibling state:

```json
{
  "mails": [{
    "engine_id": "<engine-id>",
    "recipient_name": "aether.component/aether.embedded:panel/aether.embedded:button",
    "kind_name": "aether.kit.widget.set_state",
    "params": {
      "state": {
        "visible": true,
        "enabled": false,
        "read_only": false,
        "validation": "Valid"
      }
    }
  }],
  "fire_and_forget": false
}
```

At the MCP boundary, `load_component` takes a registry `selector` plus either
an inline structured `config` object or `config_path` pointing to a JSON object;
the harness schema-encodes that JSON to `PanelConfig`. Do not pre-encode
`LoadComponent.config` bytes in tool JSON. Use `describe_component` for the live
config kind and `describe_kinds` for its schema, and set `children` to `[]` to
request the built-in stack above.

To add a new widget — a dropdown, a checkbox, a color well — write one more
`#[actor(instanced)]` type that speaks the same state/interaction lanes and answers `Collect`
with a `WidgetDrawList`, then spawn it into a panel's stack. The focus model and
the draw protocol carry it with no new machinery.

## Local clipping and root emission

`WidgetDrawItem::{Quad, TexturedQuad, Text}.clip` uses `WidgetClipRect { x, y,
width, height }` in the drawing widget's local pixel space.
`WidgetDrawItem::TexturedQuad` also carries named destination and UV fields,
an `Rgba` tint, and a non-owning session texture id from `CreateTexture`; the
producer that created the texture remains responsible for update and destroy.
`WidgetChildSpec.clip` uses the same named clip type in the parent that owns
the slot. `Composite` moves an item clip with its geometry, intersects it with
the slot clip, and repeats that at each ancestor. A missing clip is unbounded;
an invalid, empty, disjoint, or edge-touching result omits the draw. Stock
leaves normally leave their item clip unset and rely on their parent-owned
slot.

Only the root has framebuffer coordinates. It converts the effective
`WidgetClipRect` to the render/text `ClipRect` when it emits. In one pass over
the non-text items, solids group into contiguous equal-clip batches and
textured items group by contiguous equal `(texture_id, clip)` keys. Kind,
texture, and clip transitions flush; repeated keys are never regrouped across
a transition. Both direct handlers target the same render recipient, whose
FIFO preserves authored order. Text still follows the established later lane.
Thus an unclipped all-solid tree remains one solid batch, while mixed items or
distinct clips may produce several mails from the same single root render
sender.
