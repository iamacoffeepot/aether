# The widget set & focus model

`aether-kit-widget` ships a set of guest-side widgets — a slider, a text field, a
multiline text area, a radio group, a fixed-row virtual list, a button, a label,
an image, a toggle, a segmented control, a tab strip, a dropdown, a menu bar,
a numeric editor, a tooltip, a toast region, and a splitter — as ordinary
`#[actor(instanced, composable)]` types.
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

The theme carries a type scale and a selection role alongside its palette, and
both exist so one visual token carries one meaning. `TextRole` names the step a
run of text is set at — `Title`, `Heading`, `Body`, `Caption` — and
`Theme::text_size_pixels(role)` resolves it against `title_size_pixels`,
`heading_size_pixels`, `label_size_pixels`, and `caption_size_pixels`, so
hierarchy on a screen is a property of what a line *is*, never a pixel size
picked at a call site. `space_unit_pixels` with `Theme::space(steps)` is the
matching rule for distance: every gap a layout draws is a whole number of
units, so the whole screen lands on one grid. `selection` and `selection_text`
are the current item of a list, a radio group, a segmented control, a tab
strip, or a dropdown — a *state*, drawn as a lit row rather than as something
to press; `accent` and `accent_text` stay reserved for the primary action and
the focus ring, so a chosen row and a pressable button never share a look.
`Theme::scaled(factor)` multiplies every metric and leaves every color alone,
which is how a consumer takes the display's scale factor without restating the
scale.

`info`, `warning`, and `error` are the **severity scale** — a notice that
reports, one that cautions, and one that failed. `warning` and `error` double
as the validation outline roles, and `info` exists because the three have to be
three distinguishable colours: a notice drawn in `outline` is a notice nobody
sees, and one drawn in `accent` claims to be the primary action. None of the
three is ever the accent.

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
  `SegmentedConfig`, `TabStripConfig`, `DropdownConfig`, `MenuBarConfig`,
  `NumericConfig` — each embedding a
  `theme: Theme`. The
  config is both the value
  `spawn_inline_child::<WidgetPanel, W>(subname, &config)` boots the widget with and a
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
  TextArea, VirtualList, Toggle, Segmented, TabStrip, and Numeric controls
  remain focusable but reject mutation. Button and Label ignore read-only and
  validation; the menu bar ignores read-only too — it holds no value to
  protect — while becoming disabled or hidden closes any menu it had open.
