# The widget set & focus model

`aether-kit-widget` ships a set of guest-side widgets — a slider, a text field, a
multiline text area, a radio group, a fixed-row virtual list, a button, a label,
an image, a toggle, a segmented control, and a numeric editor — as ordinary
`#[actor(instanced)]` types.
A panel root spawns them as inline children (ADR-0114) and drives them entirely
by mail, so composing an editor panel is a matter of laying out widgets and
translating their value events, never re-deriving hit rects, focus, or per-row
layout for each new knob.

The set is a defaultless grab-bag module (ADR-0138): load a widget by its
`module@export` selector against the `aether_kit_widget` stem — `WidgetPanel` is
`aether_kit_widget@aether.kit.widget.panel`, the `EditorShell` arbiter is
`aether_kit_widget@aether.kit.widget.editor`, and so on. The `aether.kit.widget.*`
export namespaces themselves are unchanged.

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
  `TextFieldConfig`, `TextAreaConfig`, `RadioConfig`, `VirtualListConfig`,
  `ButtonConfig`, `LabelConfig`, `ImageConfig`, `ToggleConfig`,
  `SegmentedConfig`, `NumericConfig` — each embedding a `theme: Theme`. The
  config is both the value
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
  draw muted but leave input routing; read-only Slider, Radio, TextField,
  TextArea, VirtualList, Toggle, Segmented, and Numeric controls remain
  focusable but reject mutation. Button and Label ignore read-only and
  validation.
- **Interaction, down.** The root sends `FocusGained` / `FocusLost` and
  `HoverGained` / `HoverLost`. Hover comes from explicit edges, not from a child
  inferring absence from raw motion. Pressed and hover select an exclusive fill
  (Disabled → Pressed → Hover → Normal); focus and validation are separate
  outlines, so both remain visible together.
- **Value, up.** `SliderChanged { value, committed }`, `TextCommitted { text }`,
  `RadioSelected { index }`, `VirtualListSelected { selected_index }`, and
  `ButtonClicked` flow to the parent through `ctx.parent()`. A slider streams
  `committed: false` values through a drag and a final `committed: true` on
  release, so a consumer previews the drag and commits the expensive work
  once.

  `ToggleChanged { on }`, `SegmentedSelected { index }`, and
  `NumericChanged { value, committed }` use that same source-attributed lane;
  Numeric applies the preview/commit distinction to typed values.

## Fixed-row virtual lists

`VirtualListConfig { items, initial_selected_index, visible_row_count, theme,
state }` retains the complete string vector while realizing at most
`visible_row_count` rows. The panel fixes the slot height at
`theme.row_height * visible_row_count` and clips the slot to that viewport;
an empty item vector or zero-row viewport is not pointer- or focus-eligible.
This bounded realization is the intended path for hundreds or thousands of
uniform-height choices.

Up and Down move selection by one. PageUp and PageDown move by the configured
nonzero visible-row count. Movement clamps at the item-vector ends and shifts
the realized window only enough to reveal the selected row. A click divides
the assigned frame height by the number of rows actually realized, so a short
list fills its viewport and the frame's bottom edge remains exclusive. Hidden
lists answer every `Collect` with an empty draw list; disabled and read-only
lists reject both pointer and keyboard selection changes.

## Image widget

`WidgetKind::Image` spawns a non-interactive `ImageWidget` from
`ImageConfig`. The config borrows a session-scoped `texture_id` previously
created through `aether.render`; the creator still owns updates and
`DestroyTexture`. Re-sending `ImageConfig` replaces the borrowed texture and
presentation in place without resizing or respawning the parent slot.

The config names its natural pixel size with `natural_width_pixels` and
`natural_height_pixels`. `ImageFit::Fill` stretches the full texture into the
row, `Contain` preserves aspect ratio and centers the whole image, `Cover`
fills the row and center-crops through UV coordinates, and `Natural` centers
the configured natural size. Natural content larger than the row is clipped by
the parent-owned slot. `Contain` is the default. Consumer-created texture ids,
including the registry's first id `0`, are valid. Zero or non-finite natural
dimensions and non-positive or non-finite frame dimensions produce no textured
draw; the inert `ImageConfig::default()` therefore paints nothing.

Valid natural dimensions are reported through the existing
`WidgetDrawList::intrinsic` field, including while the image is hidden. The
reference panel does not yet consume child intrinsic values for layout, so
this report does not make its row content-sized. Hidden images keep their slot
but draw nothing. Disabled images stay visible with the theme's disabled alpha
applied to their configured tint. Images are never pointer- or focus-eligible;
read-only and validation state have no visual or behavioral meaning for this
static leaf.

## Toggle, segmented, and numeric controls

