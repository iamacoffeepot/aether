# Text

> **Decision status:** the renderer draws text.
> [ADR-0248 §10](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0248-lineage-is-an-ordered-tree.md)
> (Proposed) records that, and supersedes the part of
> [ADR-0105](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0105-text-rendering.md)
> that made text a capability of its own. ADR-0105's render surface, and its
> layout and atlas design, stand.

Text is part of `aether.render`. There is no `aether.text` mailbox: the three
text kinds are `aether.render.*` kinds, sent to the renderer like any other
draw. The renderer registers fonts, lays strings out, keeps the glyph atlas as
one of its own textures, and draws each batch of strings as one textured
overlay batch. See [Rendering & camera](rendering.md) for the overlay pass it
draws in.

## Mental model

The render actor holds two pieces of session state for text:

1. a font registry keyed by numeric `font_id`; and
2. the glyph atlas: a shelf packer and a cache keyed by
   `(font_id, glyph index, rounded pixel size)`, over one 512 × 512 RGBA8
   texture the renderer reserves for itself.

Both are plain state of the one render actor, with no lock around either.
Registering a font is a request with a reply. Drawing is immediate mode: send
a draw every frame it should be visible.

A text draw is one mail from the actor that wants the text to the renderer,
and nothing else. The renderer lays it out on its own turn and pushes one
batch onto the overlay list, where a shape batch would go. So text and shapes
from one actor keep the order that actor sent them in. Between two actors the
order is lineage order: a child's draws lie over its parent's and a later
sibling's over an earlier one's, whatever order the mail arrived in
([Rendering](rendering.md)).

The kinds compile without the renderer's native half: a wasm guest that
depends on `aether-render` with `default-features = false` gets the kind types
and the `RenderCapability` identity without `fontdue` or `wgpu`.

## Public mail surface

All three go to the `aether.render` mailbox.

| Mail kind | Rust payload | Contract |
|---|---|---|
| `aether.render.create_font` | `CreateFont { bytes }` | parse a font from a blob; reply `aether.render.create_font_result` / `CreateFontResult` |
| `aether.render.font_metrics` | `FontMetricsRequest { font_id }` | reply `aether.render.font_metrics_result` / `FontMetricsResult` |
| `aether.render.draw_text` | `DrawText { clip, space, runs }` | fire-and-forget one batch of strings |

The exact schemas are in
[`aether-render/src/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/kinds.rs).
`TextRun` is a value type inside `DrawText.runs`, not a mail kind.

### Registering a font

`CreateFont { bytes }` carries the whole of a TrueType or OpenType file as a
blob (`aether_data::Blob`). The renderer reads no file and depends on no other
capability, so the caller gets the bytes however it likes:

- a component reads the file with `aether.fs.read` and passes the `bytes` blob
  of the `ReadResult` on, or takes one of its module's assets. A blob held
  this way is sent on by reference, so the font is not copied through the
  guest's memory;
- a component that embeds a font builds the blob with `Blob::from`;
- an MCP session passes the bytes as `{"$hex": "..."}`.

The parse runs on a blocking worker, off the renderer's turn, and the reply
comes when it finishes: `Ok { font_id }`, or `Err { error }` for bytes that
are not a font. Each request has its own parse and its own reply.

Font ids start at zero, count up for the process, and are not stable across a
restart. A second `create_font` of the same bytes is a second font with a new
id, as a second `create_texture` of the same pixels is a second texture. There
is no destroy operation. A session that runs out of ids is answered `Err`.

### Drawing

`DrawText` carries a `QuadSpace`, an optional `ClipRect`, and a list of runs.
Each `TextRun` names a registered `font_id`, UTF-8 text, a positive finite
`size_pixels`, a linear RGBA `color`, and an `origin`.

- `QuadSpace::Screen` treats each run's `origin` as the pen's starting pixel,
  from which the run flows left to right. Screen y increases downward.
- `QuadSpace::World { anchor, scale }` ignores `origin`, centres each run
  horizontally on `anchor`, and places its baseline at the anchor. The glyph
  quads remain camera-facing. `QuadScale::Pixels` holds screen size;
  `QuadScale::Distance` holds the requested pixel size at its reference
  distance and shrinks with perspective.
- `clip`, when present, is a framebuffer-pixel scissor in either projection
  mode. It is not local to an origin or to the world anchor.

The projection and the clip are on the batch, as they are on `DrawShapes`.
Runs that need a different clip or projection go in a different `DrawText`.

Layout walks Unicode scalar values with fontdue's horizontal metrics. It does
not shape scripts, apply kerning, perform BiDi, choose fallback fonts, or
build multiple lines. A newline is not a layout command; callers that need
line wrapping or line breaks measure and send separate runs.

Empty-coverage glyphs such as spaces draw nothing and still advance the pen.
A run naming an unknown font id, or a size that is not finite and positive, is
dropped with a warning, and the other runs of the batch still draw. The
implementation is in
[`runtime/text/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/runtime/text/mod.rs)
and
[`runtime/text/layout.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/runtime/text/layout.rs).

### Measuring locally

`FontMetricsRequest { font_id }` is answered inside the call. An unknown id
returns `FontMetricsResult::Err`.

The result is size-independent: units per em, ascent, descent, line gap,
default advance, and a codepoint-sorted advance table. Consumers scale those
font units locally for caret placement, hit testing, or fit-to-content layout,
avoiding a mail round trip for every string measurement. The shared value
types are in
[`aether-kinds/src/lib.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kinds/src/lib.rs), and the
wasm-safe scaling helper is in
[`text_metrics.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kinds/src/text_metrics.rs).

## The reserved atlas

The glyph atlas is an entry in the renderer's texture registry under a
reserved id: `u32::MAX` is the white texture, and the atlas is the id below
it. `create_texture` hands ids out from zero and stops below both, so the
atlas never shifts a caller's ids. A caller can sample it, and an
`update_texture` or `destroy_texture` naming it is refused with a warning.

The texture is registered in the call that first draws text, zeroed, so that
first draw shows. Its staged pixels are the only copy of the atlas image, and
it is counted on the renderer's `textures` memory gauge at its 1 MiB like any
other texture. The staged pixels survive a render device replacement, so the
glyph cache stays valid across one.

On a glyph miss, fontdue coverage is written into the staged pixels as alpha
over white RGB, and the texture uploads at the next frame record. A one-pixel
gutter separates packed glyphs. Pixel sizes are rounded to the nearest
integer, at least one, for cache keys; layout still uses the authored float
size. Nearby fractional sizes can therefore share one raster while keeping
different advances and quad placement.

If a glyph cannot fit, that glyph is omitted and the atlas marks itself full.
At the top of the next text draw the renderer clears the cache, resets the
shelf packer, zeroes the texture, and places that draw's glyphs as misses. The
saturating draw may be partial; the next recovers if its working set fits.
There is no LRU or multi-atlas spill. See
[`runtime/text/atlas.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/runtime/text/atlas.rs).

