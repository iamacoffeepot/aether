# Text

> **Decision status:** [ADR-0105](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0105-text-rendering.md) is
> Accepted and marked shipped. Its load-bearing split is stable: the
> `aether.text` capability owns fonts and layout on the CPU, while
> `aether.render` owns textures and textured-quad drawing.

`aether.text` turns TTF bytes and strings into render mail. It has no GPU
handle and no private rendering path: it loads fonts, measures and rasterizes
glyphs, maintains a CPU atlas, then mails texture updates and textured quads to
`aether.render`. This keeps font machinery replaceable and gives sprites,
images, widgets, and text one shared texture surface.

## Mental model

The capability holds two session-scoped caches:

1. a font registry keyed by numeric `font_id`, with a reverse index from
   `(namespace, path)` to id; and
2. one 512 × 512 RGBA8 glyph atlas, with entries keyed by
   `(font_id, glyph index, rounded pixel size)`.

Loading and measuring are retained operations. Drawing is immediate-mode:
send a draw every frame it should be visible. The capability lays out a
horizontal glyph run, uploads atlas misses before use, and emits one or more
`aether.render.draw_textured_quads` batches. The renderer, not the text actor,
applies projection, clipping, blending, and frame lifetime. See
[Rendering & camera](rendering.md).

`TextCapability` is available as a lightweight addressing identity under the
`text` feature. Its fontdue-backed native state is compiled only with
`text-runtime`; wasm senders can use the kinds without linking fontdue or
substrate runtime types. The split is defined in
[`text/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/text/mod.rs).

## Public mail surface

Wire kind names and Rust types are deliberately shown separately:

| Mail kind | Rust payload | Contract |
|---|---|---|
| `aether.text.load_font` | `LoadFont` | read `namespace://path`; reply `aether.text.load_font_result` / `LoadFontResult` |
| `aether.text.load_font_bytes` | `LoadFontBytes` | parse request-carried TTF bytes; reply with the same `LoadFontResult` kind |
| `aether.text.font_metrics` | `FontMetricsRequest` | resolve `FontRef::Id` or `FontRef::Path`; reply `aether.text.font_metrics_result` / `FontMetricsResult` |
| `aether.text.draw` | `DrawText` | fire-and-forget one string |
| `aether.text.draw_batch` | `DrawTextBatch` | fire-and-forget ordered strings with compatible-run batching |

The exact schemas are in
[`text/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/text/kinds.rs).
`FontRef`, `DrawText`, and `LoadFontBytes` are Rust value types, not extra
mail-kind names.

### Loading a font

`LoadFont { namespace, path }` forwards an `aether.fs.read`, preserves the
original reply route through typed request context, and parses the bytes on a
blocking task. Success returns `font_id`, a filename-derived display name, and
the source byte count. Read and parse errors return the requested namespace and
path with a reason. See [File I/O](file-io.md).

`LoadFontBytes { name, bytes }` takes the same parse path without fs. It
registers under the synthetic namespace `memory`, using `name` as both path key
and display name. This is the route for a guest with an embedded fallback TTF.

Path-backed fonts are deduplicated by exact `(namespace, path)`: repeated loads
reuse the existing session id. Memory-backed loads likewise collide by exact
name. Font ids start at zero, are monotonic for the process, and are not stable
across restart. There is no unload operation.

### Drawing

`DrawText` names a resident `font_id`, UTF-8 text, positive finite
`size_pixels`, a linear RGBA tint, an `origin`, a `QuadSpace`, and optional
`ClipRect`.

- `QuadSpace::Screen` treats `origin` as the top-left screen-pixel offset from
  which the horizontal run flows. Screen y increases downward.
- `QuadSpace::World { anchor, scale }` ignores `origin`, centers the run
  horizontally on `anchor`, and places its baseline at the anchor. The glyph
  quads remain camera-facing. `QuadScale::Pixels` holds screen size;
  `QuadScale::Distance` holds the requested pixel size at its reference
  distance and shrinks with perspective.
- `clip`, when present, is a framebuffer-pixel scissor in either projection
  mode. It is not local to `origin` or to the world anchor.

Layout currently walks Unicode scalar values with fontdue horizontal metrics.
It does not shape scripts, apply kerning, perform BiDi, choose fallback fonts,
or build multiple lines. A newline is not a layout command; callers that need
line wrapping or line breaks must measure and emit separate runs.

World runs use pixel offsets relative to their anchor; screen runs receive the
authored origin after glyph placement. Empty-coverage glyphs such as spaces do
not emit quads but still advance the pen. An unknown font id warn-drops that
item. A non-finite or non-positive size silently drops it.

`DrawTextBatch` preserves authored item order. Adjacent non-empty items with
equal projection and equal clip are coalesced into one quad send. A projection
or clip transition flushes the current run; later items with an earlier key do
not reorder backward to join it. Unknown-font or invalid-size items alone are
dropped, without discarding valid neighbors. Glyph uploads are sent before the
quad run that can sample them. The implementation is in
[`text/runtime/mod.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/text/runtime/mod.rs)
and
[`text/runtime/layout.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/text/runtime/layout.rs).

### Measuring locally

`FontMetricsRequest` accepts either a known id or a path. A resident id/path
replies immediately. A path miss loads and registers the font through fs before
replying. An unknown id returns `FontMetricsResult::Err` rather than a warning.

The result is size-independent: units per em, ascent, descent, line gap,
default advance, and a codepoint-sorted advance table. Consumers scale those
font units locally for caret placement, hit testing, or fit-to-content layout,
avoiding a mail round trip for every string measurement. The shared wire value
types are in
[`aether-kinds/src/lib.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kinds/src/lib.rs), and the
wasm-safe scaling helper is in
[`text_metrics.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-kinds/src/text_metrics.rs).

## Atlas lifecycle

The first valid draw has no texture id yet. It sends one
`aether.render.create_texture` for a zeroed 512 × 512 RGBA8 atlas and emits no
glyph draw. The correlated `CreateTextureResult` stores the id; because drawing
is immediate-mode, the caller's next frame retries naturally.

On a glyph miss, fontdue coverage is stored as alpha over white RGB and one
sub-rectangle update is mailed. A one-pixel gutter separates packed glyphs.
Pixel sizes are rounded to the nearest integer, at least one, for cache keys;
layout still uses the authored float size. Nearby fractional sizes can
therefore share one raster while retaining different advances and quad
placement.

If a glyph cannot fit, that glyph is omitted and the atlas marks itself full.
At the top of the next draw call, the capability clears the CPU pixels and
cache, resets the shelf packer, uploads one full transparent rectangle, then
re-rasterizes the requested glyphs as misses. The saturating frame may be
partial; the next frame recovers if its working set fits. There is no LRU or
multi-atlas spill. See
[`text/runtime/atlas.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/text/runtime/atlas.rs).

## Invariants and failure modes

- Font parsing and fs reads are settlement-held deferred work; the eventual
  result must resolve the original requester, not the intermediate fs sender.
- The CPU atlas is the source of truth. Texture creation and glyph sub-rect
  uploads must remain ordered before draw mail on the same chain.
- Draw mail has no success reply. Unknown ids, bad sizes, atlas overflow, and
  renderer-side texture errors are observable through logs or missing output.
- A failed atlas `create_texture` clears the in-flight flag but leaves no
  texture id. The next immediate-mode draw retries creation.
- Font ids and the atlas texture id are process/session state. They must never
  be persisted as durable asset identifiers.
- The current id allocator uses saturating increment and has no explicit
  exhaustion error. Code that changes registry lifetime should add an
  observable exhaustion policy rather than relying on practical unreachability.

## Chassis and feature caveats

Desktop and the render-capable TestBench compose both text and render, so all
operations are usable. The full-stack headless chassis also composes the CPU
text capability: font loading and `font_metrics` can work when the addressed fs
namespace is present, but drawing cannot. Its headless render cap returns
`CreateTextureResult::Err`, after which later draws retry and still emit no
quads. The minimal hub chassis does not compose `aether.text` or
`aether.render`.

The current composition is defined in
[`chassis_common.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-substrate-bundle/src/chassis_common.rs),
[`test_bench/chassis.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-substrate-bundle/src/test_bench/chassis.rs),
and
[`render/headless_runtime.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/render/headless_runtime.rs).

## Where to change or extend it

- Change the wire surface in
  [`text/kinds.rs`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/text/kinds.rs) and
  update ADR-0105 when the contract changes. Keep mail kind strings distinct
  from helper value types.
- Add shaping, fallback, kerning, or multiline behavior in the CPU layout
  layer. If callers need new authored policy, add an explicit schema field;
  otherwise keep it behind the existing draw kinds.
- Change packing or rasterization in `runtime/atlas.rs` without exposing GPU
  state to the text capability. A multi-atlas design would need batching and
  lifecycle work because each draw run names one texture id.
- Projection, clipping, texture format, and blending changes belong to the
  render surface, not font layout. Coordinate changes with
  [Rendering & camera](rendering.md).
- Any new native implementation remains behind `text-runtime`; the always-on
  identity and kinds must stay wasm-safe.
