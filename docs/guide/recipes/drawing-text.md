# Drawing your first text

**Class:** drive. No recompile — use a running render-capable engine and the
bytes of a TTF. Reach for the MCP harness (`send_mail`) or a component's `ctx`;
the mail contract is the same either way.

The renderer draws text. You register a font once from its bytes, then send a
text draw to `aether.render` every frame you want it on screen. The first draw
shows: nothing has to be created by a round trip first.

## 1. Register a font

Mail `aether.render.create_font` to the `aether.render` mailbox. Its one field,
`bytes`, is the whole font file as a blob. The renderer reads no file, so
getting the bytes is the caller's job, and there are two usual ways.

**From a session**, pass the file's bytes as hex. The vendored
`RobotoMono.ttf` is about 184 kB, so about 370 kB of hex text:

```jsonc
// send_mail → aether.render  (kind: aether.render.create_font)
{ "bytes": { "$hex": "0001000000120100000400204753554249…" } }
```

**From a component**, read the file through `aether.fs` and hand the blob on.
A blob a guest received is sent on by reference, so the font is never copied
through the guest's memory. A throwaway component is enough for a session
that would sooner not paste hex:

```rust
#[actor(root, depends(FsCapability, RenderCapability))]
impl WasmActor for FontLoader {
    const NAMESPACE: &'static str = "demo.font_loader";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(FontLoader { font_id: None })
    }

    // Mail is allowed from `wire`: ask `aether.fs` for the file.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.send::<FsCapability>(&Read { addr: NamespaceAddr::new("assets", "fonts/RobotoMono.ttf") });
        Ok(())
    }

    // The file's bytes arrive as a blob; pass it straight to the renderer.
    #[handler::response]
    fn on_read_result(&mut self, ctx: &mut WasmCtx<'_>, read: ReadResult) {
        match read {
            ReadResult::Ok { bytes, .. } => {
                ctx.send::<RenderCapability>(&CreateFont { bytes });
            }
            ReadResult::Err { error, .. } => tracing::warn!(?error, "the font file did not read"),
        }
    }

    // Keep the id: it names the font in every draw.
    #[handler::response]
    fn on_create_font_result(&mut self, _ctx: &mut WasmCtx<'_>, created: CreateFontResult) {
        match created {
            CreateFontResult::Ok { font_id } => self.font_id = Some(font_id),
            CreateFontResult::Err { error } => tracing::warn!(%error, "the bytes are not a font"),
        }
    }
}
```

A component that carries its font with it takes it from its module's assets, or
builds the blob from embedded bytes with `Blob::from`, and skips the read.

Either way the renderer parses the font off its own turn and replies
`aether.render.create_font_result`:

```jsonc
{ "Ok": { "font_id": 0 } }
```

Bytes that are not a font reply `{ "Err": { "error": "…" } }` instead. Hold
onto `font_id` — it names the font for every draw, and it is valid until the
engine restarts. Registering the same bytes again gives a second font with a
new id.

## 2. Draw a string

Mail `aether.render.draw_text` every frame the text should be visible — the
same immediate-mode contract as `aether.draw_triangle`. Send it once and the
string shows for one frame; stop sending it and it vanishes.

```jsonc
// send_mail → aether.render (no application reply; keep the tool's settled default)
{
  "clip": null,
  "space": "Screen",
  "runs": [
    {
      "font_id": 0,
      "text": "hello aether",
      "size_pixels": 32.0,
      "color": { "r": 1.0, "g": 1.0, "b": 1.0, "a": 1.0 }, // RGBA, linear
      "origin": [24.0, 24.0]
    }
  ]
}
```

A `draw_text` is a batch: `clip` and `space` apply to every run in `runs`, and
each run is one string in one font at one size. `Screen` lays a run out in
window pixels starting at its `origin`, flowing left to right. The baseline
sits one ascent below the origin.

`color` is a linear RGBA multiplier over the glyph coverage: the alpha channel
scales the blend, so `{ "r": 1, "g": 0, "b": 0, "a": 1 }` draws solid red
text and `{ "r": 1, "g": 1, "b": 1, "a": 0.5 }` draws half-transparent
white.

Text is drawn in the order you send it among your other overlay draws. Send a
`draw_shapes` plate and then the label, and the label lies on the plate; send
the plate second and it covers the label.

## 3. See it

From the MCP harness, `capture_frame` with the draw in `mails` renders the
string into the returned PNG. One capture is enough, even for the first draw
of the session.

```jsonc
// capture_frame — window is required; it is the window's actor path
// as aether.window.list reports it.
{
  "window": "aether.window/aether.window.instance:main",
  "mails": [
    { "address": "aether.render", "kind_name": "aether.render.draw_text",
      "params": { "clip": null, "space": "Screen",
                  "runs": [ { "font_id": 0, "text": "hello aether", "size_pixels": 32.0,
                              "color": { "r": 1.0, "g": 1.0, "b": 1.0, "a": 1.0 },
                              "origin": [24.0, 24.0] } ] } }
  ]
}
```

## 4. Float a label above a character

To draw a label at a world-space position — above a character's head, for
instance — use `World { anchor, scale }` instead of `Screen`.

```jsonc
// send_mail → aether.render (no application reply; keep the tool's settled default)
{
  "clip": null,
  "space": {
    "World": {
      "anchor": [0.0, 2.0, 0.0],
      "scale": { "Distance": { "reference_distance": 10.0 } }
    }
  },
  "runs": [
    {
      "font_id": 0,
      "text": "Player",
      "size_pixels": 18.0,
      "color": { "r": 1.0, "g": 1.0, "b": 0.8, "a": 1.0 },
      "origin": [0.0, 0.0]
    }
  ]
}
```

`anchor` is the world-space point the label floats above. Each run is
centered horizontally on the anchor, with the baseline sitting at the anchor's
projected screen position and glyphs extending upward. A run's `origin` is
ignored in world space.

`scale` controls how the label's apparent size relates to camera distance:

- `{ "Distance": { "reference_distance": 10.0 } }` — the label holds its
  `size_pixels` exactly when the anchor is 10 units from the camera and shrinks
  proportionally as it recedes. This is the above-the-head mode: the label
  looks natural from any distance.
- `"Pixels"` — the label keeps a fixed on-screen pixel size regardless of
  distance. Useful for HUD-style labels that must stay readable at any range.

Both modes use the current `aether.view_projection` view-projection matrix, so the label
always faces the camera and never skews as the camera orbits. Send the draw
every frame the label should appear, the same as `Screen` text.

## Related text operations

- `aether.render.font_metrics` returns a size-independent metrics table for a
  `font_id`, so callers can measure, fit, and place text locally.
- `clip` applies an optional framebuffer-pixel scissor to the whole batch.
  Runs that need different clips go in different `draw_text` mails.

## What it does not do yet

- **One font, one size, one line per run.** No shaping, bidirectional text, or
  emoji — the layout is fontdue's horizontal advance metrics.
- **The atlas has no LRU or multi-atlas spill.** Glyphs that do not fit are
  skipped in the draw that fills it. The next text draw clears the atlas and
  places its glyphs afresh; drawing recovers when the working set fits.
- **Fonts are never released.** There is no destroy request for a font.

All of these sit behind the three `aether.render` text kinds, so the internals
can grow without changing the mail you send.
