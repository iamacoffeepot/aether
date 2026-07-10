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
covers the layer above those: the four mail lanes every widget speaks and the
focus-and-input model the root owns.

## Four lanes of mail

A widget reacts to three data-down lanes and reports on one events-up lane. The
value kinds carry **no widget-identity field** — the root attributes a reply by
the sender's `MailboxId` (`ctx.source_mailbox()`) against the children it
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
- **Value, up.** `SliderChanged { value, committed }`, `TextCommitted { text }`,
  `RadioSelected { index }`, and `ButtonClicked` flow to the parent through
  `ctx.parent()`. A slider streams `committed: false` values through a drag and
  a final `committed: true` on release, so a consumer previews the drag and
  commits the expensive work once.

## Root-owned focus and input

Widgets never subscribe to input. The panel root subscribes the pointer and
keyboard streams once (the input cap) and the frame stage once (the lifecycle
cap), then routes every event through a `Focus` helper it embeds — the
input-side counterpart to `Composite`. `Focus` holds the child hit rects in
layout order, the focused child, and the drag-captured child, and answers three
questions:

- **Where does a pointer event go?** To the drag-captured child if one holds
  capture — so a drag that leaves a widget's rect still reaches it — otherwise
  to the topmost child under the cursor.
- **Where does a keyboard event go?** To the focused child.
- **What moves focus?** A left press on a focusable child, or Tab (which cycles
  focusable children in registration order, wrapping). Each move yields the
  `(previous, next)` pair the root turns into a `FocusLost` down to the old
  holder and a `FocusGained` down to the new one, and each widget draws its own
  focus ring and caret from that — the root carries no per-widget-type visual
  knowledge.

Drag capture is the kit's own policy over the raw button vocabulary: a left
press that hits a widget sets capture on that child, moves route to it while
capture holds, and the matching release clears it. That is what lets a slider
track the cursor past the end of its track and a button cancel when the release
drifts off it.

## The reference panel

`WidgetPanel` (export `aether.kit.widget.panel`) is the worked example — the
test vehicle and the template a real editor forks. It embeds `Composite` and
`Focus`, spawns the vertical stack its `PanelConfig` declares on its first
frame (each `WidgetChildSpec` names a `WidgetKind` and carries that widget's
pre-encoded config; an empty child list falls back to the built-in reference
stack of every widget), loads a font through `aether.text` and stamps the
session `font_id` into its theme when `load_font_result` arrives, drives the
collect/emit loop each frame, and routes input through `Focus`. Row height and
focusability derive from each child's decoded config and the vertical order
follows the declared order, so what a panel contains is config data. Its
value-up handlers are the seam: each attributes the event by
`ctx.source_mailbox()` and is where a map editor translates a widget change
into world-knob driver mail. Fork it by handing it your own `children` and
filling in those handlers.

To add a new widget — a dropdown, a checkbox, a color well — write one more
`#[actor(instanced)]` type that speaks the same four lanes and answers `Collect`
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
