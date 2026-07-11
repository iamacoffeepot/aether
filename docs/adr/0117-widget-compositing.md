# ADR-0117: Widget Compositing

- **Status:** Accepted
- **Amended (ADR-0118):** mentions of `postcard` below describe the superseded body format. The structured wire body is now `aether_data::wire`; postcard is no longer a dependency of any crate in the workspace. The decision this ADR records otherwise stands.
- **Date:** 2026-06-15

## Context

ADR-0114 landed inline child actors: a component holds many co-located child actors cheaply — one WASM instance, one slot, one run-token — and addresses each like any actor. It deferred the `Widget` trait and the draw/compositing handshake to a consumer ADR. This is that ADR for compositing and draw order; the `Widget` trait API surface is a separate later ADR.

Two problems remain once a component has many inline children that draw:

- **Fan-in.** Each inline child that draws calls `ctx.actor::<RenderCapability>().send(...)` and reaches `aether.render` directly, stamped with the child's own address. A component with N drawing children is N render senders, which re-creates the #1852 fan-in inside a single component — the exact cost the inline-child arc was built to remove.
- **Draw order.** `DrawTriangle` / `DrawTexturedQuads` / `DrawSolidQuads` carry no ordering key; order is submission order within a pass and a fixed world-then-overlay split between passes. Independent components have no deterministic way to say which draws on top.

The widget tier needs both fixed: a component as one render sender regardless of child count, and a draw order that composes.

## Decision

A component is the **compositor** for its inline-child subtree. The structure of that subtree — the inline-child tree ADR-0114 already gives us — carries layout, addressing, and draw order at once.

