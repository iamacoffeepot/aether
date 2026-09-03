# The widget set & focus model

`aether-kit-widget` ships a set of guest-side widgets — a slider, a text field, a
multiline text area, a radio group, a fixed-row virtual list, a button, a label,
an image, a toggle, a segmented control, a tab strip, a dropdown, a menu bar,
a numeric editor, a tooltip, a toast region, a dialog plate, and a splitter —
as ordinary
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

## Buttons, emphasis, and tone

`WidgetKind::Button` spawns `ButtonWidget` from `ButtonConfig { label,
emphasis, tone, theme, state }`. A left press inside arms it and the matching
release inside fires `ButtonClicked`; a release that drifts off cancels.
Enter fires on its press, Space on its matching release.

`emphasis` is `ButtonEmphasis { Filled, Tonal, Outlined, Text }` and ranks how
loudly the verb asks to be pressed — Material 3's ladder, loudest first. It
defaults to `Filled`, which is the accent plate every button drew before the
field existed, so an existing consumer keeps its look. `tone` is `ButtonTone {
Neutral, Danger }` and says what the verb does to the reader's work; it
defaults to `Neutral`. The pair resolves to three inks:

| `emphasis` | Plate | Stroke | Label |
|---|---|---|---|
| `Filled` | the tone's role (`accent`, or `error` for `Danger`) | — | `accent_text` |
| `Tonal` | `Theme::tonal(role)` — `surface_raised` carried a fifth of the way toward the role | — | `text_primary`, or `error` for `Danger` |
| `Outlined` | — (a hover wash) | `outline`, or `error` for `Danger` | `text_primary`, or `error` for `Danger` |
| `Text` | — (a hover wash) | — | `text_primary`, or `error` for `Danger` |

`Theme::tonal(role)` is derived rather than stored, so a theme that moves its
accent moves every tonal plate with it, and it is deliberately not
`selection`: a chosen row and a secondary verb must not share a look. A
neutral verb at the quiet ranks reads in the primary ink rather than the
accent, because the accent means *the* primary action — a screen that letters
four secondary verbs in it has spent the token again. The filled ranks carry
hover and press in their plate through `Theme::fill`; the plateless ranks have
no plate to carry it, so the same role-agnostic `hover_overlay` /
`pressed_overlay` is drawn as the whole background instead.

Everything else is identical at every step of the ladder: the label is
measured, centered, and elided the same way, the reported
`WidgetDrawList::intrinsic` is the same label plus one `theme.pad` either
side, and the hit rectangle is the whole frame. A quieter button is a quieter
look, never a smaller target — a host may rank a verb down without its row
moving.

The rule for *which* rank a verb takes is
[the screen-design method](../building/designing-a-screen.md): one filled verb
per region, secondary verbs tonal or outlined, a verb that throws work away
outlined in the error colour, and a dialog's row is one filled confirm beside
a text cancel.

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

A measured run wider than its frame is **elided** — the same `elide_to_width`
cut with the same `ELLIPSIS` the list's rows and a button's label take, against
the whole frame since a label reserves no pad. The slot clip stays the backstop
and still bounds an unmeasured run, but a clip cuts mid-glyph: a stat row in a
column narrower than its words read `Physical damage mitigat`, which reads as a
name that ends oddly rather than as a name that was too long. This is a safety
net rather than a layout — a column sized from the intrinsic below never
reaches it, the reported width stays the *whole* run's, and the hover reveal
still carries the whole text — so what it changes is only what a column too
narrow for its words looks like.

The label reports that measured run as its `WidgetDrawList::intrinsic` —
`[measured width, theme.row_height]`, with no pad either side because a label
reserves none — so a layout can size a column to the words it holds instead of
to a share of the row. Like the button's, it is `None` until the theme font's
metrics resolve; a slot sized from a guess would resize the moment the real
advances arrived.

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
places it so the margins either side are equal at every frame width. A widget
that has not resolved its font's metrics yet draws left-padded rather than
guessing, so the label never jumps when the measurement lands.

