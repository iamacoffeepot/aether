# Designing a screen

The widget kit gives you controls. It does not give you a screen. A panel
built by mapping every fact to a label and every verb to a button, stacked in
one column at one row height, is a terminal with buttons: it passes the letter
of "real widgets" and fails everything a person actually uses a screen for.
This page is the procedure from "these facts and these verbs" to a laid-out
screen, in the order the decisions have to be made, with the rule each step
enforces and the published source for that rule. It is a method, not taste:
every rule below is checkable, and most are checkable by a test over the
panel's own reported rectangles and strings.

The sources are the ordinary ones — Nielsen Norman Group's research articles,
the Windows, GNOME, and Apple human-interface guidelines, and Material Design's
published scales. They are linked where they apply and collected at the end so
an agent can read the primary text rather than this page's summary of it.

## 1. Regions before widgets

The first decision is not a control. It is the division of the window into
regions, each with one purpose and its own ground. The primary content — the
map, the document, the scene — owns the largest region. Supporting content —
the inspector, the sheet, the library — is docked beside it in its own region,
never floated over it.

The reason is perceptual, not aesthetic. People group what is near
([proximity](https://www.nngroup.com/articles/gestalt-proximity/): "items close
together are likely to be perceived as part of the same group") and what
shares a boundary ([common
region](https://www.nngroup.com/articles/common-region/): "items within a
boundary are perceived as a group and assumed to share some common
characteristic or functionality"). A panel floating over the map is inside the
map's region, so it reads as *part of the map* — and its position in that space
reads as a claim about hierarchy that nobody intended. Docking it gives it its
own region, and the boundary between the two says "these are two things."

Both desktop guideline sets describe the same shape. GNOME's [utility
panes](https://developer.gnome.org/hig/patterns/containers/utility-panes.html)
"display additional controls, locations or information alongside the main
window view," placed "on the left if they influence the main view, or on the
right when they serve a subordinate role." Material's supporting-pane layout
is the same division with the same left/right rule.

Concretely:

- Decide the regions on paper first: primary view, supporting pane(s), a
  header band if identity belongs to the whole screen. Give each a fixed or
  proportional width and a minimum. A supporting pane is a **fixed logical
  width** (a few hundred pixels), not a fraction of the window: a pane that is
  60% of a 3000-pixel window is a wall.
- The primary view's viewport is *its region*, not the window. "Fit to view"
  fits the region. Nothing about the supporting pane is the primary view's
  business; it is not told to avoid a rectangle.
- Content that is itself a separate thing gets a separate region: an
  ascendancy sub-tree drawn over the main tree's nodes is one region carrying
  two things, and the reader cannot tell them apart. Give it an inset, a
  second viewport, or a mode — a boundary of its own.
- Transient UI (a picker, a menu) is the one thing allowed to overlay, and it
  overlays *its own region's* content, is light-dismissed, and is gone when
  answered ([WinUI expander
  notes](https://learn.microsoft.com/en-us/windows/apps/design/controls/expander)
  draw exactly this line between transient and pushing content).

## 2. Inventory and rank the facts

Before any control is picked, list every fact the screen shows and every verb
it offers, and rank them. Who or what this is (identity) comes first; the
numbers the whole screen exists to show come second; everything else is
secondary or machinery. The rank decides placement, size, and words.

- **Identity goes in the header band, top-left, in full.** Eye-tracking is
  unambiguous: "first lines of text on a page receive more gazes than
  subsequent lines" and "first few words on the left of each line receive
  more fixations than subsequent words"
  ([F-pattern](https://www.nngroup.com/articles/f-shaped-pattern-reading-web-content/)).
  Put the name, the class, the level — whatever the reader would say first
  when describing the thing — where the eye lands first, at the largest size
  on the screen.
- **One place per fact.** A fact shown twice makes the reader check whether
  the two agree. Nielsen's [aesthetic and minimalist
  design](https://www.nngroup.com/articles/ten-usability-heuristics/):
  "interfaces should not contain information that is irrelevant or rarely
  needed." A sheet that restates the class, ascendancy, and name the header
  already shows is repeating itself.
- **Full words, the domain's words.** Nielsen's second heuristic: "use words,
  phrases, and concepts familiar to the user, rather than internal jargon."
  GNOME's [writing style](https://developer.gnome.org/hig/guidelines/writing-style.html):
  "a three-word label providing clear information surpasses a one-word label
  that's ambiguous." An abbreviation is allowed only when it is the domain's
  own word (`DPS` is; `Light` for lightning is not; `Asc` is not). Truncating
  a *value* — a resistance that falls off the row's end — is never allowed:
  the value is the point of the row. Widen, wrap, or reflow.
- **Define the hierarchy before drawing.** NN/g's [visual
  hierarchy](https://www.nngroup.com/articles/visual-hierarchy-ux-definition/)
  page says it directly: "before beginning a design, take a step back from the
  visuals and define the hierarchy of the content and the key point(s) you
  want the user to take away." The levers are scale, colour/contrast, and
  grouping. If every row is the same size and weight there is no hierarchy,
  whatever the order.

## 3. Choose the control by the shape of the choice

A control is chosen by what kind of choice it presents, and the guideline sets
agree on the thresholds closely enough to state them as one table. `N` is the
number of options; "attention" is whether every option deserves to be seen
every time.

| The choice | Control | Rule and source |
|---|---|---|
| Binary, on/off | Toggle or checkbox | Two radio buttons for one yes/no is wrong ([WinUI radio buttons](https://learn.microsoft.com/en-us/windows/apps/design/controls/radio-button)) |
| One of 2–5, all deserve attention | Radio group, or a segmented control | Radio "when users need to see all options before they make a selection" (WinUI); segmented "no more than about five to seven segments in a wide interface" ([Apple HIG](https://developer.apple.com/design/human-interface-guidelines/segmented-controls)), equal widths, short labels |
| One of 3–8, one is the current/default, others secondary | **Dropdown** (pop-up showing the current value) | "Use a combo box when the selection items are of secondary importance … draws the user's attention to the selected item" ([WinUI combo box](https://learn.microsoft.com/en-us/windows/apps/design/controls/combo-box)); GNOME: never with fewer than three items ([drop-downs](https://developer.gnome.org/hig/patterns/controls/drop-downs.html)) |
| One of more than 8, static | Dropdown | "If there are more than eight options, use a combo box" (WinUI radio buttons) |
| One of many, dynamic or long | List with a filter field, 3–9 rows visible | Listbox "ideal range of items … is 3 to 9"; "don't use a listbox if it forces users to scroll excessively" ([NN/g listbox vs dropdown](https://www.nngroup.com/articles/listbox-dropdown/), WinUI) |
| A number in a range | Numeric field sized to the number, or a slider | Field "about the same size as the expected input" ([NN/g form design](https://www.nngroup.com/articles/web-form-design/)) |
| A command | Button; `…` when it asks for more before acting | GNOME writing style: ellipsis "if further input or confirmation is required" |
| Parallel content sets, one viewed at a time | Tabs, one row, 1–2 word labels | "Tab labels should usually be 1-2 words"; "prominently highlight the selected tab" ([NN/g tabs](https://www.nngroup.com/articles/tabs-used-right/)) |
| Secondary detail under an always-visible primary | Expander | "Some primary content should always be visible, but related secondary content may be hidden until needed" (WinUI expander) |
| Content the reader needs most of | **Not** an accordion | "Accordions should be avoided when your audience needs most or all of the content on the page" ([NN/g accordions](https://www.nngroup.com/articles/accordions-complex-content/)) |
| A value whose meaning the reader may not carry | **Tooltip** (`TooltipWidget`), sections divided by a rule, wrapped at a reading measure | Hover explains, in a neat measured box; the host owns the dwell and the words, the kit owns the box |
| A refusal or a confirmation | **Toast region** (`ToastWidget`), one per screen, coloured by severity | Notices have one place, one severity colour each (never the accent), and leave on their own after a few seconds |
| A split between two regions the reader sets | **Splitter** (`SplitterWidget`), two lit pixels over a generous strip | Keep an affordance the reader has learned; the pointer says what a drag will do — the widget reports the hover, the host sets the cursor |
| A group of controls raised over the primary view | **Popover** (`set::popover`), drawn in the root's overlay, light-dismissed | A setting is a control, not a file; a pop-up takes priority over what it covers, through the overlay's clip subtraction and never a draw layer |

Two consequences worth stating because they are the common mistakes:

- A **stack of expanders is an accordion**, and an accordion whose sections
  the reader needs all of — or which, all open, does not fit the region — is
  the wrong control. Content the reader consults constantly is pinned; the
  rest are tabs, because tabs are parallel and one-at-a-time by construction
  and never overflow when "all open."
- A **dropdown shows the current value closed**, which is the whole reason to
  use one for "current choice, other options enumerated." A list of options
  standing open all the time for a single current choice is a listbox, and a
  listbox with a highlighted first row while the model holds no selection is
  a lie the reader will act on.

## 4. Space and density

Density is set by a spacing scale, not by a row height. Material's
[8-pixel grid](https://m3.material.io/foundations/layout/understanding-layout/spacing)
is the convention most systems share: spacing tokens are multiples of 4 and 8
(`4, 8, 12, 16, 24, 32`), and every gap on the screen is one of them.

- **Row height is derived**, from the type size plus vertical padding, not one
  constant applied to every row. A section heading and a body row are
  different heights because they are different sizes.
- **Inside a group, gaps are small; between groups, gaps are large** —
  proximity again, and the whole trick to making five blocks read as five
  blocks. Whitespace first; a border or a rule only when whitespace cannot do
  it (common region "can overpower other grouping principles," so it is the
  strong tool, used sparingly).
- **Controls are sized to their content.** A field is as wide as its expected
  input. A button is its label plus padding, with a minimum width, and a row
  of buttons is not three equal thirds of the column. A list shows 3–9 rows
  and is never a tall empty box: zero items get one line of empty-state text.
- **Targets are big enough and near enough.** [Fitts's
  law](https://www.nngroup.com/articles/fitts-law/): "the larger the target,
  the shorter the movement time," and related controls sit next to each other
  so the pointer does not cross the screen between steps. A minimum hit
  height of 24–32 logical pixels; frequent controls at a region edge, where
  the edge stops the pointer.

## 5. Type and alignment

- **A type scale with named roles.** At minimum title, heading, body, and
  caption/label, each a distinct size and weight. Material's published scale
  is the reference (title 22/16, body 16/14, label 14/12 in pixels at 1×;
  [type scale
  tokens](https://m3.material.io/styles/typography/type-scale-tokens)), and
  GNOME's guidance for using it: "smaller and/or lighter text for less
  important information, and heavier/darker text to attract attention"
  ([typography](https://developer.gnome.org/hig/guidelines/typography.html)).
  No all-caps, no italics, no hard-coded one-off sizes.
- **A proportional face for UI text.** Monospace is for code and for columns
  of numbers, and columns of numbers get it through *tabular figures* in the
  UI face, not by setting the whole screen in a code font. Aligning columns
  by counting characters and padding strings with spaces is `printf`, and it
  is why a screen reads as terminal output no matter what draws it.
- **One alignment rule per column.** Text left-aligned to one edge per column;
  numbers right-aligned in their column so magnitudes line up; a label beside
  its field is left-aligned and *adjacent* to it — "labels should be close to
  the fields they describe," never right-aligned or equidistant between two
  fields ([NN/g form design](https://www.nngroup.com/articles/web-form-design/)).
- **Button labels are centred**, horizontally and vertically, and a single
  glyph is *optically* centred: measure the ink box of the glyph, not the
  advance width of the character cell, or a `+` sits low and left of where the
  eye says centre is.
- **Nothing is cut.** A string that does not fit its slot is a layout problem,
  not a string problem. Names may elide with an ellipsis only when the full
  form is one hover away; values never elide.

## 6. One meaning per visual token

Every colour role and shape means one thing across the whole screen — Nielsen's
consistency heuristic: "users should not have to wonder whether different
words, situations, or actions mean the same thing."

| Role | Means | Never also means |
|---|---|---|
| Accent fill | the primary action | a selected row, a selected segment, a disclosure |
| Selection | the current item in a list or a set | a button |
| Hover / pressed | pointer feedback, as overlays over any role | — |
| Disabled | alpha over the role | a different colour |
| Disclosure | a small chevron beside the title | a large filled button |

A theme that has one accent and one text size cannot express this table, which
is the honest reason a panel built on it ends up with gold meaning four
things. The roles have to exist in the theme before a consumer can keep them
apart.

## 7. Review

A screen is reviewed against the steps above, in order, before it is judged on
looks. The review is a heuristic evaluation in Nielsen's sense — a walk through
named rules, each producing a finding or a pass — and every item below is
phrased so a reviewer can answer it yes or no. Where the panel reports its own
rectangles and strings (as a query kind), most items are also a test.

1. Regions: the supporting pane never overlaps the primary view at the
   reference window; the primary view fits to its region; separate things
   have separate regions.
2. Identity: name, kind, and the two or three facts a person would say first
   are in the header band, top-left, at the largest type on the screen, in
   full words.
3. One place per fact: no fact appears twice.
4. Words: no abbreviation outside the domain's own vocabulary; no value is
   truncated; every command that asks for more ends in `…`.
5. Control fit: each choice uses the control the table in §3 gives for its
   `N` and attention; a dropdown shows its current value; no list shows a
   selection the model does not hold; no accordion carries content the reader
   needs all of.
6. Space: every gap is a token from the scale; gaps inside a group are smaller
   than gaps between groups; row heights vary with type role.
7. Sizing: fields match expected input; buttons are label plus padding; lists
   show 3–9 rows and have an empty state.
8. Type: at least three distinct type roles are visible; UI text is
   proportional; numbers are tabular.
9. Alignment: one left edge per text column; numbers right-aligned; labels
   adjacent to their fields; button labels centred, glyphs optically centred.
10. Tokens: the accent appears only on primary actions; selection uses the
    selection role; disclosure is a chevron.
11. Squint test: with the screen blurred, the biggest and brightest shape is
    the most important fact.

## Rules learned from use

The steps above are the design pass. The rules below came out of watching a
person use a screen built by them; each is stated so a reviewer can check it,
and each is the general form of a specific complaint.

- **Native where the platform has it.** Menus, cursors, the application's
  name in the platform's own chrome, the window title: use the platform's
  mechanism on a chassis that has one (a native menu bar on the desktop),
  and draw the kit's version only where there is none. A person expects the
  platform's shape and finds the in-window copy strange, however correct.
  (Nielsen: consistency and standards.)
- **Every text control honours the platform's editing conventions.**
  Select-all on the platform's modifier (Cmd on macOS, Ctrl elsewhere), key
  repeat on a held key, word-wise movement, cut / copy / paste. A control
  that takes typing and drops one of these is a bug, not a wish.
- **A control that cannot complete an action does what it can and says
  what it could not.** Allocating a path with too few points allocates up
  to the budget and reports how far it got; it does not refuse the whole
  path. (Nielsen: user control; error prevention over error refusal.)
- **The pointer says what a drag will do.** A resizable edge shows a resize
  cursor along its axis; a movable thing shows a move cursor; a control that
  will not take the press shows not-allowed. Signifiers precede action.
- **Group by category with a divider between groups, never within.**
  Attributes together, resistances together, pools together; one rule
  between groups. A list of fifteen rows with no grouping is a table nobody
  can scan. (Gestalt: proximity and common region.)
- **Names are specific.** "Rotation time", not "Time"; "Energy shield", not
  "Shield". A generic word beside a specific number tells the reader nothing
  about which number it is.
- **No jargon without its sentence.** A hint like "dim = not counted yet" is
  either a full sentence a newcomer understands or it is not shown.
- **Diagnostics live behind a debug mode.** Node ids, mailbox ids, frame
  counters: useful to whoever is fixing the screen, noise to whoever is
  using it. A View → Debug toggle, off by default.
- **Notices have one place.** Refusals and confirmations appear in a single
  dedicated region the reader learns once, coloured by severity (info,
  warning, error), and leave on their own after a few seconds. A message
  that appears wherever there was room, and stays, is read as part of the
  screen.
- **Search is a first-class control.** When a screen holds more than a
  reader can scan, the search field is visible without opening anything,
  at the top of the region it searches.
- **Numeric inputs carry steppers.** A number a person adjusts by one gets
  an up and a down beside the field; typing is the other way in, not the
  only way.
- **Hover explains.** A value with context the reader may not carry
  (which act a level is, what a stat is, where a number came from) explains
  itself on hover in a tooltip that is a neat measured box: wrapped at a
  maximum width, sections divided, never broken mid-thought.
- **A focus ring marks keyboard focus only.** A control the reader just
  clicked does not also grow a box; Tab traversal does show one. (The
  platform focus-visible rule.)
- **Padding is symmetric.** A label centred in its button has the same
  space on both sides; a control inside a plate sits at least two spacing
  units from the plate's edge; a glyph used as a control (`+`, `−`) is
  optically centred.
- **Hide what the context disables.** A node, start, or option that only
  applies under a state the build is not in (another class's start, an
  ascendancy-gated passive) is not drawn until that state holds. Clutter is
  every element that cannot currently matter.
- **Signify only what is not obvious.** A resize cursor belongs on a
  splitter, where the affordance is hidden; a move cursor over a map that
  every reader knows pans is noise. A signifier on the default gesture is
  intrusive, and it dilutes the ones that matter.
- **A control's parts are one element.** A number field and its steppers
  share one frame and one fill; a field and its clear button likewise.
  Two adjacent boxes read as two controls fighting.
- **Empty collapses.** A container with nothing chosen (an inset for an
  ascendancy that is not picked) starts collapsed and collapses when its
  content goes; an expanded empty box is a promise the screen is not
  keeping.
- **One colour, one state, everywhere.** The allocated colour appears on
  allocated things only. A ring drawn in the "taken" gold before anything
  is taken says the opposite of the truth.
- **A result says why it matched.** A search that lights a node shows,
  on that node, the line that matched; a hit with no visible reason reads
  as a mistake.
- **Own versus derived, separated.** What this node gives and what the
  path to it costs are two facts; a tooltip that blends them makes the
  reader do the subtraction.
- **A setting is a control, not a file.** If a person can change it, the
  screen offers the control; the file is the store behind it, never the
  interface.
- **Labels stay complete under a heading.** "Fire resistance", not "Fire"
  under a "Resistances" heading the reader may not see at the same time.
- **Keep an affordance the reader has learned** unless they ask for it to
  go; when they ask for its *look* to change, change the look.
- **A box is sized from measured text, never from a character count.**
  A plate, a tooltip, a toast, a list cell: its width comes from the
  face's real advances (the kit measures through its font metrics), not
  from characters times an average. Over a proportional face the average
  is wrong by a glyph every few words, and the text walks out of the box
  the estimate drew. If the measure is not available yet, draw nothing
  rather than a guessed box. (Owner, round 3, live: "the text is going
  outside the bounds of the box.")
- **Generate what can be generated.** A name, a summary line, a default:
  when the data implies one ("Wander of Kinetic Blast Ranger" from weapon,
  skill, and class), offer it rather than an empty field.

## What the kit has to provide

The method assumes a few things the widget kit does not yet express. Each is a
small, separable change and belongs in the kit rather than in any one consumer:

- **Theme**: a type scale with named roles (title, heading, body, label), a
  spacing scale, and a `selection` colour role distinct from `accent`.
- **Layout**: a region/dock primitive for the panel root (fixed-width pane
  beside a primary view), and a row/column layout that sizes rows from a
  child's reported intrinsic size instead of one row height.
- **Controls**: a dropdown that draws its open list outside its slot, a tab
  strip, and an expander with a chevron; a button that centres its label with
  a minimum width; a list with an empty state and a genuine no-selection
  state. The transient surfaces a screen composes over its own content — the
  tooltip, the toast region, the splitter, and the popover's plate and
  dismissal — are in the kit too, so a second screen does not hand-roll them.
- **Text**: a proportional UI face with tabular figures, and text measurement
  a consumer can ask for so alignment is computed, not counted.

## Sources

- Nielsen Norman Group: [10 usability heuristics](https://www.nngroup.com/articles/ten-usability-heuristics/) ·
  [proximity](https://www.nngroup.com/articles/gestalt-proximity/) ·
  [common region](https://www.nngroup.com/articles/common-region/) ·
  [visual hierarchy](https://www.nngroup.com/articles/visual-hierarchy-ux-definition/) ·
  [F-shaped reading pattern](https://www.nngroup.com/articles/f-shaped-pattern-reading-web-content/) ·
  [Fitts's law](https://www.nngroup.com/articles/fitts-law/) ·
  [accordions on desktop](https://www.nngroup.com/articles/accordions-complex-content/) ·
  [tabs, used right](https://www.nngroup.com/articles/tabs-used-right/) ·
  [listbox vs dropdown](https://www.nngroup.com/articles/listbox-dropdown/) ·
  [drop-down menus](https://www.nngroup.com/articles/drop-down-menus/) ·
  [web form design](https://www.nngroup.com/articles/web-form-design/)
- Windows (WinUI) design guidance: [combo box and list box](https://learn.microsoft.com/en-us/windows/apps/design/controls/combo-box) ·
  [radio buttons](https://learn.microsoft.com/en-us/windows/apps/design/controls/radio-button) ·
  [expander](https://learn.microsoft.com/en-us/windows/apps/design/controls/expander) ·
  [tab view](https://learn.microsoft.com/en-us/windows/apps/design/controls/tab-view)
- GNOME Human Interface Guidelines: [drop-down lists](https://developer.gnome.org/hig/patterns/controls/drop-downs.html) ·
  [typography](https://developer.gnome.org/hig/guidelines/typography.html) ·
  [writing style](https://developer.gnome.org/hig/guidelines/writing-style.html) ·
  [utility panes](https://developer.gnome.org/hig/patterns/containers/utility-panes.html)
- Apple Human Interface Guidelines: [segmented controls](https://developer.apple.com/design/human-interface-guidelines/segmented-controls) ·
  [pop-up buttons](https://developer.apple.com/design/human-interface-guidelines/pop-up-buttons) ·
  [sidebars](https://developer.apple.com/design/human-interface-guidelines/sidebars)
- Material Design 3: [type scale tokens](https://m3.material.io/styles/typography/type-scale-tokens) ·
  [spacing and the 8-pixel grid](https://m3.material.io/foundations/layout/understanding-layout/spacing) ·
  [canonical layouts](https://m3.material.io/foundations/layout/canonical-layouts/overview)
