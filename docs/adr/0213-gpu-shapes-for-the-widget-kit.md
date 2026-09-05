# ADR-0213: GPU shapes for the widget kit

- **Status:** Proposed
- **Date:** 2026-09-05

## Context

The widget kit (`aether-kit-widget`) draws with three items — `WidgetDrawItem::Quad`,
`WidgetDrawItem::TexturedQuad`, and `WidgetDrawItem::Text` (`kinds.rs`) — which the
root coalesces into runs (`direct_runs` in `lib.rs`) and sends as
`aether.render.draw_solid_quads`, `aether.render.draw_textured_quads`, and one
`aether.text.draw_batch` per lane, every tick. Everything on screen is an
axis-aligned flat rectangle. The consequences show up as visual limits the design
keeps hitting:

- A border is four thin quads (`push_rect_border` in `set/mod.rs`); a one-pixel
  edge at a fractional y lands on two pixel rows at half strength, the finding
  the round-17 tooltip review left open.
- A toggle knob and a radio dot are squares, because the kit has no circle.
- The dropdown's caret is a triangle built from up to `TRIANGLE_MAX_ROWS = 16`
  quad rows (`push_triangle`), while the render cap has had
  `aether.render.draw_screen_triangles` since the overlay pass grew per-vertex
  colour.
- The lunaris design record states the cost outright: *"The kit has no shadow to
  spend and the design allows no draw layer, so the plate and its edge are the
  whole of the lift"* — a hover plate over a chosen row reads at 1.04:1 because
  the only lift available is a fill and a stroke.
- Corners cannot round, so every plate, field, and button is a hard box; the
  method doc's face ladder is expressed entirely in colour steps.

Meanwhile the render capability has grown a real GPU surface: the overlay pass
records batches in painter order with a per-batch scissor (`record_quad_overlay_pass`,
`clamped_scissor` in `aether-substrate/src/render/quad.rs`); the world material pass
already evaluates a coverage field with `fwidth` anti-aliasing in a fragment shader
(`fs_coverage` in `material.wgsl`, ADR-0140); authored programs put WGSL, uniforms,
geometry, compute, and render-to-texture in an actor's hands (`ProgramRegister`,
`ProgramDispatch`, `TextureUsage::Writable`, ADR-0170 / ADR-0171). None of it is
reachable from the overlay lane the kit draws in, and the kit uses none of it.

Two constraints bound the design. ADR-0117 makes draw order the widget hierarchy —
no z value, no layer, and text under a later fill is handled by cutting scissor
holes (`LaterFills` in `lib.rs`). ADR-0105 and ADR-0107 rejected retained UI
objects and per-frame caching, with the clause that *"a retained layer can come
later behind the same kinds if profiling ever demands it"*; the kit's own cost
measurement (`tests/widget_actor_cost.rs`) puts an actor-backed widget at about
1.3 µs per frame, count-independent, and the studio's lists are virtualized, so
profiling does not demand it today.

## Decision

**1. The render capability gains one screen-space shape primitive, evaluated on
the GPU, in the overlay lane.** A new kind, `aether.render.draw_shapes`, carries
`{ space: QuadSpace, clip: Option<ClipRect>, shapes: Vec<Shape> }` where a `Shape`
is an axis-aligned box with `corner_radius`, an optional `fill`, an optional inside
`stroke { width_pixels, color }`, and an optional `shadow { blur_pixels, offset, color }`.
The substrate expands each shape to one quad grown by its shadow extent, exactly as
`draw_solid_quads` expands to six vertices today, and a new `shape.wgsl` fragment
stage evaluates a rounded-box signed distance per pixel: shadow under fill under
stroke, each edge anti-aliased over one `fwidth`. A radius at or above half the
shorter side is a circle; a stroke with no fill is a ring; a shadow with neither is
a soft halo. The batch takes the same painter position and the same scissor as any
other overlay batch — it is one more `OverlayDraw` variant with its own pipeline,
not a pass and not a layer. Headless absorbs it. The vocabulary is fixed and
substrate-owned, like `DrawMaterialCoverage`: callers supply parameters, never
WGSL, so the overlay lane stays a closed contract that ADR-0117's hole cutting can
reason about.