- **Interaction, down.** The root sends `FocusGained { keyboard }` /
  `FocusLost` and `HoverGained` / `HoverLost`. Hover comes from explicit edges,
  not from a child inferring absence from raw motion. Pressed and hover select
  an exclusive fill (Disabled → Pressed → Hover → Normal); focus and validation
  are separate outlines, so both remain visible together. `keyboard` says how
  focus arrived, which is what decides whether a ring is drawn at all — see
  [the focus-visible rule](#the-focus-ring-marks-keyboard-focus).
- **Value, up.** `SliderChanged { value, committed }`, `TextCommitted { text }`,
  `RadioSelected { index }`, `VirtualListSelected { selected_index }`, and
  `ButtonClicked` flow to the parent through `ctx.parent()`. A slider streams
  `committed: false` values through a drag and a final `committed: true` on
  release, so a consumer previews the drag and commits the expensive work
  once.

  `ToggleChanged { on }`, `SegmentedSelected { index }`, `TabSelected
  { index }`, `MenuItemActivated { menu, item }`, and `NumericChanged { value,
  committed }` use that same source-attributed lane; Numeric applies the
  preview/commit distinction to typed values.

Every stock widget is `#[actor(instanced, composable)]`, so it satisfies
`ChildOf<P>` for any Wasm actor parent in the same resident module. A custom
widget intended for one panel can instead declare
`#[actor(instanced, child_of(MyPanel))]`. Typed inline spawn names both types
and verifies the ctx is actually running `MyPanel` before allocating the child
alias; data-driven by-tag composition enforces the same cardinality and
placement facts at runtime.

## Labels, type roles, and the selection role

`WidgetKind::Label` spawns the non-interactive `LabelWidget` from
`LabelConfig { text, role, align, theme, state }`. The label is where the type
scale reaches the screen: it draws at `theme.text_size_pixels(role)`, and a
`Caption` additionally inks with `text_muted` — a hint, a unit, or an
empty-state line is quieter than body text by construction, not because a
caller remembered to dim it. `Body` is the default and is what every label drew
before roles existed.

`align` places the run in the assigned frame: `Start` at the frame's left edge,
`Center` on its width, `End` flush with its right edge — which is what a column
of numbers wants, so magnitudes line up on their last digit. Every label drives
the same single-flight `FontMetricsRequest` the text field and text area do and
lays out against the resolved `CachedFontMetrics`. Until those metrics arrive
the label draws at the start rather than at a guessed width. A run wider than
its frame also stays flush left, so overflow clips at the slot's right edge
instead of pushing the head of the string out of view.

Clipped text is readable on hover. When a label's or a text field's run is
wider than the frame it lives in and the pointer is over it, the widget raises
the whole run on an overlay plate: `surface_raised` with a one-pixel `outline`
ring, starting at the widget's own origin — so it covers its own slot and
whatever sits to its right, and the root's overlay cutout keeps the covered
widgets' glyphs from printing through it. Nothing is raised while the run fits,
so the plate is a signal rather than chrome. This is why a label is
pointer-eligible (for hover) while remaining non-focusable: pressing one still
clears focus, exactly as pressing bare panel background does.

The plate is a box, not a line. A run long enough to need revealing is usually
long enough to run off the window if it is drawn in one line, so the plate
wraps at a reading measure — `reveal_wrap_width(size_pixels)`, about
`REVEAL_WRAP_CHARS` (40) body characters — and is then sized to its *longest
wrapped line* plus one `pad` either side, one row per line. Past roughly that
measure the eye loses the line it is on coming back from the right edge, which
is the same reason prose is set in columns.

The wrapper is public, so a consumer building its own tooltip or hint plate
gets the same shape without copying the rule:

```rust
use aether_kit_widget::set::{reveal_wrap_width, wrap_to_width};

// `measure` is yours: exact glyph advances from a resolved `CachedFontMetrics`
// once the font settles, an approximation before that.
let lines = wrap_to_width(hint, reveal_wrap_width(size_pixels), measure);
```

It breaks only between words. A word wider than the measure keeps its own line,
unsplit and over budget — the measure is a reading preference and cutting a
word in half to honour it reads far worse than one long line. A `\n` in the
source is the author's own break and always survives, so a longer hint can be
divided into paragraphs deliberately.

Selection is a state, not an affordance, and every widget that has a current
item draws it the same way: the chosen row of a virtual list, the chosen bucket
of a segmented control, and the marker of a radio group's chosen option fill
with `theme.selection`, and the text on them inks with `theme.selection_text`.
A radio group's unselected markers stay on `surface_raised` and so read as
empty slots beside the lit one. None of these use `accent`, which means the
primary action and the focus ring and nothing else — so a chosen row never
reads as a button waiting to be pressed.

## Placing one line of text

Every widget that draws a single line — label, button, text field, numeric,
list row, segmented bucket, tab, dropdown row — places it with one shared
rule, `aether_kit_widget::set::text_origin_y(row_top, row_height,
size_pixels)`. Reach for it rather than deriving an origin at the draw site;
per-widget arithmetic is how the set drifted out of alignment once already.

The rule exists because `aether.text` places a `Screen` draw's `origin` at the
*pen*, not at the ink: the baseline lands one **ascent** below the origin, and
an ascent is not the draw size. The kit's font (RobotoMono) has an ascent of
`2146 / 2048` em, so an origin computed as though the line were `size_pixels`
tall sank every glyph about a fifth of the size below centre — a visible sag in
a 24-pixel row, and the reason text on buttons and inputs read as sitting low
and to the right. `text_origin_y` instead centres the **cap box**, which is
what a reader sees as the text: the baseline sits half a cap height below the
row's middle (`text_baseline_y`, which the composition underlines also draw
on), and the origin is that baseline minus one ascent.

Horizontal centring is separate and needs the run's measured width, which is
the sum of its glyph advances at the size the draw uses: `centered_text_x`
places it and never pushes the run left of one `pad`. A widget that has not
resolved its font's metrics yet draws left-padded rather than guessing, so the
label never jumps when the measurement lands.

## Fixed-row virtual lists

`VirtualListConfig { items, initial_selected_index, visible_row_count,
empty_text, theme, state }` retains the complete string vector while realizing
at most `visible_row_count` rows. The panel fixes the slot height at
`theme.row_height * visible_row_count` and clips the slot to that viewport;
an empty item vector or zero-row viewport is not pointer- or focus-eligible.
This bounded realization is the intended path for hundreds or thousands of
uniform-height choices.

