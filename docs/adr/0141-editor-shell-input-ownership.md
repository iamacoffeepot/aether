# ADR-0141: Editor-shell input ownership across region roots

- **Status:** Proposed
- **Date:** 2026-07-09

## Context

ADR-0117 makes one widget cluster's root the sole owner of its input and its
draw fan-in: `WidgetPanel` subscribes the pointer and keyboard streams once,
holds a flat `Focus` table over its inline children, and forwards each event to
the drag-captured or hit child (`crates/aether-kit/src/widget/panel.rs`,
`crates/aether-kit/src/widget/focus.rs`). That model is complete *within* one
cluster: a single vertical stack, one focus ring, one drag capture.

A terrain-authoring editor is not one cluster. It composes several
independently-rooted surfaces on screen at once — one or more tool panels
(each its own `WidgetPanel` cluster), a world viewport (the `world::WorldView`
render surface plus `mover::WorldMover` picking), and a command console
(`console::ConsoleOverlay`). Each already owns its own input today, so nothing
arbitrates *between* them. Two behaviors have no owner: a drag that begins in a
panel and travels over the viewport must keep reaching the panel until release
(release-to-press-owner across regions), and keyboard input must go to exactly
one focused region while Tab still cycles controls *within* that region (nested
focus scopes). If every region self-subscribes the input streams, a single
pointer press is delivered to every region that hit-tests it, and a
press/release split across a region boundary has no single owner — the outcome
depends on delivery order, which is not deterministic under ADR-0114 breadth-
first dispatch. Sibling issue #2919, which standardizes per-panel interaction
state and focus traversal, explicitly defers "editor-wide ownership priority,
nested focus scopes, release-to-press-owner routing across regions, and
viewport/console coordination" to this decision.

## Decision

Introduce an **editor shell**: a new root actor (`EditorShell`, namespace
`aether.kit.widget.editor`, in `crates/aether-kit`) that owns input arbitration
*between* regions, one level above the per-cluster ownership ADR-0117 already
establishes. It reuses that ADR's decomposition recursively — a plain-state
routing table plus an actor that owns the mail — rather than inventing a new
mechanism.

1. **The shell is the sole input subscriber.** It subscribes the pointer,
   keyboard, text, IME, and modifier streams once, exactly as `WidgetPanel`
   does today. Region roots embedded under a shell stop self-subscribing; a
   region root run standalone keeps subscribing, so the two modes are selected
   by config, not by two code paths. `PanelConfig` gains an `owns_input` flag
   (default `true`, today's standalone behavior); a shell spawns its panel
   regions with `owns_input = false` and forwards their input itself.

2. **Regions are declared, not inferred.** The shell's config lists regions,
   each a hit rect plus the `MailboxId` of the region's input target (a panel
   root, the viewport's picking actor, or the console) and a keyboard-focus
   eligibility flag. A new plain-state `Routing` struct — the editor-level
   analogue of `Focus` — holds the region entries, the focused region, and the
   region-level press owner (drag capture). It does no mail and holds no
   capability handle; the shell drives it, mirroring how `Focus` and
   `Composite` are the bookkeeping halves of ADR-0117 while the actor owns the
   sends.

3. **Routing is two-level and deterministic.** The shell arbitrates *between*
   regions; each region root still owns focus/capture *within* itself.
   - A pointer **press** hit-tests to a region, records that region as the
     press owner, sets editor keyboard focus to it when focusable, and forwards
     the raw event to the region's target.
   - A pointer **release** routes to the press-owner region regardless of the
     current pointer position, then clears the press owner
     (release-to-press-owner across regions).
   - A pointer **move** routes to the captured region if one holds capture,
     else the hit region.
   - **Keyboard / text / IME / modifier** events route to the focused region.
     A reserved editor-scope chord cycles the focused region (nested focus
     scope); a plain Tab is forwarded into the focused region so the region's
     own Tab ring is unaffected.

4. **The shell owns input only, not draw fan-in.** Each region remains its own
   render sender for its own subtree — ADR-0117's one-sender-per-cluster
   property holds per region. The shell does not composite region draws; it may
   draw its own minimal chrome (region gutters) as an independent sender. A
   viewport is not a widget subtree, so folding regions into one `Composite`
   fan-in is explicitly rejected.

## Consequences

- Editor-wide input becomes deterministic and testable: a cross-region drag,
  a press/release split across a boundary, and single-region keyboard focus all
  have one arbiter, exercisable under TestBench without a live session.
- Input ownership crosses cluster/component roots for the first time: the shell
  forwards raw input kinds to peer mailboxes addressed by `MailboxId`, and
  region roots relinquish self-subscription under a shell. This is the boundary
  ADR-0117 did not cross and #2919 deferred here.
- `PanelConfig` gains `owns_input`; standalone panels are unchanged by the
  default, and existing loaders need no edit.
- The shell is a foundation for the terrain workbench (#2932) and the terrain
  editor/selection/preview surfaces (#2929–#2931), which compose regions the
  shell arbitrates.
- The shell layers on the per-panel focus/interaction contract standardized by
  #2919; building it on an unstandardized focus model would bake in the drift
  #2919 removes, so this work sequences after it.

## Alternatives considered

- **Extend `WidgetPanel`'s flat `Focus` to hold nested regions** — conflates
  within-panel widget focus with between-region arbitration, growing a second
  addressing level into the single-panel template consumers fork.
- **Composite regions through the existing `Composite` fan-in (shell as
  compositor)** — forces every region into one render sender and one draw
  cascade, coupling input ownership to draw ownership and breaking per-region
  independence (a viewport is not a widget subtree).
- **Let each region self-subscribe and negotiate ownership peer-to-peer** — no
  single arbiter, so a cross-region drag or a press/release split is
  nondeterministic under breadth-first dispatch.
- **Amend ADR-0117 instead of a new ADR** — the sibling clipping/textured
  issues (#2915/#2916) amend ADR-0117 because they stay within one cluster;
  editor-wide input ownership crosses cluster roots, the boundary #2919
  reserved for a dedicated decision record.
