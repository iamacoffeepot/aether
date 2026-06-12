# ADR-0105: Text Rendering

- **Status:** Proposed
- **Date:** 2026-06-12

## Context

The render surface is colored triangles (`aether.draw_triangle`, position + color vertices) under a single `view_proj` uniform published as `aether.camera` (ADR-0066). There is no texture, sampler, or screen-space machinery anywhere in the render path. We need text: load TTF fonts at runtime and draw them both as screen-space UI and as world-anchored labels above characters under a perspective camera.

The renderer's eventual architecture is deliberately undecided — we are still discovering the shape of our rendering requirements, so the commitment to make now is the mail vocabulary, not the implementation behind it. A design that needs the full renderer answered first is the wrong shape; a design whose interface survives a renderer rework is the right one.

## Decision

Text is built from two independent surfaces. The render capability gains a small generic texture surface — upload pixels, draw textured alpha-blended quads in either projection — and text is a separate capability that composes it. The split is load-bearing: textured quads are the part every future rendering need shares (sprites, HUD images, particles), so that vocabulary is worth committing to now, while the font machinery stays a CPU-only actor behind its own mailbox, swappable without touching the renderer.

### Render surface (kinds to the `aether.render` mailbox)

- `aether.render.create_texture { width, height, pixels }` → `aether.render.create_texture_result` (`Ok { texture_id }` / `Err { error }`). Pixels are RGBA8; `texture_id` is session-scoped, assigned by a registry the same way ADR-0103 assigns instrument ids.
- `aether.render.update_texture { texture_id, x, y, width, height, pixels }` — incremental sub-rect upload (atlas growth). Fire-and-forget; a bad id or out-of-bounds rect logs and drops.
- `aether.render.draw_textured_quads { texture_id, space, quads }` — each quad carries a pixel-unit rect, a uv rect, and an RGBA tint. Accumulated per frame with the same immediate-mode contract as `aether.draw_triangle`: send it every frame or it disappears. Drawn by a second pipeline with alpha blending.

`space` selects the projection:

- `Screen { }` — quad rects are window pixel coordinates, drawn in an overlay pass after the world pass under an ortho matrix derived from the surface size, no depth.
- `World { anchor: [f32; 3], scale }` — the shader transforms only the anchor through the existing `view_proj`, then applies quad offsets in clip space (`clip.xy += offset_px * 2 / viewport * k`). The quad always faces the camera and never skews, and the path needs nothing beyond the uniforms the renderer already has. `scale` picks `k`:
  - `Distance { reference_distance }` — `k` is a constant derived from the reference distance, so the perspective divide shrinks the text as the anchor recedes; `size_pixels` holds exactly at `reference_distance`. This is the above-the-head label mode.
  - `Pixels` — `k = clip.w`, cancelling the divide for constant on-screen size regardless of distance.

The headless chassis absorbs all three kinds with empty-body handlers on `HeadlessRenderCapability`, except `create_texture`, which replies `Err` (fail-fast, matching `capture_frame`).

### Text capability (the `aether.text` mailbox, in `aether-capabilities`)

A native capability with no GPU access — it only sends mail.

- `aether.text.load_font { namespace, path }` → `aether.text.load_font_result` (`Ok { font_id, name, resident_bytes }` / `Err { namespace, path, error }`). Mirrors `aether.audio.load_instrument`: park the request, fetch bytes via `aether.fs.read`, correlate on `aether.fs.read_result`, parse and rasterize off the hot path in a task handler, register under a session-scoped `font_id`.
- `aether.text.draw { font_id, text, size_pixels, color, space }` — fire-and-forget, immediate-mode per frame. The capability lays out glyphs, rasterizes any unseen `(font_id, glyph, size)` into its atlas (emitting one `update_texture`), and sends the quad batch to `aether.render` the same tick. `color` is RGBA; `space` is the render-surface discriminant above, passed through.

Rasterization uses `fontdue` (pure-Rust TTF parse + rasterize + horizontal layout). The atlas is a single shelf-packed RGBA8 texture with glyph coverage in alpha; when it fills, further new glyphs log and drop for the session.

### Non-goals for v1

Shaping, BiDi, and emoji; SDF/MSDF atlases; atlas eviction; retained text objects; in-plane world text that skews with the camera (a `world_size` variant can extend `scale` later). All sit behind the kind surface, so any of them can replace the internals without a vocabulary change.

## Consequences

- The renderer gains textures, a blended pipeline, and a screen-space overlay layer as general vocabulary — sprites and HUD images get a surface for free, and the overlay concept is established before any larger renderer design.
- World-anchored labels need no new camera kinds and no view-matrix decomposition; both scaling modes are one multiplier in the same shader.
- The per-frame immediate contract extends to quads and text, keeping one redraw model across the render surface.
- Implementation lands as three PRs: the render texture/quad surface, the text capability with `Screen` drawing, then the `World` anchor path. The tutorial ("load a TTF, draw text on screen and above a mesh") is drafted during design as the API sanity check.
- A future renderer rework replaces pipeline internals behind `aether.render.*` kinds; `aether.text` is unaffected.

## Alternatives considered

- **Triangulated glyph outlines through `aether.draw_triangle`** — works today with zero renderer change, but no antialiasing, heavy triangle counts at small sizes, and it answers neither screen-space nor scaling.
- **Text rasterization inside the render capability** — fewer moving parts, but couples font machinery to the GPU owner and the texture surface never materializes for other consumers.
- **SDF/MSDF atlas** — crisper scaling across sizes, but meaningfully more machinery; it is a drop-in internal replacement behind the same kinds if wanted later.
- **True in-world text quads for labels** — shrink with distance comes free, but they skew as the camera orbits; the clip-space `Distance` mode gives the shrink while staying camera-facing.
- **Retained text objects (create/update/destroy)** — avoids per-frame resends, but diverges from the established immediate-mode render contract; retained layering can come later if profiling demands it.