Up and Down move selection by one. PageUp and PageDown move by the configured
nonzero visible-row count. Movement clamps at the item-vector ends and shifts
the realized window only enough to reveal the selected row. A row is always one
configured row tall — the viewport divided by `visible_row_count`, never by the
number of rows the list happens to hold — so a list with fewer items than its
viewport draws that many normal rows at the top and leaves the rest of the
frame empty, and hit testing below the last realized row selects nothing. (A
list that spread its items to fill the frame instead turned a two-item list
into a pair of slabs, with a selected row half the viewport high.) Hidden lists
answer every `Collect` with an empty draw list; disabled and read-only lists
reject both pointer and keyboard selection changes.

`initial_selected_index` is an `Option`, and a list whose model holds no
current item shows none — no row lights up, rather than the first row lighting
as if it had been chosen. The selected row, when there is one, fills with
`theme.selection` over `theme.selection_text`. A list with no items at all
draws `empty_text` as a single caption-role, muted line at the top of its
viewport (`"No saved builds"`), so an empty result says so instead of leaving a
blank rectangle the reader has to interpret; an empty `empty_text` draws
nothing.

## Dropdowns

`DropdownConfig { options, initial_selected_index, placeholder, open_row_count,
theme, state }` is the control for one current choice whose alternatives are
secondary. Closed it is a single row reading the chosen option — or
`placeholder` in muted ink while nothing is chosen — with a chevron at its
right end. A press-and-release inside the row opens the list, as does Enter or
a matching Space release while focused; Escape closes it.

Open, the list hangs directly below the closed row: up to `open_row_count` rows
of `theme.row_height`, the frame's full width, on a raised surface inside a
one-pixel outline ring. The current option is drawn in the **selection** role
(`theme.selection` / `selection_text`), never the accent — a chosen thing is a
state, not a button. The option under the pointer takes the hover overlay, and
Up/Down walk that highlight by the keyboard, scrolling the realized window only
enough to keep it visible, exactly as a virtual list reveals its selection. A
longer `options` vector than `open_row_count` therefore scrolls inside the
realized rows rather than growing the list.

While the list is open every left press is the dropdown's: a press on a row
takes that option and closes, a press anywhere else closes without a change.
`DropdownSelected { index }` reports only an actual change of choice, and
`DropdownOpenChanged { open }` reports each open and close edge exactly once —
including the close that focus loss, a re-sent config, or becoming disabled or
read-only forces. An empty option vector, a zero-row list, and a read-only or
unavailable dropdown never open at all.

## Menu bars

`MenuBarConfig { menus, theme, state }` is the row of application menus a
screen's commands live in — File, Edit, View, Help — so a verb that is not a
control on the pane still has an address a person can find. Each `Menu` is a
`title` and its `items`, and each `MenuItem` is a `label`, the `shortcut` it
advertises (`"Cmd+S"`, or empty), whether it is `enabled`, and whether a
divider follows it (`separator_after`).

Closed, the bar is one row on `theme.surface_raised`. Each title is as wide as
its own text plus one `theme.pad` either side — never an even split of the row
— laid out left to right from the bar's local origin with `theme.space(1)`
between them, so the space between two titles belongs to neither. Like the tab
strip, that sizing needs real glyph widths, so the bar drives the same
single-flight font-metrics request the text controls do and splits the row
evenly only as an interim, for the frame or two before the measurement lands.
The title under the pointer takes the hover overlay, and so does the title
whose menu is open, so the bar says which menu the plate below it belongs to.

A press and release on a title opens that menu. The items are drawn in the
**overlay** (`WidgetDrawList::overlay`) as a plate hanging directly below the
title: `theme.surface_raised` inside a one-pixel `theme.outline` ring, as wide
as the widest item's label plus accelerator plus padding and never narrower
than the title it hangs from, one `theme.row_height` per item. An item's label
sits at `pad` in `theme.text_primary` — `text_muted` when it is disabled — and
its `shortcut` is right-aligned in `text_muted`; honouring the accelerator
itself is the root's business, the bar only advertises it. An item with
`separator_after` is followed by a one-pixel `theme.outline` divider with
`theme.space(1)` either side, and that band belongs to no item: a press there
selects nothing, exactly as the gap between two titles does. A trailing
`separator_after` on the last item draws nothing.