`WidgetKind::Toggle` spawns `ToggleWidget` from `ToggleConfig { label,
initial, theme, state }`. A left press arms the switch and a release back
inside toggles it once. A focused Enter press or matching Space release also
toggles once; key repeats, focus loss, read-only state, and unavailability
cancel the arm. `ToggleChanged { on }` reports the new boolean value. The
local draw orders the track before its moving knob and label, then adds the
common validation/focus outlines.

`WidgetKind::Segmented` spawns `SegmentedWidget` from `SegmentedConfig {
options, initial_index, theme, state }`. The assigned row is divided into
equal-width named segments. A pointer press selects its bucket, and focused
Left/Right movement clamps at the first and last option. Empty option lists
have no hit buckets. `SegmentedSelected { index }` reports only actual changes;
selected, hovered, pressed, disabled, validation, and focus presentation use
the common theme/state contract.

`WidgetKind::Numeric` spawns `NumericWidget` from `NumericConfig { min, max,
step, initial, theme, state }`. It reuses the shared `TextEditState`, named
`TextSpan`, and measured `SingleLineLayout`, so pointer placement, dragging,
selection, replacement, and clipboard edits stay on UTF-8 boundaries. Typed
characters arrive only through `TextInput`; `Key` remains navigation and
control (`Backspace`, Left/Right, Enter, Up/Down, and Ctrl+A/C/X/V).

Numeric keeps the visible buffer separate from its last committed number.
Empty, `-`, `.`, and other invalid or non-finite intermediates remain visible
and emit nothing. A finite edit is clamped and snapped for a
`NumericChanged { committed: false }` preview without rewriting what the user
typed. Enter or focus loss canonicalizes a valid value and emits
`committed: true`; an invalid buffer reverts to the last canonical value
without an event. Up/Down step from the current valid value, falling back to
the committed value, and immediately canonicalize and commit. Copy never
mutates, while cut and asynchronous paste pass through the same selection,
parse, clamp, and preview path.

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

TextField and TextArea share the same UTF-8-safe edit, selection, and IME
state. Once the configured font resolves, pointer placement, caret motion,
selection fills, preedit cursor bands, and preedit underlines all use its exact
glyph advances. `TextAreaConfig { initial, max_chars, rows, theme, state }`
adds a fixed whole-line viewport: `rows` is the number of visible theme rows
(`0` means one), vertical motion preserves the caret's preferred measured x
across shorter lines, and the viewport scrolls by complete lines to keep the
caret visible. Plain Enter inserts a newline; Ctrl+Enter sends
`TextCommitted` without changing the value. A multiline selection can cross
newlines and renders one measured band in each covered visible row.

## Scroll containers and wheel ownership

`ScrollWidget` (`aether.kit.widget.scroll`) is the stateful container for an
oversized widget subtree. A `WidgetKind::Scroll` child decodes `ScrollConfig`:
its `viewport_extent` is the row the parent places, `content_extent` is the
fixed clamp authority, `initial_offset` is clamped at startup, and `content`
is one ordinary `WidgetChildSpec` root. All new coordinate vocabulary is
field-named and unit-explicit:

- `ScrollExtent { width_pixels, height_pixels }`
- `ScrollOffset { x_pixels, y_pixels }`
- `ScrollDelta { x_pixels, y_pixels }`
- `ScrollResidual { x_pixels, y_pixels }`

The scroll contract introduces no positional tuple or array wrapper. The old
`WidgetChildSpec::origin` array is decoded immediately into local coordinates,
and `WidgetDrawList::intrinsic` remains only the existing compositor
compatibility channel; neither owns scroll bounds.

Painting and input use the same retained state in different coordinate
spaces. In local draw space, the content slot is placed at
`content_origin - offset` and clipped by a viewport-local `WidgetClipRect`.
In window input space, the child receives an absolute `WidgetFrame` at the
parent frame origin plus that same translated content origin. A directly
nested scroll viewport is hit-testable only where that frame intersects its
ancestor viewport. A nested scroll config's viewport extent must exactly match
the content extent its parent assigned it, or the slot is rejected.

A wheel always targets the deepest scroll viewport under the cursor using
`Focus::hit_test`; it never follows pointer capture. The consuming actor
converts chassis deltas once (`x_pixels = -delta_x`, `y_pixels = -delta_y`),
then clamps each axis independently. It emits
`ScrollOutcome { container, offset, consumed, residual }`. If an axis
overshoots a bound, only the exact `ScrollResidual` remainder moves to the
parent, already in content-space; parents apply it directly without negating
again. Intermediate scroll actors relay descendant outcomes unchanged, so a
panel log preserves inner-before-outer ownership. A remainder that reaches the
panel is logged as a terminal residual and dropped.