1. **Parent-as-compositor; children draw local, only the parent emits.** An inline child draws in its own local coordinates and emits its geometry to its **parent** via local in-guest mail (the ~1.3µs path #1793 measured — no host hop), rather than to `aether.render`. The parent's compositor reads each draw's stamped `origin = child address` (the ADR-0114 §4 recipient-as-identity stamp, which doubles as the compositor's attribution key), applies the per-child layout offset it owns, accumulates the subtree, and emits every required render call itself. A component is then one render sender regardless of child count or batch count — the #1852 fix.

2. **Draw order is structural.** Order is the depth-first traversal of the subtree: a node draws itself, then its children in sibling order, nested. No per-draw layer or z field exists. The ordering key is the node's position in the tree, which the inline-child address already encodes (`hud/aether.embedded:ability-bar/aether.embedded:button-3`). This is a total order — every node has a definite position via its sibling index — so there are no ties to resolve, and it composes: a subtree carries its own internal order and relocates anywhere without renumbering. Bring-to-front is a parent re-sequencing its own children, never a global magnitude.

3. **Slots are named inline children.** A slot — the ability-bar region of a HUD, an inventory panel — is an inline child at a position the parent assigns; the slot name is the child's address segment. Subslots are nested inline children. The slot tree, the address tree, and the draw-order tree are one structure.

4. **Layout flows down; geometry is local; intrinsic size flows up.** The parent owns layout — it assigns each slot a rect from its own configuration — and the child draws in local coordinates for the compositor to offset. The one channel flowing up is intrinsic size: when a slot's size depends on its content (a label's measured width, a list's length), the child reports that size up as a cached event so the parent can position it. Text measures locally and synchronously via #1883 (`CachedFontMetrics`), covering the common case with no round trip.

5. **No overlay depth buffer.** Two-dimensional UI composes by structural order and painter's algorithm, constrained only between overlapping elements. World geometry keeps its depth-tested pass; the overlay gains no depth buffer, which would fight alpha blending.

6. **The component is the grain of isolation — collapse cheap, split heavy.** A screen is a handful of components, each a cluster of inline widgets — neither one monolithic component nor one component per widget. Cooperative, cheap, reload-together widgets collapse inline, serialized under one run-token. Serialization is free for UI: per-widget handlers run in microseconds and sit well inside the frame budget, and #1852 is a fan-in and instance-memory problem rather than a compute one. An aspect is split into its own component when it earns a separate run-token (heavy or blocking work that would stall its siblings), independent hot-reload (inline children reload with their whole component), or failure isolation (a WASM trap takes its whole component down). A handful of component senders stays far below the ~1024 threshold where fan-in turns super-linear. The `spawn_inline_child` (co-located) versus `spawn_child` (detached, ADR-0097) split is the dial, already built.

### Realization (issue 2659)

The model above is a **role**, not a type: "a component is the compositor for its subtree" names a responsibility the parent actor carries, and §Alternatives already rejects a parallel widget API and a generic `Compositor<K>` membrane. Issue 2659 discharges it as a pure guest library in `aether-kit`, with no engine change, no new `aether-actor` verb, and no `Compositor` type — only ordinary `#[actor]` handlers over one new kind family plus one plain helper struct:

- **`aether.kit.widget.collect`** flows data-down: the root subscribes the frame stage once and, each frame, sends `collect` to every child via `ctx.child(name)` in layout order.
- **`aether.kit.widget.draw_list`** flows events-up: each widget's `collect` handler **always** replies its `WidgetDrawList` to `ctx.parent()` — empty when it draws nothing. That always-reply contract is what makes the parent's completion counter sound.

The reply kind is a single `WidgetDrawList { intrinsic: Option<[f32; 2]>, items: Vec<WidgetDrawItem> }`, where `WidgetDrawItem` is a `Quad`/`TexturedQuad`/`Text` enum in local coordinates. `TexturedQuad` carries named destination and UV fields, an `Rgba` tint, and a non-owning session texture id; the producer that created the texture retains update/destroy authority. Each item may carry a `WidgetClipRect { x, y, width, height }` local to its widget, and each parent-owned child slot may carry the same named type in the parent's local space. Composition translates item geometry and item clips together, then intersects them with the parent-local slot clip; repeating that operation produces one effective widget clip at the root. Empty, invalid, disjoint, and edge-touching intersections omit the draw. The work is O(N) in draw-item count and keeps coordinate-space identity explicit: kit composition never stores the render vocabulary's `ClipRect`, whose coordinates mean framebuffer pixels.

The single-list shape carries §Decision 4's intrinsic-size channel as the `intrinsic` field and preserves authored per-item solid/textured/text order in one vector — a split-vector reply would foreclose that order and drop the intrinsic channel. At emit, and only there, the root converts each effective `WidgetClipRect` field-for-field into framebuffer `ClipRect`. It filters text onto the established later lane and plans the remaining direct items in one pass: maximal solid runs share an effective clip, while maximal textured runs share both texture id and effective clip. A solid/textured transition, texture-id change, or clip change flushes without globally regrouping repeated keys. The root sends those direct runs to the same render recipient in authored order, so mailbox FIFO preserves painter order; it then sends per-string `DrawText`. An all-solid unclipped tree remains one `DrawSolidQuads`. This is still one render **sender** — the root actor — rather than a promise of one render mail. Direct/text interleave across the render and text pipelines remains bounded by the v1 split; the single-list shape keeps authored order recoverable for when a unified pass or the deferred layer key lands.

Completion is a **structural** signal, not a temporal one. An intra-cluster send is FIFO-queued and fully drained before control returns to the host (ADR-0114), so the whole collect cascade settles inside the one host dispatch that delivered the frame. The parent attributes each reply by `ctx.source_mailbox()`, files it into the slot that child was assigned, and emits once every registered slot has replied — counting filled slots against the fanned count. The queue is breadth-order, so a self-addressed "flush" mail would run *before* the children's replies; a filled-slot counter, not a re-poke or a deadline, is therefore the correct flush trigger. Depth-first draw order emerges by construction: each node orders by its own child list, and an interior node withholds its upward reply until its own slots close, so a nested subtree carries its internal order up intact. `WidgetConfig` describes the tree a node is loaded or spawned with; each child rides as a pre-encoded `WidgetConfig` in `WidgetChildSpec.config` bytes, which is what lets a tree nest without forming a self-referential schema.

For the stock flat panel, the root also owns focus, hover, and pointer capture for the cluster. Every stock config carries defaulted `WidgetControlState`; `SetWidgetState` flows external state down and source-attributed `WidgetStateChanged` flows changes up so root routing stays synchronized. Explicit `HoverLost` then `HoverGained` edges prevent sticky sibling hover, and forward/reverse focus traversal skips hidden and disabled children. Hidden children retain their layout slot and still discharge the compositing contract by replying to every `Collect` with an empty `WidgetDrawList`; disabled children remain drawn but cannot receive input, while read-only value controls may focus without mutating. These are actor mail and plain root bookkeeping, not a widget trait or retained UI tree.

A clipped scroll viewport is a dedicated stateful `ScrollWidget`, not a mode
on the passive compositor. `ScrollConfig` is the fixed layout authority for
its named `ScrollExtent` viewport and content extents; the actor alone owns its
named `ScrollOffset`. It registers the content root at local
`content_origin - offset` under a viewport-local `WidgetClipRect`, while the
same state produces an absolute window-pixel `WidgetFrame` for input. A nested
scroll viewport must exactly match the content extent its parent assigned it.
That keeps config, rather than a child's optional `WidgetDrawList::intrinsic`,
as the one clamp authority.

Wheel ownership follows the actor tree. A panel and every scroll actor use
`Focus::hit_test` over a wheel-only table, never the drag-capture-aware
`pointer_target`, so an unrelated button drag cannot steal a wheel gesture.
The deepest scroll child under the cursor receives the unchanged
`MouseWheel`; only the actor that consumes locally converts it once into the
content-space `ScrollDelta` sign. Each axis computes `next = clamp(old +
requested, 0, max)`, `consumed = next - old`, and `residual = requested -
consumed`. A typed `ScrollOutcome` reports the owning container and exact
named fields; a non-zero `ScrollResidual` moves upward already converted.
Ancestors apply that residual directly and relay any remainder, preserving
partial overshoot and inner-before-outer event order.

Container transparency also applies to live style: `SetTheme` follows the
scroll actor tree to the retained content root, so a panel's resolved font id
and later restyles reach stock widgets nested inside one or more viewports.

### Ordering escape hatch (deferred)

Structural order cannot express a node that must draw outside its tree slot — a tooltip or modal floating above everything regardless of where it lives, or order between top-level roots. That needs an explicit ordering key or edge the compositor evaluates (lift a flagged subtree later in the order, or to a higher root). It is **named here but not built**: the common case is pure structural order, and the escape hatch earns its keep only when a real overlay needs it. It is forward-compatible — the absence of a key is what tree order means, so an opt-in key added later promotes only the nodes that request it and changes nothing else; the compositor collects-then-emits, so the later reorder is a localized change; and the postcard draw kinds can grow an optional field without breaking the wire. Order between independent top-level roots, when it lands, is the substrate's concern, sequenced where top-level surfaces are tracked — structural order governs everything inside a root.

## Consequences

- A component is one render sender for its whole widget subtree; the #1852 fan-in does not arise even when distinct effective clips require multiple contiguous render batches from that root.
- Draw order, addressing, and layout share one structure, so there is no separate ordering namespace to coordinate and no global-magnitude inflation.
- Order composes: a widget subtree relocates without renumbering, because its internal order is relative to itself.
- Widgets are live actors. Each owns its state (ADR-0113), draws itself, and handles its own mail; each compositor is the layout/paint authority for its own subtree, with no central one. Configuration reaches a child as mail or init, not by mutating its fields.
- Serialization under one run-token is the cost of collapsing a cluster inline — acceptable for cooperative UI, and the verb split lets an author opt a heavy aspect out into its own component.
- An overlay that must escape its tree slot is not expressible until the deferred escape hatch lands; it is the named first follow-on.
- Reload granularity is the whole component, as for any WASM instance (ADR-0114).
- Widget-local and parent-local clipping composes in O(N) without leaking framebuffer-coordinate types into the tree. Invalid or empty intersections disappear before submission, and the root is the sole conversion boundary to the render/text scissor type.
- Textured widget items retain named local destination and UV fields through composition. Their session texture ids are borrowed lifecycle references: missing or expired textures keep the render capability's record-time warn/drop behavior.
- One panel root owns its cluster's focus, hover, and capture. External state changes reconcile routing through source-attributed mail; unavailable children cannot retain live focus/hover/capture, and hidden children preserve structural completion with empty draw replies.
- Each scroll viewport has one actor-owned offset and one config-owned content
  extent. Recursive wheel hit testing gives the deepest viewport first claim;
  only exact per-axis residual reaches an ancestor, independent of pointer
  capture.
- Scroll wire values use named, pixel-unit fields (`ScrollExtent`,
  `ScrollOffset`, `ScrollDelta`, and `ScrollResidual`). The pre-existing
  array-backed `WidgetChildSpec::origin` and `WidgetDrawList::intrinsic`
  remain compatibility boundaries and are not copied into the scroll
  protocol.

## Alternatives considered

- **A flat ordering key (a `u16` or named bands on every draw kind)** — rejected: an absolute key on a shared axis requires every author to agree on one scale and invites unbounded inflation to force ordering, and it does not compose, since a subtree's values are meaningful only against the scale they were authored against. Structural order is relative and locally authored; an absolute scheme would resolve its own ties by structural order underneath in any case.
- **An overlay depth buffer or per-quad z** — rejected: two-dimensional UI composes by painter's order, and a depth buffer over the alpha-blended overlay fights blending.
- **A central retained UI tree processed by one engine** — rejected: the nodes are live actors that own their state and drawing, and each compositor is the authority for its subtree; a central engine would re-introduce the shared mutable model the actor design avoids.
- **A parallel widget/composite API** — rejected in ADR-0114 and unchanged here: a widget is a plain actor that mails its parent, with no separate composite model to load.

## Open questions

- The `Widget` trait surface (`layout` / `draw` / `on_event`) and the SDK support that makes "a widget is just an actor" ergonomic are the next consumer ADR; this ADR fixes the compositing and ordering model they sit on.
- The escape hatch's concrete form — an explicit key versus ordering edges, and how order between top-level roots is owned by the substrate — is left to the issue that builds it.