While a menu is open the bar holds the pointer grab, so it sees every move and
every press on the window. Moving over a different title opens that menu
instead, and Left/Right do the same by keyboard, clamping at the ends. The item
under the pointer takes the hover overlay; Up/Down walk that highlight over the
enabled items only, skipping the disabled ones and clamping at the ends, and
Enter activates the highlighted item. A press on an enabled item reports
`MenuItemActivated { menu, item }` and closes; a press on a disabled item does
nothing at all — not even close — so a mis-aimed press leaves the menu standing
to try again. A press anywhere else, a title included, closes without
activating, which is what makes pressing the open title read as the toggle it
looks like.

`MenuOpenChanged { open }` reports each open and close edge exactly once —
including the close that Escape, focus loss, a re-sent config, or becoming
unavailable forces, and *excluding* a switch from one menu to another, which is
not a new open edge and does not disturb the grab the root already holds. A
menu with no items never opens: an empty plate under a pointer grab is a trap,
not a menu.

## Tooltips

`TooltipConfig { sections, max_width_pixels, side, bounds, theme, state }`
spawns `TooltipWidget` — the anchored plate that says what the thing under the
pointer *is*. The widget's assigned `WidgetFrame` is the **anchor**: a host
points a tooltip at a row by handing it that row's rectangle, and the plate
stands beside it, in the widget's overlay so the rows under it stay readable.

The plate is measured, not padded to a grid. Every line wraps at
`max_width_pixels` (`0` takes `reveal_wrap_width` at the caption size — the
same reading measure the hover reveal uses), the box is exactly as wide as its
longest wrapped line plus one spacing unit either side and exactly as tall as
the line boxes it holds, and a `TooltipSection` boundary is a one-pixel
`outline` rule with a spacing unit either side rather than a blank line. An
empty section draws nothing at all, rule included, so a section a host had no
words for cannot leave a divider hanging.

The first line of the first section is the **name** of the thing being
explained and takes `TextRole::Body` in `text_primary`; every line after it is
`TextRole::Caption` in `text_muted`. `TextRole::Title` is deliberately not
used — that is the size a screen's one title is set at, and a 22-pixel line on
a hover plate is a headline.

`side` is the side of the anchor the plate prefers and `bounds` is the region
it must stay inside, in the same window pixels the frame is assigned in. A
plate that would run past those bounds **flips to the other side of the
anchor** (`set::place_plate`), then clamps on the cross axis — so a tooltip on
the last row of a pane stands above that row instead of half off the pane, and
never covers the row it is about. A widget cannot ask the window how big it is,
so the host that owns the region names it in the config.

Visibility is `WidgetControlState::visible`, the lane every stock widget
already has: the host flips it with `SetWidgetState` when its own dwell timer
says the pointer has rested long enough, and flips it back when the pointer
moves on. A tooltip with no sections likewise draws nothing. There is
deliberately no third `shown` field — the dwell, the row, and the words are all
the host's knowledge, and the widget takes the finished lines and nothing else.
The tooltip reports nothing up and is neither pointer- nor focus-eligible: a
plate that took hover would steal it from the row it explains.

## The toast region

`ToastConfig { max_standing, lifetime_frames, theme, state }` spawns
`ToastWidget` — the one place a refusal or a confirmation appears. Anything
mails it `ToastNotice { severity, text }`, so a save result, a planner refusal,
and a confirmation all arrive through the same door and land in the same place
the reader learned once.

The widget's frame is the region. Notices stack down from its top edge at its
width, newest first, up to `max_standing` (the oldest leaves to make room), and
each one is a `surface_raised` plate inside a hairline ring with a **severity
bar** down its left edge: `theme.info` (a blue-grey report), `theme.warning`
(orange), or `theme.error` (red), never the accent. The text wraps at the
region's width with one spacing unit of padding and the plate grows downward —
a notice is never elided, because a cut-off refusal says less than nothing.

A widget never sees a tick, so a notice's life is counted in **frames the root
asked it to draw**: `lifetime_frames` `Collect`s (240 is four seconds at sixty
a second), `0` meaning only the cap removes a notice. Ageing runs before the
draw and before the hidden-widget branch, so a hidden region still runs its
clock down instead of saving up a stack of stale refusals; a region that
becomes unavailable drops what it was holding.

`ToastRegionChanged { standing, height_pixels }` reports the edge — one
arrived, one aged out, the cap pushed one off — and never every frame. The
height is how far down the region the stack reaches, which is the rectangle a
host passes to whatever else is drawing under the notices (a tree view being
told what is covered) without re-deriving the geometry. The plates draw in the
overlay, so within the cluster the root's clip subtraction already keeps the
glyphs under them from printing through.

## Splitters