`SetTheme` follows the same actor tree to the retained content root. This keeps
live restyles and the panel's resolved session font id intact through nested
scroll containers.

## Editor-wide region ownership

`EditorShell` (export `aether.kit.widget.editor`) composes several independent
roots into one input domain without turning them into one widget tree. It is
the sole subscriber for interactive input across its configured regions; each
panel still owns widget focus and capture inside its own cluster, while the
shell owns region focus and capture between clusters.

Assemble an editor peer-first. Load each panel, console, or mover, retain the
returned `MailboxId`, and set its config's `owns_input` to `false`. Then load
one `EditorShell` with an ordered `EditorConfig { regions }`. Each `RegionSpec`
contains a named pixel rectangle, target mailbox, keyboard eligibility,
`RegionInputLanes`, and optional exact `EditorKeyChord`. Later entries are
topmost. A topmost region that rejects a lane blocks that event; routing does
not fall through to a covered region.

The first accepted pointer press owns pointer motion and releases across region
boundaries until the matching button is released. Wheel uses the position in
its own event. Keyboard, committed text, IME preedit, and modifiers route only
to the focused region. An exact activation chord can focus a region (for
example, the console's backquote chord). Ctrl+Tab cycles editor regions,
Ctrl+Shift+Tab cycles backward, and both the reserved press and matching
release are consumed. Plain Tab is forwarded unchanged so the focused panel's
own widget traversal remains intact.

`owns_input` defaults to `true`, preserving standalone behavior. It gates only
interactive subscriptions: panel/console/mover lifecycle and render roles are
unchanged, and console/mover continue subscribing to `WindowSize` directly.
The shell itself owns no lifecycle, render, or window-size work.

The terrain annotation workbench is the concrete peer-first assembly of this
model. Load the mark book under the exact default component name
`aether.kit.mark`, load `aether.kit.world`, then load
`aether.kit.terra` with the mark-book mailbox. Finally load
`aether_kit_workbench@aether.kit.workbench` with those three returned mailbox ids and a
named `WorkbenchLayout { tools, viewport, console }`. The workbench spawns a
specialized `TerrainToolPanel`, a camera-owning `TerrainViewport`, a
non-input-owning `ConsoleOverlay`, and the one `EditorShell` that routes their
three non-overlapping regions. The panel and viewport are distinct region
targets, so panel focus/capture and viewport terrain clicks remain separate
nested focus scopes.

`TerrainToolPanel` is intentionally not another configuration of the reference
panel. It composes the stock segmented, text, numeric, button, and label actors
under stable terrain-control subnames, attributes their value events by source,
and translates them into workbench intents. Preview, accept, and discard remain
unavailable until the world replies with `ProposalResult::Staged`; conflicting
controls are disabled while the coordinator has one correlated request in
flight. `WidgetPanel` and `PanelConfig` retain their demonstration/template
contract unchanged.

## The reference panel

`WidgetPanel` (export `aether.kit.widget.panel`) is the worked example — the
test vehicle and the template a real editor forks. It embeds `Composite` and
`Focus`, spawns the vertical stack its `PanelConfig` declares on its first
frame (each `WidgetChildSpec` names a `WidgetKind` and carries that widget's
pre-encoded config; an empty child list falls back to the built-in reference
stack), loads a font through `aether.text` and stamps the
session `font_id` into its theme when `load_font_result` arrives, drives the
collect/emit loop each frame, and routes input through `Focus`. Row height,
initial state, and static eligibility derive from each child's decoded config.
TextArea slots derive their height from `theme.row_height * rows.max(1)`.
A `WidgetKind::BehaviorHost` derives the same metadata from both its `wrapped`
discriminator and opaque `wrapped_config`; wrapping does not make every child
focusable. A `WidgetKind::Scroll` row takes its width and height from its named
viewport extent and also enters the panel's separate wheel-only hit table.
The vertical order follows the declared order, so what a panel
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
request the built-in stack above. `children: []` selects that fallback inside
an otherwise complete `PanelConfig`; the MCP schema encoder does not fill the
other fields from Rust's `Default`, so provide `x`, `y`, `width`,
`font_namespace`, `font_path`, `owns_input`, and the complete `theme` object.

That built-in stack is limited to `label`, `slider`, `radio`, `text_field`, and
`button`; it does not demonstrate the other stock kinds, including `Toggle`,
`Segmented`, or `Numeric`. A non-empty `children` list crosses a second schema
boundary: each `WidgetChildSpec.config` is already-encoded bytes for that
child's concrete config kind. The MCP harness can transport those bytes (for
example through its bytes embeds), but it does not currently provide a generic
nested-kind encoder that creates them from structured child JSON. Until that
primitive exists, author custom panel trees from Rust/component configuration
code rather than expecting `load_component` to recursively encode child
configs.

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