**A pad is what the intrinsic reserves, not room the draw must leave.** Both
halves of placing a run have learned this the hard way. `centered_text_x` used
to clamp the origin to one `pad`, which made the margins 8 and 2 on a frame a
few pixels under the intrinsic width — the asymmetric Remove button. A button
then elided against the frame *less* a pad each side, which on a one-glyph
control sized to its own mark left room for neither the mark nor the ellipsis
that would say it was cut, so the run came back empty and the ascendancy
inset's collapse button drew as an empty outlined square. A frame smaller than
the intrinsic is a caller saying there is no more room; the widget gives up its
pads, then elides, and only draws nothing when the frame cannot hold even the
ellipsis.

`set::text_cap_height(size_pixels)` is that cap band as a number, for a caller
placing something *beside* a run — an inline icon, a rule, a swatch — that has
to stand exactly as tall as the letters do.

**Never send a glyph the face lacks.** Nothing in the text path errors or skips
one: `aether.text` looks the character up in the font's cmap, gets glyph index
`0`, and rasterizes `.notdef` — in the vendored RobotoMono, a hollow box —
while the kit's measurement (`CachedFontMetrics::measure`, the same table every
widget sizes from) falls back to the `.notdef` advance. So an unsupported
character is silently a box of the right width in every box measured around it.
A host that wants `⌘` in a label ships a face that has it, or writes
`Command`.

## Fixed-row virtual lists

`VirtualListConfig { items, initial_selected_index, visible_row_count,
empty_text, ruled, theme, state }` retains the complete row vector while
realizing at most `visible_row_count` rows. The panel fixes the slot height at
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

The list measures its rows. Like the label and the tooltip it drives the
single-flight `FontMetricsRequest`, and once the advances land a row too long
for its frame is **elided** — cut on a character boundary with an ellipsis
(`set::elide_to_width`, `set::ELLIPSIS`) that fits inside the frame less one
`pad` at each end. Before the metrics arrive a row draws whole and the slot
clip bounds it as it always did; the clip stays the backstop either way, but it
cuts mid-glyph, which reads as a name that ends oddly rather than as a name
that was too long. The same helper is public, so a consumer cutting its own
row uses the kit's rule:

```rust
use aether_kit_widget::set::elide_to_width;

let shown = elide_to_width(name, column_width, |run| measure(run));
```

A list whose vector overflows its viewport draws a **scroll bar** down its
right edge: a `theme.outline` track two spacing units wide, and a
`theme.text_muted` thumb whose length is the visible share of the whole vector
and whose position is where the reader is. It is present whenever the list
overflows — never only on hover, because a bar that appears when touched cannot
answer "how many entries are there" for a reader who has not touched it — and
absent when the vector fits, where it would claim there is more to see. Past a
floor of one and a half track widths the thumb stops shrinking, so a list of
thousands still has something to grab, and only its travel goes on saying how
much is off screen.

