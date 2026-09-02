//! `aether.text` mail kinds (ADR-0105, ADR-0121). The capability owns its
//! own mail contract: the `aether.text.*` kinds plus the `FontRef`
//! request param live here, beside the implementation that dispatches
//! them. Always-on and wasm-safe — they need only `aether-data` + `serde`
//! — so a wasm component can address the cap by type without pulling
//! `fontdue` into its graph. Their `inventory::submit!` descriptor entries
//! ride the `Kind` derive (`cfg(not(wasm32))`-gated), so
//! `aether_kinds::descriptors::all()` still surfaces them.
//!
//! Two value sub-types stay central in `aether-kinds`: `FontMetrics` and
//! `GlyphAdvance` are consumed by `aether_kinds::text_metrics`'s wasm-safe
//! scaling primitive, so moving them would form a crate cycle. The moved
//! kinds reference them via `use aether_kinds::{FontMetrics, QuadSpace}` —
//! the existing `capabilities → kinds` direction.

use aether_kinds::{ClipRect, FontMetrics, QuadSpace};
use aether_math::Rgba;
use serde::{Deserialize, Serialize};

/// Synthetic namespace used when a font is loaded directly from mail-carried
/// bytes rather than through `aether.fs`.
pub const MEMORY_FONT_NAMESPACE: &str = "memory";

/// `aether.text.load_font` — fetch a TTF through `aether.fs` and
/// register it under a session-scoped `font_id` (assigned the same
/// way ADR-0103 assigns instrument ids). `namespace` / `path` address
/// the file the same way `aether.fs.read` does (e.g. `"assets"` /
/// `"fonts/RobotoMono.ttf"`). The capability forwards the read,
/// parses the font off the hot path, and replies `LoadFontResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.load_font")]
pub struct LoadFont {
    pub namespace: String,
    pub path: String,
}

/// `aether.text.load_font_bytes` — parse and register a TTF supplied
/// directly in the request payload. This is for wasm components that
/// embed a small fallback font and need to register it without staging
/// through `aether.fs`. `name` is used as the memory-backed font key
/// and the human-readable name in `LoadFontResult::Ok`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.load_font_bytes")]
pub struct LoadFontBytes {
    pub name: String,
    #[serde(with = "aether_data::bytes")]
    pub bytes: Vec<u8>,
}

/// Reply to `LoadFont`. `Ok` carries the assigned `font_id` — thread
/// it into `DrawText.font_id` — the derived `name` (the file stem),
/// and `resident_bytes` (the parsed TTF's byte length). `Err` echoes
/// the `namespace` / `path` for diagnostics plus a human-readable reason
/// — a bad path, or a file fontdue could not parse as a font.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.load_font_result")]
pub enum LoadFontResult {
    Ok { font_id: u32, name: String, resident_bytes: u64 },
    Err { namespace: String, path: String, error: String },
}

/// `aether.text.draw` — lay out and draw `text` in the font named by
/// `font_id` at `size_pixels`, every frame the string should appear
/// (the same immediate-mode contract as `aether.draw_triangle`: send
/// it each frame or it vanishes). `color` is a linear RGBA multiplier
/// over the glyph coverage — the alpha channel scales the blend.
/// `origin` is the screen-pixel top-left the string flows from along
/// the baseline in `Screen` mode — `[0.0, 0.0]` is the window's
/// top-left corner, the same as the pre-origin behavior. In `World`
/// mode `origin` is ignored; the `anchor` positions the string there.
/// `space` selects the projection: `Screen` flows the string from
/// `origin` along the baseline; `World { anchor, scale }` anchors it
/// in the scene. An unknown `font_id` warn-drops. Fire-and-forget; no
/// reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.draw")]
pub struct DrawText {
    pub font_id: u32,
    pub text: String,
    pub size_pixels: f32,
    pub color: Rgba,
    /// Screen-pixel top-left the string flows from in `Screen` mode.
    /// `[0.0, 0.0]` is the window's top-left corner. Ignored in
    /// `World` mode — the `anchor` positions there.
    pub origin: [f32; 2],
    pub space: QuadSpace,
    /// Optional framebuffer-pixel scissor applied to the emitted glyph
    /// quad batch. `None` leaves the text unclipped.
    pub clip: Option<ClipRect>,
    /// Overlay draw layer, forwarded onto the emitted
    /// `aether.render.draw_textured_quads`. `0` is the ordinary layer;
    /// the renderer records batches in ascending layer and, within one
    /// layer, in submission order. Glyphs reach `aether.render` one mail
    /// hop after a direct draw does, so a caller that wants to cover its
    /// own text raises the covering batch's layer rather than reordering
    /// sends.
    pub layer: u8,
}

/// `aether.text.draw_batch` — the batched form of [`DrawText`]. Every item
/// follows the same immediate-mode contract; the capability preserves vector
/// order while coalescing adjacent compatible glyph quad runs. Fire-and-
/// forget; no reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.draw_batch")]
pub struct DrawTextBatch {
    pub items: Vec<DrawText>,
}

/// Names the font a `FontMetricsRequest` measures: by the
/// session-scoped `font_id` a prior `LoadFont` (or metrics grab)
/// assigned, or by the `aether.fs` `namespace` / `path` of its TTF —
/// the latter loads the font on a miss the same way `LoadFont` does.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum FontRef {
    /// A session-scoped `font_id` from a prior load or grab.
    Id(u32),
    /// A TTF addressed the same way `aether.fs.read` addresses a file
    /// (e.g. `"assets"` / `"fonts/RobotoMono.ttf"`).
    Path { namespace: String, path: String },
}

/// `aether.text.font_metrics` — grab a font's complete,
/// size-independent `FontMetrics` table so a consumer measures text
/// locally and synchronously (fit-to-content sizing, caret placement,
/// hit-testing) without a per-measurement mail round trip. `font`
/// references the font by id or by path; an unresident path loads on
/// the miss, reusing the `aether.fs` fetch + parse path. The cap
/// replies `FontMetricsResult`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.font_metrics")]
pub struct FontMetricsRequest {
    pub font: FontRef,
}

/// Reply to `FontMetricsRequest`. `Ok` carries the resolved
/// `FontMetrics` table; `Err` carries a human-readable reason — an
/// unknown `font_id`, a bad path, or a file fontdue could not parse.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.text.font_metrics_result")]
pub enum FontMetricsResult {
    Ok { metrics: FontMetrics },
    Err { error: String },
}