`SplitterConfig { axis, min_pixels, max_pixels, position_pixels, inverted,
theme, state }` spawns `SplitterWidget` — the drag handle on the edge between
two regions. It owns one scalar: the pane width, console height, or plate side
the host resizes with, held between `min_pixels` and `max_pixels`.

`SplitterAxis` says which motion moves it. `Horizontal` is a vertical edge
dragged left and right (the docked pane), `Vertical` a horizontal edge dragged
up and down, and `Corner` one side length of a square plate dragged by its
corner — the **mean of both axes**, because a square invites a diagonal drag
and taking one axis alone makes half of every drag do nothing. `inverted`
flips the direction for a region anchored to the far edge: a plate pinned to
the bottom-right grows as its top-left corner is pulled up and left.

The position follows the pointer's **travel** from where the press landed, not
its absolute position, so grabbing the strip anywhere along its width does not
jump the split. `SplitterMoved { position_pixels }` streams the clamped value
while the drag is live and goes quiet at either end of the range rather than
re-sending the same number every frame; there is no preview/commit split,
because a region resize is applied as it happens.

The widget's frame is the **hit strip** and can be as generous as the host
likes; the mark drawn in it is two logical pixels (half a spacing unit, so it
scales with `Theme::scaled`) of `theme.accent`, lit only while the pointer is
on the strip or a drag is live. `Corner` lights the two edges the drag pulls,
as an L. Nothing is drawn at rest: a resizable edge is a signifier that appears
when it is relevant, not a permanent rule down the screen.

`SplitterHover { entered }` reports the pointer crossing the strip so the
**host** can mail `aether.window.set_cursor`. The widget never touches the
window cap, and the decision is genuinely the host's: a resize cursor belongs
on a splitter whose affordance is hidden, and is intrusive over a view whose
gesture everyone already knows.

The drag asks for no new pointer routing. A left press on a pointer-eligible
child already gives that child the root's drag capture, which lasts exactly as
long as the button is held — the life of a resize gesture. (The modal grab an
open dropdown holds is the wrong tool: it outranks capture and persists across
releases, so it would have to be handed back for a gesture that is over when
the button comes up.)

## Popovers

A popover is a plate hosting *other* children over the primary view, dismissed
by a press outside it or by Escape. It ships as the `set::popover` module and
the plain `Popover` value a root owns beside its `Focus`, **not** as a widget —
because hosting interactive children is a root's job in this kit. Pointer and
keyboard routing, hit rectangles, focus traversal, and drag capture all live in
the root's `Focus` table over the root's own direct children; `ScrollWidget`
re-frames and re-composites its content and keeps only a wheel hit table, and
the compositing `Widget` node routes no input at all. A widget that owned its
children's input would be a second input root inside a widget, which is what
`EditorShell` is for one level up.

What two screens' popovers actually share is three decisions, and those are
what the module holds:

- **Where the plate stands.** `Popover::open(plate)` takes a rectangle the host
  has already chosen; `Popover::open_beside(anchor, width, height, side, gap,
  bounds)` places it with the same flip-and-clamp rule the tooltip uses and
  returns the plate it took. `Popover::plate()` is where the root frames the
  popover's children.
- **What it looks like.** `Popover::plate_items(&theme)` is a `surface_raised`
  fill inside a one-pixel `outline` ring — the plate a dropdown's list and a
  menu's items already wear, so a reader learns one "this stands over the
  screen" look. Put those items in the root's **overlay**, never its chrome:
  chrome flattens before the children, which is the wrong end for something
  that stands over them, and the overlay is what the root's clip subtraction
  cuts the covered text out from under.
- **When it goes away.** `Popover::press(x, y)` dismisses on a press outside
  the plate and reports `true` so the root consumes that press instead of also
  delivering it to whatever was under it; a press on the plate reports `false`
  and routes to the popover's children as usual. `Popover::key(code)` does the
  same for Escape and claims nothing else, so the focused child keeps its
  typing.

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

## Toggle, segmented, tab strip, and numeric controls

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
have no hit buckets. `SegmentedSelected { index }` reports only actual changes.
The selected bucket fills with `theme.selection` over `theme.selection_text`;
hovered, pressed, disabled, validation, and focus presentation use the common
theme/state contract.