## Invariants and failure modes

- A text draw and a shape from one actor keep that actor's send order. Both
  are mail to the one renderer through one queue, and each becomes an overlay
  batch in the turn its mail is handled. An opaque plate sent after a text
  draw covers it, and text sent after a plate lies on it. Text and a plate
  from two different actors lie in lineage order instead: the actor created
  later, or beneath the other, is on top.
- The first draw of a registered font shows. Nothing is created by a round
  trip on the way.
- Draw mail has no success reply. Unknown ids, bad sizes and atlas overflow
  are observable through logs or missing output.
- Layout and the rasterising of unseen glyphs run on the renderer's turn,
  which on desktop is the driver thread. A warm draw costs one metrics call,
  one cache lookup and one quad per character.
- A `Screen` run's `origin` is the pen, not the ink: layout starts the pen
  at the origin and places the baseline one **ascent** below it (the face's
  horizontal line ascent at the draw size, or the size itself for a face
  without line metrics). A caller that wants a run centred in a row computes
  the baseline it wants and subtracts the ascent `FontMetricsResult` reports,
  scaled to the draw size;
  treating the origin as the top of a `size_pixels`-tall box sinks the run.
- A character the face lacks is not an error and is not skipped. Its cmap
  lookup yields glyph index `0`, so the run draws and advances by the face's
  `.notdef` glyph, and `FontMetrics::default_advance` is that same advance.
  A caller that wants `⌘` in a label ships a face that has it.
- A dirty staged texture uploads whole, so one new glyph re-uploads the
  atlas at the next frame record.
- Font ids are session state. They must never be persisted as durable asset
  identifiers.

## Chassis and feature caveats

Text is available wherever the renderer is: the desktop chassis and a
render-composing SubstrateHarness. A chassis composes only the capabilities it
serves, so headless and the hub compose no `aether.render`, and a component
that depends on it is refused at load there
([ADR-0232 §6](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0232-flat-ctx-send-verbs.md)).

## Where to change or extend it

- Change the mail surface in
  [`aether-render/src/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-render/src/kinds.rs)
  and record the change in the governing ADR.
- Add shaping, fallback, kerning, or multiline behavior in
  `runtime/text/layout.rs`. If callers need new authored policy, add an
  explicit schema field; otherwise keep it behind the existing draw kind.
- Change packing in `runtime/text/atlas.rs`. A multi-atlas design would need
  batching work, because each text batch names one texture.
- Projection, clipping and blending belong to the overlay pass text shares
  with the other overlay draws; see [Rendering & camera](rendering.md).
- New native code stays behind the `runtime` feature; the kinds and the
  identity must stay wasm-safe.