**2. The kit gains `WidgetDrawItem::Shape` and draws its chrome with it.** One
item replaces a plate quad plus four stroke quads. `covered_rect` for a shape is
its fill box (the shadow covers nothing), so `LaterFills` hole cutting is unchanged
and conservative at rounded corners. `direct_runs` coalesces adjacent shapes by
clip into one `draw_shapes` mail. Theme tokens for radius, stroke width, and
shadow are stated on the spacing scale and measured like every other token; the
contrast tripwires (`REGION_STEP`, `OVERLAY_STEP`) extend to the shadow's edge so
the lift a plate gets from its shadow is a number in a test. Buttons, fields,
plates, tooltips, dialogs, dropdowns, toggles, radios, and the scroll bar's thumb
adopt it; the caret becomes a `draw_screen_triangles` item; `push_triangle` retires.

**3. Text stays behind ADR-0105's kinds; its atlas may become multi-channel signed
distance later.** `aether.text.draw` keeps its surface. An MSDF atlas is the
internal replacement ADR-0105 already reserved, and becomes worth its machinery when
the tree's world-space labels and the studio at a scale factor other than the atlas
size need to hold crisp under zoom. It is a follow-on, not part of this decision.

**4. Retention stays deferred, with a named trigger.** A pane rendered once into a
writable texture (`TextureUsage::Writable`) and re-shown as one textured quad is
the "retained layer behind the same kinds" ADR-0107 allowed. It is not built here.
The trigger is measured, not felt: `widget_actor_aggregate_scale` or a chassis
frame trace showing the kit's own draw path above a third of the frame budget at
the studio's real widget count. Until then the per-tick resend stands.

## Consequences

- The overlay pass gains a third pipeline and a wider vertex (`QUAD_VERTEX_STRIDE`
  stays for quads; shapes carry their own stride). The 4 MiB overlay buffer cap
  applies to shape bytes too; a shape is about three quads' worth.
- The one-pixel edge at a fractional position renders as one anti-aliased edge
  instead of two half-rows. Snapping overlay plates to whole pixels stays a kit
  nicety for text crispness, no longer a correctness fix.
- The design gets a shadow to spend: the hover plate's lift over a chosen row can
  be a soft edge rather than a stroke alone, and the tooltip review's 1.04:1 ground
  has a remedy that is not another colour.
- The kit's draw vocabulary grows from three items to four; ADR-0117's rules
  (submission order, holes, no layer) are untouched.
- Follow-on work, in order: (a) render cap: `Shape`, `draw_shapes`, `shape.wgsl`,
  the overlay pipeline, headless absorb, a SubstrateHarness pixel test for radius,
  stroke, and shadow; (b) kit: `WidgetDrawItem::Shape`, `direct_runs`, theme tokens,
  the widget set's adoption, the caret via screen triangles, and the guide chapter;
  (c) lunaris: the studio's plates and the tooltip lift on the new tokens;
  (d) later, on its own evidence: MSDF text behind `aether.text`, and the retained
  pane surface behind the same kinds.
- A sibling item outside the kit: the lunaris tree view resubmits on the order of
  a hundred thousand `DrawTriangle`s per frame for four thousand discs and rings.
  That is the actual GPU-bound consumer, and its path is ADR-0171's geometry
  registry plus a program draw pass with `PassRepeat { count, uniform_stride }` as
  per-instance data — one dispatch instead of one `send_many` of everything.

## Alternatives considered

- **Authored WGSL in the overlay lane (a kit-registered program that draws its
  chrome).** Rejected: a program draws into its own targets before the world pass,
  so it cannot take a painter position among the kit's other batches; it would
  become a layer by another name.
- **Rounded corners and shadows as textures (nine-slice from an atlas).** Rejected:
  every radius, scale factor, and colour needs its own slices; the SDF evaluates
  any of them from six numbers and stays crisp at every scale.
- **A general path or polygon primitive.** Rejected for now: the kit's shapes are
  boxes, circles, and one caret; a path rasterizer is much more machinery than the
  design has asked for. `draw_screen_triangles` covers the caret today.
- **Retained pane surfaces first.** Rejected: the measurement says the kit's draw
  path is not the long pole, and ADR-0107 asked for profiling before retention.
- **Stencil for holes instead of scissor subtraction.** Rejected: it removes the
  CPU rectangle subtraction but reintroduces a per-pixel occlusion mask that is not
  the widget hierarchy, and the existing rejections (hairlines, union bound) are
  cost trades that already work.