`WidgetKind::TabStrip` spawns `TabStripWidget` from `TabStripConfig { labels,
initial_index, theme, state }` — one row of tabs over parallel content sets
viewed one at a time. Unlike the segmented control, the row is not divided
evenly: each tab is as wide as its own label plus one `theme.pad` either side,
laid out left to right from the strip's local origin with `theme.space(1)`
between them, so the space between two tabs belongs to neither and a press
there selects nothing. That sizing needs the label's real width, so the strip
drives the same single-flight font-metrics request the text controls do and
splits the row evenly only as an interim, for the frame or two before the
measurement lands. The hit buckets and the draw read the same widths, so a
press always lands in the tab under the pointer. The selected tab is marked by a
two-pixel `theme.text_primary` underline along its bottom edge and nothing
else: every tab keeps the row's own `surface_raised` fill and `text_primary`
ink, so the strip reads as a row of places with one marked rather than a row of
buttons with one lit, and hover and press stay the only fills the pointer
changes (the usual `Theme::fill` overlays). The tab strip is the one current-item
control that does not take the selection role — a segmented control divides one
bar and needs the fill to say which part is chosen, while a tab already reads as
a place you are standing in. A left press selects, focused
Left/Right moves the selection and clamps at the ends, and `TabSelected {
index }` reports only actual changes. The strip owns nothing but the choice:
swapping the content behind the selected tab is the root's business.

