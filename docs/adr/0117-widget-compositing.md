# ADR-0117: Widget Compositing

- **Status:** Proposed
- **Date:** 2026-06-15

## Context

ADR-0114 landed inline child actors: a component holds many co-located child actors cheaply — one WASM instance, one slot, one run-token — and addresses each like any actor. It deferred the `Widget` trait and the draw/compositing handshake to a consumer ADR. This is that ADR for compositing and draw order; the `Widget` trait API surface is a separate later ADR.

Two problems remain once a component has many inline children that draw:

- **Fan-in.** Each inline child that draws calls `ctx.actor::<RenderCapability>().send(...)` and reaches `aether.render` directly, stamped with the child's own address. A component with N drawing children is N render senders, which re-creates the #1852 fan-in inside a single component — the exact cost the inline-child arc was built to remove.
- **Draw order.** `DrawTriangle` / `DrawTexturedQuads` / `DrawSolidQuads` carry no ordering key; order is submission order within a pass and a fixed world-then-overlay split between passes. Independent components have no deterministic way to say which draws on top.

The widget tier needs both fixed: a component as one render sender regardless of child count, and a draw order that composes.

## Decision

A component is the **compositor** for its inline-child subtree. The structure of that subtree — the inline-child tree ADR-0114 already gives us — carries layout, addressing, and draw order at once.

1. **Parent-as-compositor; children draw local, the parent emits once.** An inline child draws in its own local coordinates and emits its geometry to its **parent** via local in-guest mail (the ~1.3µs path #1793 measured — no host hop), rather than to `aether.render`. The parent's compositor reads each draw's stamped `origin = child address` (the ADR-0114 §4 recipient-as-identity stamp, which doubles as the compositor's attribution key), applies the per-child layout offset it owns, accumulates the subtree, and emits to `aether.render` **once per frame**. A component is then one render sender regardless of child count — the #1852 fix.

2. **Draw order is structural.** Order is the depth-first traversal of the subtree: a node draws itself, then its children in sibling order, nested. No per-draw layer or z field exists. The ordering key is the node's position in the tree, which the inline-child address already encodes (`hud/aether.embedded:ability-bar/aether.embedded:button-3`). This is a total order — every node has a definite position via its sibling index — so there are no ties to resolve, and it composes: a subtree carries its own internal order and relocates anywhere without renumbering. Bring-to-front is a parent re-sequencing its own children, never a global magnitude.

3. **Slots are named inline children.** A slot — the ability-bar region of a HUD, an inventory panel — is an inline child at a position the parent assigns; the slot name is the child's address segment. Subslots are nested inline children. The slot tree, the address tree, and the draw-order tree are one structure.

4. **Layout flows down; geometry is local; intrinsic size flows up.** The parent owns layout — it assigns each slot a rect from its own configuration — and the child draws in local coordinates for the compositor to offset. The one channel flowing up is intrinsic size: when a slot's size depends on its content (a label's measured width, a list's length), the child reports that size up as a cached event so the parent can position it. Text measures locally and synchronously via #1883 (`CachedFontMetrics`), covering the common case with no round trip.

5. **No overlay depth buffer.** Two-dimensional UI composes by structural order and painter's algorithm, constrained only between overlapping elements. World geometry keeps its depth-tested pass; the overlay gains no depth buffer, which would fight alpha blending.

6. **The component is the grain of isolation — collapse cheap, split heavy.** A screen is a handful of components, each a cluster of inline widgets — neither one monolithic component nor one component per widget. Cooperative, cheap, reload-together widgets collapse inline, serialized under one run-token. Serialization is free for UI: per-widget handlers run in microseconds and sit well inside the frame budget, and #1852 is a fan-in and instance-memory problem rather than a compute one. An aspect is split into its own component when it earns a separate run-token (heavy or blocking work that would stall its siblings), independent hot-reload (inline children reload with their whole component), or failure isolation (a WASM trap takes its whole component down). A handful of component senders stays far below the ~1024 threshold where fan-in turns super-linear. The `spawn_inline_child` (co-located) versus `spawn_child` (detached, ADR-0097) split is the dial, already built.

### Ordering escape hatch (deferred)

Structural order cannot express a node that must draw outside its tree slot — a tooltip or modal floating above everything regardless of where it lives, or order between top-level roots. That needs an explicit ordering key or edge the compositor evaluates (lift a flagged subtree later in the order, or to a higher root). It is **named here but not built**: the common case is pure structural order, and the escape hatch earns its keep only when a real overlay needs it. It is forward-compatible — the absence of a key is what tree order means, so an opt-in key added later promotes only the nodes that request it and changes nothing else; the compositor collects-then-emits, so the later reorder is a localized change; and the postcard draw kinds can grow an optional field without breaking the wire. Order between independent top-level roots, when it lands, is the substrate's concern, sequenced where top-level surfaces are tracked — structural order governs everything inside a root.

## Consequences

- A component is one render sender for its whole widget subtree; the #1852 fan-in does not arise even for a dense HUD.
- Draw order, addressing, and layout share one structure, so there is no separate ordering namespace to coordinate and no global-magnitude inflation.
- Order composes: a widget subtree relocates without renumbering, because its internal order is relative to itself.
- Widgets are live actors. Each owns its state (ADR-0113), draws itself, and handles its own mail; each compositor is the layout/paint authority for its own subtree, with no central one. Configuration reaches a child as mail or init, not by mutating its fields.
- Serialization under one run-token is the cost of collapsing a cluster inline — acceptable for cooperative UI, and the verb split lets an author opt a heavy aspect out into its own component.
- An overlay that must escape its tree slot is not expressible until the deferred escape hatch lands; it is the named first follow-on.
- Reload granularity is the whole component, as for any WASM instance (ADR-0114).

## Alternatives considered

- **A flat ordering key (a `u16` or named bands on every draw kind)** — rejected: an absolute key on a shared axis requires every author to agree on one scale and invites unbounded inflation to force ordering, and it does not compose, since a subtree's values are meaningful only against the scale they were authored against. Structural order is relative and locally authored; an absolute scheme would resolve its own ties by structural order underneath in any case.
- **An overlay depth buffer or per-quad z** — rejected: two-dimensional UI composes by painter's order, and a depth buffer over the alpha-blended overlay fights blending.
- **A central retained UI tree processed by one engine** — rejected: the nodes are live actors that own their state and drawing, and each compositor is the authority for its subtree; a central engine would re-introduce the shared mutable model the actor design avoids.
- **A parallel widget/composite API** — rejected in ADR-0114 and unchanged here: a widget is a plain actor that mails its parent, with no separate composite model to load.

## Open questions

- The `Widget` trait surface (`layout` / `draw` / `on_event`) and the SDK support that makes "a widget is just an actor" ergonomic are the next consumer ADR; this ADR fixes the compositing and ordering model they sit on.
- The escape hatch's concrete form — an explicit key versus ordering edges, and how order between top-level roots is owned by the substrate — is left to the issue that builds it.