Three things move it, and all three write the one `first_index` the list
already had: a keyboard reveal, the **wheel** (whole rows, with the sub-row
remainder carried so a trackpad's stream of small deltas still moves the list),
and **dragging the thumb** — a press on the thumb keeps the grabbed point under
the pointer, a press on the bare track carries the reader to where they
pointed. Scrolling never changes selection: a reader looking at something has
not chosen it. A press on the bar chooses no row.

The bar owns a **gutter** at the frame's right end — its track plus one
spacing unit of gap — and a row is laid out, filled, and elided inside what is
left, so the bar stands beside the rows rather than on them. (A row fill that
ran the whole frame width put the track on top of the row it marked, which is
what "the scrollbar has no padding with the inner content to the left so it
just draws over it" was.) The reported intrinsic counts the same gutter, so a
slot sized from it does not hand the bar back a gutter's worth of the text it
just asked for. A host drawing its own rows against a kit list's geometry
reserves the same width: `Theme::space(2)` of track plus `Theme::space(1)` of
gap, whenever the vector overflows the viewport. A
virtual list joins the same wheel-only hit table a `ScrollWidget` does (see
[Scroll containers and wheel ownership](#scroll-containers-and-wheel-ownership)),
so a root that forks the reference panel routes `MouseWheel` to it by
`Focus::hit_test` rather than by pointer capture.

Those metrics also give the list its `WidgetDrawList::intrinsic`: `[widest row
in the whole item vector + 2 × pad + the scroll bar's gutter when the vector
overflows, theme.row_height × visible_row_count]`. It
measures the items, not the realized window, so the width does not change as
the reader scrolls — and because that is the one thing here that touches every
item, it is measured once and re-measured only when the items, the font, or the
type scale change. It is `None` until the metrics resolve and for a list with
no rows.

### A row is two columns and a type step

An item is a `VirtualListRow { text, trailing, role }`, and a row that is only
words is written as one (`VirtualListRow: From<String> + From<&str>`), so
`(0..n).map(VirtualListRow::from)` is the whole of the plain case.

`trailing` is the row's **second column**: a version, a count, a price, a key.
It is set right-aligned against the row's own right pad, and the widest
trailing run among the **realized** rows decides the column every visible row
shares, with one spacing unit of clear space before it. The column is
subtracted from the leading budget *first*, so the two rules that follow are
one rule: the **leading run elides** into what is left (the same
`elide_to_width` cut, with the ellipsis), and the **trailing run never does** —
a name cut short still names the thing, while an amount cut to `21/…` is a
wrong number. The column is the realized window's rather than the whole
vector's because a reader compares what is on screen, and a column sized by an
off-screen row leaves a gap nothing stands in.

`role` sets the type step both runs are drawn at, defaulting to
`TextRole::Body`. A `Caption` row draws at the caption size in the muted ink,
exactly as a caption-role label does, so a list can carry a name and a detail
line without the host drawing its own rows. A selected row keeps
`theme.selection_text` whatever its role.

`ruled: true` puts a one-pixel `theme.outline` hairline between rows — `n - 1`
of them for `n` realized rows, never a rule under the last one (that underlines
the list) or above the first (that is a second top edge). It is off by default:
a list of *choices* is read down its fills, and rules on one are chrome. Turn
it on for a list of *entries*, where a reader has to see which trailing belongs
to which name.

The reported intrinsic counts both columns and the gap between them, so a slot
sized from it holds the whole row rather than only its name.

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

The dropdown reports an **intrinsic** like every other content-sized control:
`[widest run the closed row can hold + 2 × pad + the chevron column,
theme.row_height]`, where the chevron column is the mark (half the label size)
plus one spacing unit of clear space before it. The widest *run* is every
option **and the placeholder** — the placeholder is what the row reads before
anything is chosen, so a cell that fitted only the options would clip the one
thing the control says at rest — and it is the widest rather than the current
one, so choosing does not resize the cell under the reader. Like the label's
and the list's, it is `None` until the theme font's advances resolve (the
dropdown drives the same single-flight `FontMetricsRequest`) and is
re-reported when the options, the font, or the type scale change. Before it
existed a host had to give every dropdown a full-width row of its own, which is
right for a control that is its row's whole subject and wrong for a sort
control that belongs beside the field it orders.

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

`TooltipConfig { sections, max_width_pixels, max_height_pixels,
hanging_indent_pixels, side, avoid, bounds, theme, state }` spawns
`TooltipWidget` — the anchored plate that says what the thing under the
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

A **wrapped line** and a **new paragraph** are opposite cases, and the plate
keeps both rules (round-4 note 19 — "I'd prefer if it was aligned to the first
line of text and then new paragraphs just had a break (empty line)"). A line
that wrapped is one thought that ran out of measure, so its continuation rows
start flush with its first row: `hanging_indent_pixels` is `0` by default and
the kit's own plates leave it there. A new paragraph is a new thought, and
takes a **blank row**: a `TooltipLine` with no words in it draws one empty
line box, and so does a blank line inside one line's own text
(`"first\n\nsecond"`). A blank at the very top or bottom of the plate is
dropped — a break needs something on both sides of it — and a leading blank
never takes the title role from the first real line. Neither of these is a
section: a section boundary stays a **rule**, so a blank row divides two
paragraphs of one block while a rule divides two blocks.

A section's lines are `TooltipLine { text, role, ink, icon }`, every option
`None` for that rule. `TooltipSection::new(["Life", "Your health pool."])`
takes plain strings (`TooltipLine: From<String> + From<&str>`), so a host that
only has words writes only words. The escapes are for the distinctions a role
cannot carry, because a role carries one ink: which line of a card the reader's
search matched, or which stat is not being counted. Set `ink` on those lines
and leave the rest alone.

`icon` draws a mark **before** the line's words, for the things a reader
recognizes by colour and shape before they read the name at all — an instilled
gem, a rarity, a damage type. `TooltipIcon { texture_id, width_pixels,
height_pixels }` names a texture the **host** registered through
`aether.render.create_texture` (the widget draws it and never creates,
updates, or destroys one) plus that texture's own size, which is the aspect the
plate preserves. What it is drawn at is the line's own **cap band**
(`set::text_cap_height`), so a 64-pixel icon and a 16-pixel one stand exactly
as tall as the capitals beside them. The icon's footprint — itself plus one
spacing unit — comes out of that line's measure and goes into the plate's
width, so an icon makes a line **wrap earlier** rather than run past the box;
the continuation rows are inset to the words' own start, so a wrapped line
reads as one entry indented under its icon. It takes the first row only (one
thought, one icon), and an icon on a line with no words draws nothing, because
that line is a paragraph break.

A hover card over a canvas needs three more things, and they are the remaining
fields:

- **`avoid`** — rectangles the plate would rather not cover, in the anchor's
  window pixels. `place_plate_avoiding` tries all four sides under the usual
  flip-and-clamp and takes the one covering the least of them, with **the first
  entry outranking the rest**: a card gets clear of the thing it is about — the
  hovered node, which is what keeps it attached — before it weighs any other
  obstacle. Ties keep the preferred side, and an empty `avoid` is exactly
  `place_plate`. Nothing guarantees a clear placement; a plate bigger than the
  gaps covers something whichever side it takes, and the least-covering side is
  the honest answer.
- **`max_height_pixels`** — the tallest the plate may stand (`0` is no budget).
  Over it the plate **sheds**: it drops trailing whole entries — one source
  line, wrapped rows and all — until it fits, so a reader never gets a stat
  ending in "per". A section left with nothing takes its rule with it, and a
  budget nothing fits in sheds everything and draws no plate. The count goes up
  as `TooltipShed { dropped }` whenever it changes, so the host — the only one
  that knows what the dropped entries said — can word the tail or re-send a
  shorter card. Re-sending the config resets the count, so the next frame
  reports the new card's tail.
- **`hanging_indent_pixels`** — how far the continuation rows of a wrapped line
  are inset. `0` is the default and the flush block described above; the indent
  is the opt-in for the one case that wants it, a list of stats where an inset
  continuation makes a two-row stat read as one stat. Continuations wrap that
  much earlier, so the right edge does not move.
  `set::wrap_to_width_hanging` is the same rule as a public helper.

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
The tooltip reports no *value* up — `TooltipShed` is the one thing it says, and
it is about the plate, not about what the plate means — and it is neither
pointer- nor focus-eligible: a plate that took hover would steal it from the
row it explains.

## The toast region

`ToastConfig { max_standing, lifetime_frames, role, theme, state }` spawns
`ToastWidget` — the one place a refusal or a confirmation appears. Anything
mails it `ToastNotice { severity, text }`, so a save result, a planner refusal,
and a confirmation all arrive through the same door and land in the same place
the reader learned once.

The widget's frame is the region. Notices stack down from its top edge at its
width, newest first, up to `max_standing` (the oldest leaves to make room), and
each one is a `surface_raised` plate inside a hairline ring with a **severity
bar** down its left edge: `theme.info` (a blue-grey report), `theme.warning`
(orange), or `theme.error` (red), never the accent. The line starts
`theme.space(2)` clear of that bar — round-8 note 17, "toaster left text
padding can be increased a tad" — and the plate keeps one spacing unit of
padding at its other three edges; the bar already occupies the left edge, so
matching the right pad to the inset would push the line off-centre rather than
balance it. The inset is charged against the wrap measure, so the text wraps
at what is actually left of the region's width and the plate grows downward —
a notice is never elided, because a cut-off refusal says less than nothing.

`role` is the step of the theme's type scale a notice's line is set at
(`TextRole::Body` by default — the reading size the region drew at before the
field existed). "Toast text can be larger" (round-4 note 15) is a theme fact,
not a toast fact: the region names a role and the theme resolves the size, so
one field moves the line box, the wrap measure, and the reported stack height
together rather than leaving a larger line to overprint a body-sized box.

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

`SplitterConfig { axis, min_pixels, max_pixels, position_pixels, inverted, bare,
theme, state }` spawns `SplitterWidget` — the drag handle on the edge between
two regions. It owns one scalar: the pane width, console height, or plate side
the host resizes with, held between `min_pixels` and `max_pixels`.

`bare` (default `false`) drops the lit mark: an edge the reader already sees,
such as the border of a plate, is signalled by the pointer's resize shape
alone, and the handle still reports every hover and move.

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

A leave that lands **mid-drag is held** until the button comes up. The pointer
walks off a four-pixel strip within the first few pixels of every resize, so a
crossing reported there would flicker the host's resize cursor back while the
gesture is still running; one dropped instead would leave the cursor — and,
when the release lands elsewhere, the lit mark — stuck on. The widget defers
it, cancels it if the pointer comes back, and reports exactly one
`entered: false` when the drag ends off the strip, whether it ended on a
release or on a focus loss.

The drag asks for no new pointer routing. A left press on a pointer-eligible
child already gives that child the root's drag capture, which lasts exactly as
long as the button is held — the life of a resize gesture. (The modal grab an
open dropdown holds is the wrong tool: it outranks capture and persists across
releases, so it would have to be handed back for a gesture that is over when
the button comes up.)

It does ask one thing of the host: **do not respawn the strip mid-drag**. A
root that rebuilds its layout on every `SplitterMoved` — which is the ordinary
way to host a resizable pane — calls `Focus::clear` and re-registers on the
drag's first pixel, and that is fine, because `clear` drops the entries and
only the entries (see [Rebuilding the table under a live
gesture](#rebuilding-the-table-under-a-live-gesture)). What the capture cannot
survive is the child itself going away: a rebuild that despawns the splitter
and spawns a new one hands the drag a `MailboxId` that no longer exists, and
the resize dies with it. Spawn the strip once and re-frame it.

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
  screen" look. Put those items in the root's **overlay**
  (`Composite::extend_overlay`), never its chrome: chrome flattens before the
  children, which is the wrong end for something that stands over them, and
  the overlay is what the root's clip subtraction cuts the covered text out
  from under. Raise the popover's own children into that same lane with
  `Composite::set_slot_overlay(child, true)` while it is open — see [the
  overlay lane](#root-owned-focus-and-input) below.
- **When it goes away.** `Popover::press(x, y)` dismisses on a press outside
  the plate and reports `true` so the root consumes that press instead of also
  delivering it to whatever was under it; a press on the plate reports `false`
  and routes to the popover's children as usual. `Popover::key(code)` does the
  same for Escape and claims nothing else, so the focused child keeps its
  typing.

## Dialogs

`DialogConfig { title, min_width_pixels, min_height_pixels, theme, state }`
spawns `DialogWidget` — the plate a modal stands on. The widget's assigned
`WidgetFrame` **is** the plate's rectangle; the config only says what is
written on it and how small it may get.

The plate is a `surface_raised` fill inside a one-pixel `outline` ring — the
same plate a popover, a dropdown's list, and a menu's items wear — with a
title row and a hairline rule under it. Every band is derived from the type
scale and the spacing grid rather than from one row height: the plate is inset
two spacing units on every edge, the title is set at `TextRole::Heading` in the
primary ink, its row is the heading size plus one unit above and below, and the
rule takes a unit either side of the hairline (the band a tooltip's section
rules already occupy). An empty `title` draws no title row and no rule at all,
so a confirmation with nothing to name is a bare frame rather than a rule with
nothing above it.

`DialogPlaced { frame, body }` reports the geometry up, in the same window
pixels the frame was assigned in, **whenever it changes and never every
frame** — the host re-frames its children off this mail, and sending it every
collect is a relayout per tick. `body` is the rectangle inside the chrome: it
is where the host frames its own slot children, so they land under the title
rather than over it. `frame` is the plate *as drawn*, which is the assigned
frame grown to the minimum the title needs, so the host can hand it to its
peers as the rectangle they are occluded by.

The minimum is what keeps the title readable. The plate never goes narrower
than its measured title plus a pad each side, nor shorter than its own chrome,
and `min_width_pixels` / `min_height_pixels` raise either floor. The title's
floor arrives with the font's advances rather than being guessed from a
character count — before the metrics land the plate is exactly the frame it was
given, and nothing jumps.

The dialog **hosts no children and dismisses nothing**, for the reason the
popover is a module rather than a widget: input routing is the root's job. Its
children are the root's own widgets, framed inside `body`; light dismiss and
Escape are `Popover::press` and `Popover::key`, which the host already owns for
every other plate on the screen. Register the dialog's slot *before* the
children standing on it and raise them into the overlay lane with
`Composite::set_slot_overlay(child, true)`, so the plate arrives under its own
contents and over the screen it covers.

**Resizing** uses the handle the kit already has. Frame a `SplitterWidget` with
`bare: true` over the plate's right edge (`SplitterAxis::Horizontal`), another
over its bottom edge (`Vertical`), and a third over the bottom-right corner
(`Corner`), and re-frame the dialog on each `SplitterMoved`:

```rust
// The three strips, derived from the plate the dialog reported.
let grip = theme.space(2);
let right = WidgetFrame { x: plate.x + plate.width - grip, y: plate.y, width: grip, height: plate.height - grip };
let bottom = WidgetFrame { x: plate.x, y: plate.y + plate.height - grip, width: plate.width - grip, height: grip };
let corner = WidgetFrame { x: right.x, y: bottom.y, width: grip, height: grip };
```

`bare` is the point: the edge of a plate is something the reader can already
see, so the pointer's resize shape is the whole signal and a line lighting
under it is one more thing on the screen. The dialog clamps whatever size it is
handed and reports what it actually took, so a strip dragged past the minimum
moves nothing rather than cutting the title in half.

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
initial_index, style, theme, state }` — one row of tabs over parallel content
sets viewed one at a time. `style` is `TabStripStyle { Chips, Filled }` and
picks between the two shapes below; it defaults to `Chips`, which is what
every strip drew before the field existed. Selection is identical in both: a
left press selects, focused Left/Right moves and clamps, and `TabSelected {
index }` reports only actual changes.

**`Chips`** — content-sized tabs sitting in the section. Unlike the segmented
control, the row is not divided evenly: each tab is as wide as its own label plus one `theme.pad` either side,
laid out left to right from the strip's local origin with `theme.space(1)`
between them, so the space between two tabs belongs to neither and a press
there selects nothing. That sizing needs the label's real width, so the strip
drives the same single-flight font-metrics request the text controls do and
splits the row evenly only as an interim, for the frame or two before the
measurement lands. The hit buckets and the draw read the same widths, so a
press always lands in the tab under the pointer.

Those widths are then **fitted into the strip's own frame**, because a strip in
a resizable pane is regularly handed less room than its tabs ask for. The
shortfall is a water-fill, not a proportional scale: each tab takes the smaller
of what it asked for and an equal share of what is left, shortest first, so a
narrow tab keeps its natural width and only the wide ones give anything up. A
tab the fit shrank elides its label into the width it got, and every tab
centers its run in its own cell — so the padding either side of a label is
equal on every tab at every strip width, the last one included. Without the
fit the last tab alone ran off the frame's right edge for the root's slot clip
to cut, which is what the owner saw as `Search` having less space to the right
of its label than to its left. The strip reports the row it *wanted* as its
`WidgetDrawList::intrinsic` — `[every tab at its natural width + one
theme.space(1) between them, row height]` — so a layout that sizes the strip's
slot to it never triggers the fit at all; like the image widget's natural size,
the reference panel does not yet consume it. The selected tab is marked by a
two-pixel `theme.text_primary` underline along its bottom edge and nothing
else: every tab keeps the row's own `surface_raised` fill and `text_primary`
ink, so the strip reads as a row of places with one marked rather than a row of
buttons with one lit, and hover and press stay the only fills the pointer
changes (the usual `Theme::fill` overlays). The tab strip is the one current-item
control that does not take the selection role — a segmented control divides one
bar and needs the fill to say which part is chosen, while a tab already reads as
a place you are standing in.

**`Filled`** — Material 3's primary tabs, and the answer to "they don't feel
like typical tabs … buttons that take the space and feel more dominant"
(round-8 note 14). The tabs divide the strip's **whole frame** with nothing
between them, so every pixel of the bar belongs to a tab and a press in the
middle of the row always selects one. They divide it by what is *in* them,
not evenly: each tab keeps its own measured run plus one `theme.pad` either
side, and the leftover width is shared equally among all of them. Equal
*slack*, not equal width — a row with room for every label never cuts one,
which an even split does not give you (at the studio's own pane it put
`Build` in a share three times wider than the word and elided `Equipment` to
`Equipm…` beside it). Only when the runs do not fit at all does
`fit_row_widths`' water-fill shrink the widest tabs, and only then does a
label elide, so the first word to be cut is the longest one. The underline
and the hit buckets follow the resulting widths, whichever rule produced
them. No tab carries a plate; the
current one is marked by a two-pixel `theme.accent` underline **the width of
its own tab** at the strip's bottom edge, and a one-pixel `theme.outline` rule
runs under the whole strip, drawn first so the underline lights its own span
of it — the row reads as the top edge of the content it switches. The accent
is spent as a *mark* and never as a fill, so no tab is plated in the primary
action's colour. With no plate to carry the pointer's answer, a hovered or
pressed tab draws the role-agnostic `hover_overlay` / `pressed_overlay` as its
whole background. A share too narrow for its label elides it with the kit's
ellipsis and centres the run in the share, the same rule the fitted chips
follow.

A filled strip reports `WidgetDrawList::intrinsic` as `[non-finite, row
height]`: it takes whatever width it is given, so there is nothing for a
layout to size a slot to, and a reader of that field takes a component only
when it is finite and non-negative. The strip owns nothing but the choice:
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
find out that Up/Down work. They are part of the same control, not a second box
butted against it: **one** fill covers the whole frame, and the validation and
focus outlines ring the whole frame. Inside it, at the right end, a square
column one row height wide is closed off from the value by a one-pixel
`outline` hairline and split into two buttons — up above, down below, each an
arrow drawn from quad rows rather than a glyph, because the theme's font is
whatever the consumer loaded and a missing-glyph box on a control whose whole
point is being clickable is the worst place for one. An untouched column paints
no surface of its own; the button under the pointer, or held down, composites
its hover or pressed overlay over the control's own fill, so it lights up as
that surface lifted rather than as a separate element. A press steps by `step`
through the same clamp, snap, and commit path Up/Down use, so the two routes
cannot drift. A button **held down** keeps stepping: after half a second
(30 `Collect`s) it repeats ten times a second, and the repeat stops the moment
the button is released or the pointer slides off it. A held arrow *key* does
the same by doing nothing at all — the platform's key repeat arrives as
repeated `aether.key` presses and the step path holds no arm, unlike the button
and the toggle, which arm a key precisely so a repeat cannot fire a second
click (round-4 note 14). The key's cadence is the platform's; the button's is
the widget's, counted in the frames the root asks it to draw, the same clock
the toast region ages its notices by. The value text stays left-aligned at one `pad`, the text box
shrinks by the column, and the column never takes more than half the frame — a
numeric too narrow for both stays a value rather than becoming two arrows. A
read-only or disabled numeric has no live stepper targets.

The value's box is **padded on both sides and clipped at its own margin**: the
text starts one `pad` in and every part of it the reader can see — glyphs,
selection band, IME underline, caret — carries a clip that ends one `pad` short
of the hairline. So a value that fits has the same space at each end, and one
that does not is cut at that margin rather than printing across the seam and
under the arrows (round-4 note 6). A control with no gutter — a plain text
field — has no seam to be held off and carries no clip; its slot is already its
own frame. Nothing in
`NumericConfig` changed: steppers are what a numeric *is*, not something to opt
into.

Once the theme font's metrics resolve, a numeric reports its
`WidgetDrawList::intrinsic` — `[widest value width + 2 × pad + row height, row
height]` — so a consumer sizes the field to the range it configured instead of
guessing at it. The widest value is whichever *bound* renders longer, formatted
exactly the way the field formats a committed value (`-100 .. 20` is widest at
its minimum: the sign is a character like any other), capped at the edit
buffer's own 32-character bound so an effectively unbounded range asks for a
field rather than a wall. The endpoints, and only the endpoints, because the
number has to be stable: a width that also weighed the value on screen would
resize the slot on every keystroke, so a fractional `step` that renders an
interior value longer than either bound (`0 .. 100` by `0.5` holds `"12.5"`) is
the clip's business rather than the width's. The trailing row height is the
stepper column, so a host that takes this number gets a field three digits fit
in with a pad at each end — which is the other half of round-4 note 6, and the
half the host owns. Like
the button's intrinsic, it is `None` until the real advances land — a slot
sized from the per-character approximation would be resized the moment they
arrived — and, like the button's, the reference panel does not yet consume it.

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

### Rebuilding the table under a live gesture

`Focus::clear` drops the **entries and only the entries**. Focus, hover, the
drag capture, and the modal grab all survive it, and each is validated against
the table that replaces them at every read: a child the rebuild did not
re-register routes nothing, and its hover leaves through an ordinary
`HoverTransition` on the next motion.

That is not a convenience, it is what makes a resizable pane work. A root that
hosts a splitter rebuilds its layout on every `SplitterMoved` — that *is* the
resize — which means it calls `clear` and re-registers on the drag's first
pixel. A `clear` that also dropped the capture ended the drag there, so the
pane could not be moved; one that dropped the hover left the strip lit forever,
because a widget only goes unlit when a `HoverLost` reaches it and there was no
longer any record that it had the hover to lose.

Two things a consumer root still owns:

- **Re-register, do not respawn.** What survives is a `MailboxId`. A rebuild
  that despawns and re-spawns its children hands the capture an id that no
  longer exists, and the gesture dies whatever `Focus` does. Spawn children
  once; re-frame them.
- **`Focus::new()` is the reset.** A root switching screens wholesale wants a
  fresh table, not a cleared one carrying the last screen's hover.

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
lands over the widgets below it.

A *root* reaches the same lane two ways, for the plate that hosts other
children rather than escaping its own slot. `Composite::extend_overlay(items)`
is the overlay's counterpart to `extend_chrome` — the node's own draws, laid
down before any slot's — and `Composite::set_slot_overlay(child, true)` moves
one registered slot's ordinary `items` into the overlay, keeping its origin,
its slot clip, and its place in registration order. Together they make a
**group**: a popover's plate through `extend_overlay`, its children through
`set_slot_overlay` while it is open, flattened plate-first in layout order.

That grouping is what the clip subtraction reads. The root cuts ordinary text
out from under overlay fills once, per lane — the ordinary items against the
overlay's fills, the overlay's items against nothing — so a plate hides the
primary content it stands over and can never delete the labels of the children
standing on it. Within one lane fills are simply under glyphs, as everywhere
else in the kit. There is no layer number and no z-index on any draw kind: the
group is a set of slots the root already ordered, and the lane is the two-step
order the root already emits in.

The overlay's counterpart on the input side is the
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
natural size, the reference panel does not yet consume it. A frame *narrower*
than that intrinsic elides the label into the frame before centering it, so the
margins stay equal at any width and a label that did not fit ends in the kit's
elision mark rather than on a glyph the root's slot clip sliced in half.

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

A wheel always targets the deepest **self-scrolling** child under the cursor
using `Focus::hit_test` — a scroll viewport, or a virtual list, which owns the
row window it realizes — and it never follows pointer capture. The consuming actor
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