`WidgetKind::Numeric` spawns `NumericWidget` from `NumericConfig { min, max,
step, initial, theme, state }`. It reuses the shared `TextEditState`, named
`TextSpan`, and measured `SingleLineLayout`, so pointer placement, dragging,
selection, replacement, and clipboard edits stay on UTF-8 boundaries. Typed
characters arrive only through `TextInput`; `Key` is navigation and control —
Enter, Up/Down, and everything in
[the shared editing vocabulary](#the-editing-vocabulary-every-text-control-shares).

A numeric also carries **steppers**, because a value with a step has an obvious
pointer gesture and asking for the keyboard to use it is asking a person to
find out that Up/Down work. Two stacked buttons sit at the right end in a
square column one row height wide: up above, down below, each an arrow drawn
from quad rows rather than a glyph, because the theme's font is whatever the
consumer loaded and a missing-glyph box on a control whose whole point is being
clickable is the worst place for one. A press steps by `step` through the same
clamp, snap, and commit path Up/Down use, so the two routes cannot drift; each
button carries its own hover and pressed overlay. The text box shrinks by the
column, and the column never takes more than half the frame — a numeric too
narrow for both stays a value rather than becoming two arrows. A read-only or
disabled numeric has no live stepper targets. Nothing in `NumericConfig`
changed: steppers are what a numeric *is*, not something to opt into.

Numeric keeps the visible buffer separate from its last committed number.
Empty, `-`, `.`, and other invalid or non-finite intermediates remain visible
and emit nothing. A finite edit is clamped and snapped for a
`NumericChanged { committed: false }` preview without rewriting what the user
typed. Enter or focus loss canonicalizes a valid value and emits
`committed: true`; an invalid buffer reverts to the last canonical value
without an event. Up/Down and the steppers step from the current valid value,
falling back to the committed value, and immediately canonicalize and commit.
Copy never mutates, while cut and asynchronous paste pass through the same
selection, parse, clamp, and preview path.

## The editing vocabulary every text control shares

The text field, the text area, and the numeric editor read the same keys,
because a person who learns one of them has learned all three. `edit_command`
resolves a `Key` press plus the cached `Modifiers` into one `EditCommand`, and
`apply_edit_command` carries it out against `TextEditState`:

| Chord | What it does |
| --- | --- |
| `Ctrl`/`Cmd` + `A` `C` `X` `V` | Select all, copy, cut, paste |
| `Backspace` / `Delete` | Delete before / after the caret |
| `Left` / `Right` | One character, `Shift` to extend the selection |
| `Alt` or `Ctrl` + `Left`/`Right` | One word |
| `Cmd` + `Left`/`Right` | To the line's edge |
| `Home` / `End` | To the line's edge; with `Ctrl`/`Cmd`, the whole buffer |

**The chord modifier is `ctrl` *or* `meta`, always.** Cmd is the chord on
macOS and Ctrl everywhere else, and a widget cannot ask the substrate which
platform its window is on — the input cap reports the physical modifiers and
nothing more. Accepting either costs nothing here, because no control in the
set binds the two to different meanings; a control that ever needs to should
say so loudly rather than quietly diverging.

Enter is deliberately outside the table: it is the one key whose meaning is the
control's own — commit in a field, commit-on-`Ctrl`/`Cmd` and newline otherwise
in an area, commit in a numeric — so each widget handles it before consulting
the shared vocabulary. `Up`/`Down` are the same (vertical motion in an area,
stepping in a numeric).

**Nothing on this path suppresses a key repeat.** A held Backspace arrives as a
stream of `aether.key` presses and every one of them deletes. That is the
opposite of what a Button wants — `ActivationArms::press_key` ignores a repeat
while a key is armed, so a held Enter fires one click, not a hundred — and the
two rules must not be confused: repeat suppression belongs to activation, never
to editing.

`mutable` gates only the destructive half. A read-only or disabled control
still selects, copies, and moves its caret; it just cannot delete, cut, or
paste. A read-only field a person cannot copy out of is worse than useless.

## Root-owned focus and input

Widgets never subscribe to input. The panel root subscribes the pointer and
keyboard streams once (the input cap) and the frame stage once (the lifecycle
cap), then routes every event through a `Focus` helper it embeds — the
input-side counterpart to `Composite`. `Focus` holds child hit rects in layout
order, static pointer/focus eligibility, dynamic visible/enabled availability,
the hovered and focused children, and the drag-captured child. It answers four
questions:

- **Where does a pointer event go?** To the child holding the modal grab if
  there is one, then to the drag-captured child if one holds capture — so a
  drag that leaves a widget's rect still reaches it — otherwise to the topmost
  child under the cursor.
- **Where does a keyboard event go?** To the focused child.
- **Where does hover live?** Independent hit testing yields a named
  `HoverTransition`; sibling crossings send lost before gained even while
  capture routes raw drag motion elsewhere.
- **What moves focus?** A left press on a focusable child, Tab forward, or
  Shift+Tab backward. Traversal wraps and skips hidden/disabled/static entries.
  A live availability change moves focus forward when its holder disappears
  and clears hover/capture through named transition effects.
- **What clears focus?** A left press that lands on *no* focusable child —
  bare panel background, a label, the gap between two rows. Clicking away from
  a control is how a person says they are done with it, so the field they were
  typing in stops being active and takes its `FocusLost`.

That last rule is the one a consumer root has to copy deliberately, because it
is the branch that is easy to leave out: `Focus::focus_hit` answers `None` both
when the press hit nothing focusable *and* when it hit the already-focused
child, so a root that only reacts to its `Some` never clears anything and a
pressed input stays lit forever. Ask `Focus::focus_hit_test(x, y)` for the
focusable child under the point and hand the answer — `Some` or `None` — to
`Focus::set_focus`, then fan the returned transition:

```rust
let focusable = self.focus.focus_hit_test(press.x, press.y);
if let Some(transition) = self.focus.set_focus(focusable) {
    // FocusLost to `previous`, FocusGained to `next`. `false`: this focus came
    // from a press, so the child it lands on must not draw a ring.
    apply_focus(ctx, transition, false);
}
```

Clearing focus cancels nothing else: drag capture, the modal grab, and every
child's own value are untouched.

### The focus ring marks keyboard focus

`FocusGained` carries a `keyboard` flag, and a child draws its focus ring only
when that flag was `true`. The reference panel passes `true` from Tab
traversal and `false` from a pointer press or from an availability move that
had to relocate focus; a consumer root copies the same two answers.

The rule is what the ring is *for*. A person walking a panel with Tab has no
other way to tell which control the next keystroke reaches, so the ring is the
only thing standing between them and typing into the wrong field. A person who
just clicked a control already knows where they are — they pointed at it — so a
box drawn around it afterwards adds a mark that says nothing and reads as a
stuck highlight, which is exactly how a clicked tab looked before this rule
existed.

Focus itself is unchanged by the flag: a pressed control is still focused, still
receives the keyboard, and still takes `FocusLost` when focus moves on.
`InteractionState` keeps the two apart — `focused()` answers what focus *means*
and `focus_visible()` answers whether to *mark* it — and the split matters most
in the text controls, where the **caret follows `focused()`**. A caret marks the
insertion point, which a click establishes exactly as a Tab does, so a clicked
field shows its caret and no ring. Only the ring is keyboard-conditional:
`push_control_outlines` and the Button's own border are the whole list of draws
that consult `focus_visible()`.

Drag capture is the kit's own policy over the raw button vocabulary: a left
press that hits a widget sets capture on that child, moves route to it while
capture holds, and the matching release clears it. That is what lets a slider
track the cursor past the end of its track and a button cancel when the release
drifts off it. `MouseButton.button` and `MouseButtonRelease.button` use the
engine constants in `aether_kinds::mouse_button`; `LEFT` is `0`, including for a
synthetic `aether.mouse_button` sent over MCP.

A widget that must draw outside its own slot — an open dropdown's list, a menu
bar's plate — puts those draws in `WidgetDrawList::overlay` instead of
`items`. The overlay is
offset by each slot origin on the way up like any other draw but never
intersected with the slot clip, and the root emits the whole cluster's overlay
after every ordinary quad and glyph, so the list escapes its one-row slot and
lands over the widgets below it. Its counterpart on the input side is the
**modal pointer grab**: `Focus::begin_grab(child)` routes every pointer event
to that child until `end_grab`, outranking drag capture, and hover edges are
suppressed while it holds so nothing under the overlay lights up. The widget
asks for the grab by reporting `DropdownOpenChanged { open: true }` — or
`MenuOpenChanged { open: true }`, which the root answers the same way — and
gives it back with `open: false`; that is why the close edge is reported for
every way a list or a menu can close, including focus loss. Without the grab a
press that lands outside the widget's own rect would go to whatever is under
it, and the open list would have no way to learn it should close.

A focused Button activates once on Enter press (repeat presses are suppressed
until release) and once on Space release after a matching Space press. Focus
loss or unavailability cancels the keyboard arm. Its label sits centered in the
frame on both axes once the configured font's metrics resolve; until then it
draws left-padded, because a guessed width would center the label wrong and
then visibly jump when the real one arrived. That measurement also gives the
button its `WidgetDrawList::intrinsic` — `[label width + 2 × pad, row height]`
— so a layout can size a slot to the label it holds; like the image widget's
natural size, the reference panel does not yet consume it.

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

(The shelved `aether-kit-workbench` terrain annotation workbench was the
concrete peer-first assembly of this model — a specialized tool panel, a
camera-owning viewport, a non-input-owning `ConsoleOverlay`, and the one
`EditorShell` routing their three non-overlapping regions; git history holds
it.)

## Layout

A panel root hands each child a `WidgetFrame { x, y, width, height }` and
nothing else, so every consumer that laid out a screen used to compute those
rectangles by hand — a few hundred lines of `x + pad`, `width - 2.0 * pad`,
`y += 28.0` per screen, with the design decision buried inside the arithmetic
and no single place where the pane's width and the viewport's width agree. The
`layout` module is that arithmetic named. It is pure `f32` geometry over
`WidgetFrame` — no actor, no mail — so a consumer computes a whole screen's
frames in one function, asserts them in a unit test, and only then mails them
down.

Use it in the order a screen is actually designed. **Regions first**: `dock(window,
DockSide::Right, 320.0)` returns a `Docked { pane, viewport }` whose two
rectangles tile the window exactly. A tool pane belongs beside the thing it
operates on, not floating over it — an overlay hides the content the controls
act on, and the viewport can no longer be sized honestly. `pane_extent` is
clamped into the window, so an oversized pane leaves a zero-width viewport
rather than a negative one. Reach for `inset(frame, theme.pad)` to put the
breathing room inside a region once, instead of in each child's own maths.

**Rows next.** A `Column { origin, width, gap }` stacks `Row`s down a region
and `place(&rows)` returns a `Placed { frames, height }` — one frame per cell
in row-then-cell order, plus the total occupied height for stacking a second
column below or sizing a scroll extent. The column owns one `gap`, used both
between rows and between the cells of a row; feed it from `Theme::space` and
every space on the screen is a whole number of spacing units, which is most of
what a reader perceives as "designed".

**Cells last, sized to content.** `Row::single(height)` is the full-width case
— a heading, a slider, a field that spans the pane. `Row::cells(height, cells)`
takes explicit `Cell`s: `Cell::Fixed(pixels)` for a control whose content
determines its width (a button at its measured label width, an icon, a
fixed-width numeric field) and `Cell::Share(weight)` for the one thing that
should absorb what is left. Shares divide the width remaining *after* every
fixed cell and every gap is subtracted, so adding a button to a row narrows its
text field by exactly the button plus one gap with no second place to keep in
agreement. A row of three `Fixed` buttons leaves its remainder empty at the
right — deliberately, because equal thirds size a control to its container,
which stretches "OK" to a third of the pane and clips "Regenerate terrain" in
the same row.

Degenerate input clamps rather than propagating: a negative or NaN length
becomes zero and a NaN position becomes zero, so a layout computed before the
window size is known collapses to empty rectangles instead of poisoning every
frame downstream with NaN.

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
