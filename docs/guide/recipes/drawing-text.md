# Drawing your first text

**Class:** drive. No recompile — a running engine, a TTF asset, and two
pieces of mail. Reach for the MCP harness (`send_mail`) or a component's
`ctx`; the steps are identical.

Text is two surfaces composed. The renderer owns a generic textured-quad
surface — upload pixels, draw alpha-blended quads in screen space — and the
`aether.text` capability turns a font file plus a string into those quads. You
load a font once, then draw a string every frame you want it on screen.

## 1. Load a font

Put a TTF in the `assets` namespace (the engine reads it through `aether.fs`),
then mail `aether.text.load_font` to the `aether.text` mailbox:

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
// send_mail → aether.text  (kind: aether.text.draw), fire-and-forget
{
  "font_id": 0,
  "text": "hello aether",
  "size_pixels": 32.0,
  "color": [1.0, 1.0, 1.0, 1.0],   // RGBA, linear
  "space": "Screen"
}
```

`Screen` lays the string out in window pixels starting at the top-left corner,
flowing left to right along the baseline. The capability rasterizes any glyph
it hasn't seen yet into its atlas, uploads just that glyph, and sends the quad
batch to `aether.render` the same tick — so the first frame a new glyph appears
costs one atlas upload and every frame after is a cache hit.

`color` is a linear RGBA multiplier over the glyph coverage: the alpha channel
scales the blend, so `[1, 0, 0, 1]` draws solid red text and `[1, 1, 1, 0.5]`
draws half-transparent white.

## 3. See it

From the MCP harness, `capture_frame` with the draw in `mails` renders the
string into the returned PNG:

```jsonc
// capture_frame
{
  "mails": [
    { "recipient_name": "aether.text", "kind_name": "aether.text.draw",
      "params": { "font_id": 0, "text": "hello aether", "size_pixels": 32.0,
                  "color": [1.0, 1.0, 1.0, 1.0], "space": "Screen" } }
  ]
}
```

## What it does not do yet

- **Screen text anchors at the top-left.** There is no per-draw screen origin in
  the vocabulary yet; the string starts at pixel `(0, 0)`. Position beyond that
  rides the `World { anchor, scale }` space for above-the-head labels.
- **One font, one size, one run per `draw`.** No shaping, bidirectional text, or
  emoji — the layout is fontdue's horizontal advance metrics.
- **The atlas does not evict.** When it fills, further new glyphs log and drop
  for the session.

All of these sit behind the `aether.text.*` kinds, so the internals can grow
without changing the mail you send.
