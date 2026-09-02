# Drawing your first text

**Class:** drive. No recompile — use a running render-capable engine and a TTF
asset. Reach for the MCP harness (`send_mail`) or a component's `ctx`; the mail
contract is the same, while first use takes one atlas-creation turn before text
can appear.

Text is two surfaces composed. The renderer owns a generic textured-quad
surface — upload pixels, draw alpha-blended quads in screen space — and the
`aether.text` capability turns a font file plus a string into those quads. You
load a font once, then draw a string every frame you want it on screen.

## 1. Load a font

Place a TTF under the host directory configured as the engine's read-only
`assets` root, then mail `aether.text.load_font` to the `aether.text` mailbox.
You cannot create it by mailing `aether.fs.write` to `assets`; that namespace is
read-only. For an embedded/small font, use `aether.text.load_font_bytes` instead.

```jsonc
// send_mail → aether.text  (kind: aether.text.load_font)
{ "namespace": "assets", "path": "fonts/RobotoMono.ttf" }
```

The capability fetches the bytes through `aether.fs.read`, parses the font off
the hot path, registers it under a session-scoped `font_id`, and replies
`aether.text.load_font_result`:

```jsonc
{ "Ok": { "font_id": 0, "name": "RobotoMono", "resident_bytes": 183700 } }
```

A bad path or an unparseable file replies `{ "Err": { namespace, path, error } }`
instead. Hold onto `font_id` — it names the font for every draw, and it is
valid until the engine restarts.

## 2. Draw a string

Mail `aether.text.draw` every frame the text should be visible — the same
immediate-mode contract as `aether.draw_triangle`. Send it once and the string
shows for one frame; stop sending it and it vanishes.

```jsonc
// send_mail → aether.text (no application reply; keep the tool's settled default)
{
  "font_id": 0,
  "text": "hello aether",
  "size_pixels": 32.0,
  "color": { "r": 1.0, "g": 1.0, "b": 1.0, "a": 1.0 }, // RGBA, linear
  "origin": [24.0, 24.0],
  "space": "Screen",
  "clip": null,
  "layer": 0
}
```

`Screen` lays the string out in window pixels starting at `origin`,
flowing left to right along the baseline. On the first valid draw, the
capability has no atlas texture id: it sends one `create_texture` request and
emits no glyphs. The next draw after `CreateTextureResult` can render. Once the
atlas exists, a newly seen glyph emits its texture update before the quad batch
in the same turn; later draws of that glyph are cache hits.

`color` is a linear RGBA multiplier over the glyph coverage: the alpha channel
scales the blend, so `{ "r": 1, "g": 0, "b": 0, "a": 1 }` draws solid red
text and `{ "r": 1, "g": 1, "b": 1, "a": 0.5 }` draws half-transparent
white.

## 3. See it

From the MCP harness, `capture_frame` with the draw in `mails` renders the
string into the returned PNG. This is the second draw after step 2. If the atlas
has not been created yet, the first such capture only triggers texture creation;
wait for that settled result and repeat the capture.

```jsonc
// capture_frame
{
  "mails": [
    { "recipient_name": "aether.text", "kind_name": "aether.text.draw",
      "params": { "font_id": 0, "text": "hello aether", "size_pixels": 32.0,
                  "color": { "r": 1.0, "g": 1.0, "b": 1.0, "a": 1.0 },
                  "origin": [24.0, 24.0], "space": "Screen", "clip": null, "layer": 0 } }
  ]
}
```

## 4. Float a label above a character

To draw a label at a world-space position — above a character's head, for
instance — use `World { anchor, scale }` instead of `Screen`.

```jsonc
// send_mail → aether.text (no application reply; keep the tool's settled default)
{
  "font_id": 0,
  "text": "Player",
  "size_pixels": 18.0,
  "color": { "r": 1.0, "g": 1.0, "b": 0.8, "a": 1.0 },
  "origin": [0.0, 0.0],
  "space": {
    "World": {
      "anchor": [0.0, 2.0, 0.0],
      "scale": { "Distance": { "reference_distance": 10.0 } }
    }
  },
  "clip": null,
  "layer": 0
}
```

`anchor` is the world-space point the label floats above. The string is
centered horizontally on the anchor, with the baseline sitting at the anchor's
projected screen position and glyphs extending upward.

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

- `aether.text.draw_batch` submits multiple `DrawText` items while preserving
  vector order and coalescing compatible glyph runs.
- `aether.text.font_metrics` returns a size-independent metrics table for a
  font id or namespace/path so callers can measure, fit, and place text locally.
- `origin` places screen text; `clip` applies an optional framebuffer-pixel
  scissor. In world mode the anchor replaces origin.

## What it does not do yet

- **One font, one size, one run per item.** No shaping, bidirectional text, or
  emoji — the layout is fontdue's horizontal advance metrics.
- **The atlas has no LRU or multi-atlas spill.** Missing glyphs may be skipped in
  the draw that saturates it. On the next draw the capability clears the atlas
  and cache, uploads the cleared texture, and re-rasterizes; rendering recovers
  when the active working set fits in the fresh atlas.

All of these sit behind the `aether.text.*` kinds, so the internals can grow
without changing the mail you send.
