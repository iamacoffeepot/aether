//! The `aether.text` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "runtime"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the `TextCapability`
//! identity never names these types nor pulls `fontdue` / `aether_substrate`.
//! The substrate-typed imports are gated once by this module rather than
//! line-by-line. The `#[runtime] impl NativeActor` and its handler bodies live
//! here beside the state they drive; the struct-hosted `#[actor(singleton)]` in
//! the parent reads this module off disk to lift the always-on identity.

use std::collections::HashMap;

pub use std::sync::Arc;

use aether_actor::DependsOn;
pub use aether_kinds::QuadSpace;
pub use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending, TaskDone};
pub use aether_substrate::chassis::error::BootError;
use aether_substrate::session_ids::SessionIds;

use crate::MEMORY_FONT_NAMESPACE;
use aether_data::Blob;
use aether_fs::FsError;
#[allow(unused_imports)]
pub use aether_fs::{FsCapability, NamespaceAddr, Read, ReadResult};
pub use aether_render::{
    CreateTexture, CreateTextureResult, RenderCapability, TextureFormat, TextureSampling, TextureUsage, TexturedQuad,
    UpdateTexture,
};

// ADR-0105 shelf-packed RGBA8 glyph atlas (`atlas`) and the pure layout /
// rasterization helpers (`layout`), now nested under this `runtime` directory
// so the one `mod runtime;` gate in the parent covers them (no per-sibling
// `#[cfg]`).
mod atlas;
mod layout;

// The atlas types the state struct + helpers name. Plain `use` (not a
// `pub use` re-export): the submodule items are `pub`, so a wider
// re-export is disallowed — the handler bodies in this module name atlas /
// layout symbols straight from `self::atlas` / `self::layout`.
use self::atlas::{ATLAS_SIZE, Atlas, AtlasEntry, GlyphKey, GlyphSlot};

/// Every request waiting on one font's read and parse, keyed in
/// [`TextCapabilityState::font_loads`] by the font's `(namespace, path)`
/// (ADR-0243 §9). Each list is typed by the reply its requests are owed:
/// `load_font` callers a `LoadFontResult`, `font_metrics` grabs that missed
/// the resident registry a `FontMetricsResult`. One read and one parse
/// answer them all.
#[derive(Default)]
pub struct FontWaiters {
    pub load: Vec<Held<LoadFontResult>>,
    pub metrics: Vec<Held<FontMetricsResult>>,
}

impl FontWaiters {
    /// Answer every waiter: each `load_font` with `load`, and each
    /// `font_metrics` with the reply `metrics` builds, built once and only
    /// when a grab waits.
    fn answer<A>(self, ctx: &mut NativeCtx<'_, A>, load: &LoadFontResult, metrics: impl FnOnce() -> FontMetricsResult) {
        for held in self.load {
            held.answer(ctx, load);
        }
        if self.metrics.is_empty() {
            return;
        }
        let metrics = metrics();
        for held in self.metrics {
            held.answer(ctx, &metrics);
        }
    }
}

/// Context stored under a font's `aether.fs.read` request correlation: the
/// font it reads, whose waiters sit in `font_loads`.
#[aether_data::kind(name = "aether.text.font_read")]
pub struct FontRead {
    pub namespace: String,
    pub path: String,
}

/// Context a font's staged parse carries into its task completion
/// (ADR-0243 §9): the font it parses, whose waiters sit in `font_loads`,
/// and the name a `LoadFontResult::Ok` reports.
#[aether_data::kind(name = "aether.text.font_parse")]
pub struct FontParse {
    pub namespace: String,
    pub path: String,
    pub name: String,
}

/// A successfully parsed font plus the byte length the reply reports as
/// `resident_bytes`.
pub struct ParsedFont {
    pub font: Arc<fontdue::Font>,
    pub resident_bytes: u64,
}

/// Off-hot-path parse outcome — `Err` carries the reason the cap relays
/// as `LoadFontResult::Err`.
pub type FontParseOutput = Result<ParsedFont, String>;

/// `aether.text` runtime state (ADR-0105). CPU-only — no GPU handles,
/// just the font registry and the glyph atlas. The dispatcher holds this
/// as the cap's state and routes
/// envelopes through the macro-emitted `Dispatch` impl; the addressing
/// identity is the distinct ZST [`super::TextCapability`]. Living in this
/// private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct TextCapabilityState {
    /// Session-scoped font registry. Index is the `font_id` a
    /// `LoadFontResult::Ok` handed back and `DrawText.font_id` names.
    pub fonts: HashMap<u32, Arc<fontdue::Font>>,
    /// Reverse index from `(namespace, path)` to the `font_id` that
    /// file is resident under. Dedups the registry: a repeat load or
    /// a `font_metrics` grab of the same file reuses one resident
    /// font and a stable id rather than parsing a second copy.
    pub font_id_by_path: HashMap<(String, String), u32>,
    /// Source of the `font_id`s handed back — monotonic, session-scoped.
    pub font_ids: SessionIds<u32>,
    /// Fonts whose read or parse is in flight, keyed by `(namespace,
    /// path)`, with every request waiting on each. A request for a font
    /// already here joins its waiters instead of starting another read.
    pub font_loads: HashMap<(String, String), FontWaiters>,
    /// The shelf-packed glyph atlas (CPU-side source of truth).
    pub atlas: Atlas,
    /// The render-cap `texture_id` backing [`Self::atlas`], once
    /// `create_texture` has replied. `None` until then.
    pub atlas_texture_id: Option<u32>,
    /// `true` between sending `create_texture` and its reply, so a
    /// burst of `draw`s sends exactly one creation request.
    pub atlas_create_inflight: bool,
}

impl TextCapabilityState {
    pub fn new() -> Self {
        Self {
            fonts: HashMap::new(),
            font_id_by_path: HashMap::new(),
            font_ids: SessionIds::new(),
            font_loads: HashMap::new(),
            atlas: Atlas::new(),
            atlas_texture_id: None,
            atlas_create_inflight: false,
        }
    }

    /// Register a parsed font under a session-scoped `font_id`,
    /// deduped by `(namespace, path)`: a path already resident
    /// returns its existing id (and drops the freshly-parsed `font`),
    /// so repeat loads and metric grabs of one file share a single
    /// resident font and a stable id. `None` once the session has run
    /// out of ids — the caller replies `Err` rather than aliasing a
    /// resident font.
    pub fn register_font(&mut self, namespace: &str, path: &str, font: Arc<fontdue::Font>) -> Option<u32> {
        let key = (namespace.to_owned(), path.to_owned());
        if let Some(&existing) = self.font_id_by_path.get(&key) {
            return Some(existing);
        }

        let font_id = self.font_ids.allocate()?;
        self.fonts.insert(font_id, font);
        self.font_id_by_path.insert(key, font_id);
        Some(font_id)
    }

    /// Join a request onto the waiters of the font at `(namespace, path)`.
    /// Returns `true` for the font's first waiter, whose caller starts the
    /// font's read or parse; a later waiter shares that one.
    pub fn join_font_load(&mut self, namespace: &str, path: &str, join: impl FnOnce(&mut FontWaiters)) -> bool {
        let key = (namespace.to_owned(), path.to_owned());
        let first = !self.font_loads.contains_key(&key);
        join(self.font_loads.entry(key).or_default());
        first
    }

    /// Forward an `aether.fs.read` for a font to the single fs resolver
    /// (ADR-0041), parking a [`FontRead`] naming the font under the read's
    /// correlation. The `ReadResult` routes back to `on_read_result`, which
    /// takes the context back and finds the font's waiters by it.
    pub fn forward_font_read<A: DependsOn<FsCapability>>(ctx: &mut NativeCtx<'_, A>, namespace: String, path: String) {
        let addr = NamespaceAddr::new(namespace.clone(), path.clone());
        let _ = ctx.send_with_context::<FsCapability>(&Read { addr }, FontRead { namespace, path });
    }

    /// Stage the parse of a font's `bytes` off the hot path (ADR-0243 §9).
    /// Its completion, `on_font_parsed`, takes the [`FontParse`] context and
    /// answers the font's waiters.
    pub fn stage_font_parse<A>(ctx: &mut NativeCtx<'_, A>, parse: FontParse, bytes: Blob) {
        ctx.stage_blocking_with::<FontParseOutput, FontParse>(parse).start(ctx, move || parse_font_bytes(&bytes));
    }

    /// Send `create_texture` for the zeroed atlas, unless a creation is
    /// already in flight. The reply (`CreateTextureResult`) routes back
    /// to this cap's own mailbox, where `on_create_texture_result`
    /// stores the assigned id.
    pub fn ensure_atlas_texture<A: DependsOn<RenderCapability>>(&mut self, ctx: &mut NativeCtx<'_, A>) {
        if self.atlas_texture_id.is_some() || self.atlas_create_inflight {
            return;
        }
        let create = CreateTexture {
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: Blob::from(self.atlas.pixels().to_vec()),
        };
        // Address the render cap through the lineage-correct resolver
        // (ADR-0099); `send` propagates this handler's chain by default
        // so the `CreateTextureResult` reply settles back into it.
        ctx.send::<RenderCapability>(&create);
        self.atlas_create_inflight = true;
    }

    /// Send one `update_texture` for a newly-rasterized glyph's rect.
    pub fn upload_glyph<A: DependsOn<RenderCapability>>(
        &self,
        ctx: &mut NativeCtx<'_, A>,
        texture_id: u32,
        entry: &AtlasEntry,
    ) {
        let update = UpdateTexture {
            texture_id,
            x: entry.x,
            y: entry.y,
            width: entry.width,
            height: entry.height,
            pixels: self.atlas.rect_rgba(entry),
        };
        ctx.send::<RenderCapability>(&update);
    }

    /// Re-sync the GPU side after an atlas reset by uploading the full
    /// zeroed buffer. This ensures the render cap's staged pixels are a
    /// clean mirror of the reset CPU atlas before per-glyph uploads layer
    /// on top. Uses the same `update_texture` path as `upload_glyph`.
    pub fn resync_atlas<A: DependsOn<RenderCapability>>(&self, ctx: &mut NativeCtx<'_, A>, texture_id: u32) {
        let update = UpdateTexture {
            texture_id,
            x: 0,
            y: 0,
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            pixels: self.atlas.pixels().to_vec(),
        };
        ctx.send::<RenderCapability>(&update);
    }

    /// Resolve a text item's font and reject invalid pixel sizes. An unknown
    /// font follows the established warn-drop behavior for `DrawText`.
    fn font_for_draw(&self, item: &DrawText) -> Option<Arc<fontdue::Font>> {
        let Some(font) = self.fonts.get(&item.font_id).cloned() else {
            tracing::warn!(
                target: "aether_substrate::text",
                font_id = item.font_id,
                "draw for unknown font_id; dropping",
            );
            return None;
        };
        if !(item.size_pixels.is_finite() && item.size_pixels > 0.0) {
            return None;
        }
        Some(font)
    }

    /// Return the live atlas texture, lazily creating it when needed and
    /// resetting a saturated atlas before the caller lays out its items.
    fn atlas_texture_for_draw<A: DependsOn<RenderCapability>>(&mut self, ctx: &mut NativeCtx<'_, A>) -> Option<u32> {
        let Some(texture_id) = self.atlas_texture_id else {
            // No atlas texture yet — kick off creation; immediate mode
            // resends this draw next frame once the id lands.
            self.ensure_atlas_texture(ctx);
            return None;
        };

        // Reset the atlas when full so the frame's glyphs can re-pack
        // from a clean slate. The render cap's staged buffer is re-synced
        // with one full-rect upload; per-glyph uploads follow as cache
        // misses. This costs one frame of partial text (the overflow
        // glyphs missing on the saturating frame) and then fully recovers.
        if self.atlas.is_full() {
            tracing::info!(
                target: "aether_substrate::text",
                "glyph atlas full; resetting for next frame",
            );
            self.atlas.reset();
            self.resync_atlas(ctx, texture_id);
        }

        Some(texture_id)
    }

    /// Lay out one text item, returning newly-rasterized atlas entries so the
    /// caller can preserve upload-before-use ordering at its send site.
    fn layout_text_item(&mut self, font: &fontdue::Font, item: &DrawText) -> (Vec<TexturedQuad>, Vec<AtlasEntry>) {
        let size = item.size_pixels;
        // Quantize the size for the glyph cache key — two draws at the
        // same nominal size share one raster.
        let size_key = quantize_size(size);
        let baseline = font.horizontal_line_metrics(size).map_or(size, |line| line.ascent);

        let mut pen_x = 0.0f32;
        let mut quads: Vec<TexturedQuad> = Vec::new();
        let mut uploads: Vec<AtlasEntry> = Vec::new();

        for ch in item.text.chars() {
            let glyph_index = font.lookup_glyph_index(ch);
            let metrics = font.metrics(ch, size);
            let key = GlyphKey { font_id: item.font_id, glyph_index, size_pixels: size_key };
            let (glyph_width, glyph_height) = glyph_dimensions(&metrics);

            // Rasterize only on a cache miss.
            let slot = if let Some(hit) = self.atlas.cached(&key) {
                hit
            } else {
                let (_m, coverage) = font.rasterize(ch, size);
                self.atlas.get_or_insert(key, glyph_width, glyph_height, &coverage)
            };

            match slot {
                GlyphSlot::Placed { entry, uploaded } => {
                    if uploaded {
                        uploads.push(entry);
                    }
                    quads.push(glyph_quad(&metrics, pen_x, baseline, &entry, item.color));
                }
                // Empty: no pixels, just advance the pen.
                // Full: the atlas saturated during this frame's layout pass;
                // the reset fires at the top of the next draw so this
                // glyph will re-pack and render then.
                GlyphSlot::Empty | GlyphSlot::Full => {}
            }
            pen_x += metrics.advance_width;
        }

        if matches!(&item.space, QuadSpace::World { .. }) {
            // World quads carry pixel offsets relative to the anchor, not
            // absolute screen positions. Center the string horizontally and
            // shift so the baseline sits at y=0 — the anchor is the baseline
            // point, and text appears above it (negative y in screen y-down
            // convention = above the anchor in world space).
            let half_width = pen_x / 2.0;
            for quad in &mut quads {
                quad.x -= half_width;
                quad.y -= baseline;
            }
        } else {
            // Screen quads flow from the top-left of the window by default
            // (pen starts at 0,0). Apply the caller's origin offset so a
            // string can sit at an arbitrary screen pixel.
            let [origin_x, origin_y] = item.origin;
            for quad in &mut quads {
                quad.x += origin_x;
                quad.y += origin_y;
            }
        }

        (quads, uploads)
    }
}

/// One pending contiguous text quad run. Its key is the projection plus
/// framebuffer clip; the atlas texture is shared by the text capability.
struct TextQuadRun {
    space: QuadSpace,
    clip: Option<aether_kinds::ClipRect>,
    quads: Vec<TexturedQuad>,
}

// The cap mail kinds (`LoadFont`, `DrawText`, …) plus the layout helpers the
// moved handler bodies name. The `#[runtime]` attribute emits the gated native
// runtime surface for the struct-hosted identity in the parent.
use self::layout::{build_font_metrics, emit_draw, font_name_from_path, glyph_dimensions, glyph_quad, quantize_size};
use super::TextCapability;
use super::kinds::{
    DrawText, DrawTextBatch, FontMetricsRequest, FontMetricsResult, FontRef, LoadFont, LoadFontBytes, LoadFontResult,
};
use aether_actor::runtime;

/// The error text a failed `aether.fs.read` relays to the font's requester.
fn read_failed(error: &FsError) -> String {
    format!("file read failed: {error:?}")
}

fn parse_font_bytes(blob: &Blob) -> FontParseOutput {
    let Some(bytes) = blob.contiguous() else {
        return Err("font bytes are not resident in this process".to_owned());
    };

    match fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
        Ok(font) => Ok(ParsedFont { font: Arc::new(font), resident_bytes: bytes.len() as u64 }),
        Err(e) => Err(format!("font parse failed: {e}")),
    }
}

#[runtime]
impl NativeActor for TextCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// font registry and glyph atlas.
    type State = TextCapabilityState;

    type Config = ();

    /// ADR-0105 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.text";

    /// No substrate resources to claim — the cap holds only CPU state.
    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<TextCapabilityState, BootError> {
        Ok(TextCapabilityState::new())
    }

    /// Load a font from a TTF file.
    ///
    /// # Agent
    /// Reply: `LoadFontResult`. The cap forwards an `aether.fs.read`
    /// for `namespace://path`, parses the TTF off the hot path, and
    /// replies `Ok { font_id, name, resident_bytes }` once registered
    /// or `Err` with the failure reason (bad path, or an unparseable
    /// file). The `font_id` is session-scoped — thread it into `draw`.
    #[handler::request]
    fn on_load_font(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: LoadFont) -> Pending<LoadFontResult> {
        let (pending, held) = ctx.hold::<LoadFontResult>();
        if state.join_font_load(&mail.namespace, &mail.path, |waiters| waiters.load.push(held)) {
            TextCapabilityState::forward_font_read(ctx, mail.namespace, mail.path);
        }
        pending
    }

    /// Load a font from TTF bytes carried in the request payload.
    ///
    /// # Agent
    /// Reply: `LoadFontResult`. The cap parses the supplied bytes off the hot
    /// path and registers the font under the memory namespace keyed by `name`.
    /// This avoids requiring a component with an embedded fallback font to
    /// write that font through `aether.fs` before loading it. A load of a
    /// `name` whose parse is already in flight joins it and is answered by
    /// that parse.
    #[handler::request]
    fn on_load_font_bytes(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: LoadFontBytes,
    ) -> Pending<LoadFontResult> {
        let (pending, held) = ctx.hold::<LoadFontResult>();
        if state.join_font_load(MEMORY_FONT_NAMESPACE, &mail.name, |waiters| waiters.load.push(held)) {
            let parse =
                FontParse { namespace: MEMORY_FONT_NAMESPACE.to_owned(), path: mail.name.clone(), name: mail.name };
            TextCapabilityState::stage_font_parse(ctx, parse, Blob::from(mail.bytes));
        }
        pending
    }

    /// Grab a font's size-independent metric table.
    ///
    /// # Agent
    /// Reply: `FontMetricsResult`. `font` references the font by a
    /// session-scoped `font_id` or by `aether.fs` `namespace` /
    /// `path`. A resident font (by id, or a path already loaded)
    /// replies `Ok` synchronously this turn. An unresident path loads
    /// on the miss — forwarding an `aether.fs.read`, parsing off the
    /// hot path, and replying `Ok` once registered (the font is then
    /// addressable by the assigned id too) or `Err` on a bad path /
    /// unparseable file. An unknown `font_id` replies `Err`.
    #[handler::request]
    fn on_font_metrics(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: FontMetricsRequest,
    ) -> Pending<FontMetricsResult> {
        let (pending, held) = ctx.hold::<FontMetricsResult>();
        match mail.font {
            FontRef::Id(font_id) => {
                let reply = state.fonts.get(&font_id).map_or_else(
                    || FontMetricsResult::Err { error: format!("unknown font_id {font_id}") },
                    |font| FontMetricsResult::Ok { metrics: build_font_metrics(font) },
                );
                held.answer(ctx, &reply);
            }
            FontRef::Path { namespace, path } => match state.font_id_by_path.get(&(namespace.clone(), path.clone())) {
                // Already resident — measure from the cached font now, no fs
                // round trip.
                Some(&font_id) => {
                    held.answer(ctx, &FontMetricsResult::Ok { metrics: build_font_metrics(&state.fonts[&font_id]) });
                }
                // Load on the miss, or join a load already in flight;
                // `on_font_parsed` answers once the font is parsed and
                // registered.
                None => {
                    if state.join_font_load(&namespace, &path, |waiters| waiters.metrics.push(held)) {
                        TextCapabilityState::forward_font_read(ctx, namespace, path);
                    }
                }
            },
        }
        pending
    }

    /// Correlate a forwarded `aether.fs.read` reply with the font its
    /// request context names. `Ok` stages the font's parse off the hot path,
    /// whose completion answers the font's waiters; `Err` answers every
    /// waiter with the fs error, each in the shape its request is owed.
    #[handler::response]
    fn on_read_result(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: ReadResult,
        FontRead { namespace, path }: FontRead,
    ) {
        match mail {
            ReadResult::Ok { bytes, .. } => {
                let name = font_name_from_path(&path);
                TextCapabilityState::stage_font_parse(ctx, FontParse { namespace, path, name }, bytes);
            }
            ReadResult::Err { error, .. } => {
                let Some(waiters) = state.font_loads.remove(&(namespace.clone(), path.clone())) else {
                    return;
                };
                let error = read_failed(&error);
                let metrics_error = error.clone();
                waiters.answer(ctx, &LoadFontResult::Err { namespace, path, error }, || FontMetricsResult::Err {
                    error: metrics_error,
                });
            }
        }
    }

    /// Font-parse completion (ADR-0243 §9). On success register the
    /// parsed font once (deduped by path) and answer every request waiting
    /// on it in the shape it is owed — `LoadFontResult::Ok` for each
    /// `load_font`, `FontMetricsResult::Ok` for each `font_metrics` grab;
    /// on a parse failure answer each with the matching `Err`.
    #[handler(task)]
    fn on_font_parsed(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<FontParseOutput>) {
        let Some(FontParse { namespace, path, name }) = ctx.take_context() else {
            return;
        };
        let Some(waiters) = state.font_loads.remove(&(namespace.clone(), path.clone())) else {
            return;
        };

        let registered = done.into_output().and_then(|ParsedFont { font, resident_bytes }| {
            state
                .register_font(&namespace, &path, Arc::clone(&font))
                .map(|font_id| (font, font_id, resident_bytes))
                .ok_or_else(|| "this session has run out of font ids".to_owned())
        });

        match registered {
            Ok((font, font_id, resident_bytes)) => {
                tracing::info!(
                    target: "aether_substrate::text",
                    font_id,
                    name = %name,
                    resident_bytes,
                    "font loaded",
                );
                waiters.answer(ctx, &LoadFontResult::Ok { font_id, name, resident_bytes }, || FontMetricsResult::Ok {
                    metrics: build_font_metrics(&font),
                });
            }
            Err(error) => {
                let metrics_error = error.clone();
                waiters.answer(ctx, &LoadFontResult::Err { namespace, path, error }, || FontMetricsResult::Err {
                    error: metrics_error,
                });
            }
        }
    }

    /// Store the atlas `texture_id` once `create_texture` replies. The
    /// cap creates exactly one texture, so the single reply is always
    /// its atlas — no correlation key needed.
    #[handler::response]
    fn on_create_texture_result(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: CreateTextureResult) {
        state.atlas_create_inflight = false;
        match mail {
            CreateTextureResult::Ok { texture_id } => {
                state.atlas_texture_id = Some(texture_id);
            }
            CreateTextureResult::Err { error } => {
                tracing::error!(
                    target: "aether_substrate::text",
                    error = %error,
                    "text atlas create_texture failed; text will not draw",
                );
            }
        }
    }

    /// Lay out and draw a string in immediate mode.
    ///
    /// # Agent
    /// Fire-and-forget. Rasterizes any unseen glyph into the atlas
    /// (one `update_texture` each) and sends the `draw_textured_quads`
    /// batch to `aether.render` the same tick. An unknown `font_id`
    /// warn-drops. When the atlas is full it is reset at the top of this
    /// call: the GPU side is re-synced with one full-rect `update_texture`
    /// and all glyphs for this frame are re-rasterized as cache misses.
    /// The cost is at most one frame of partial text on the saturating
    /// frame; the next frame recovers fully. The first `draw` lazily
    /// creates the atlas texture and draws nothing until the reply lands —
    /// resend every frame (immediate-mode contract).
    #[handler::tell]
    fn on_draw_text(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: DrawText) {
        let Some(font) = state.font_for_draw(&mail) else {
            return;
        };
        let Some(texture_id) = state.atlas_texture_for_draw(ctx) else {
            return;
        };
        let (quads, uploads) = state.layout_text_item(&font, &mail);
        for entry in uploads {
            state.upload_glyph(ctx, texture_id, &entry);
        }
        if !quads.is_empty() {
            emit_draw(ctx, texture_id, mail.space, mail.clip, quads);
        }
    }

    /// Lay out and draw an authored sequence of text items in immediate mode.
    /// Adjacent items with the same projection and clip share one textured-quad
    /// send; every other transition preserves the authored order as a separate
    /// run. Each item's glyph uploads are sent before any subsequent run
    /// flush, preserving `aether.render` FIFO upload-before-use ordering.
    #[handler::tell]
    fn on_draw_batch(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: DrawTextBatch) {
        let Some(texture_id) = state.atlas_texture_for_draw(ctx) else {
            return;
        };

        let mut pending: Option<TextQuadRun> = None;
        for item in &mail.items {
            let Some(font) = state.font_for_draw(item) else {
                continue;
            };
            let (quads, uploads) = state.layout_text_item(&font, item);
            for entry in uploads {
                state.upload_glyph(ctx, texture_id, &entry);
            }
            if quads.is_empty() {
                continue;
            }

            if let Some(run) = &mut pending
                && run.space == item.space
                && run.clip == item.clip
            {
                run.quads.extend(quads);
            } else {
                if let Some(run) = pending.take() {
                    emit_draw(ctx, texture_id, run.space, run.clip, run.quads);
                }
                pending = Some(TextQuadRun { space: item.space.clone(), clip: item.clip.clone(), quads });
            }
        }

        if let Some(run) = pending {
            emit_draw(ctx, texture_id, run.space, run.clip, run.quads);
        }
    }
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::*;
    use super::atlas::{ATLAS_SIZE, GlyphKey, GlyphSlot};
    use super::layout::build_font_metrics;
    use super::{
        Arc, CreateTexture, CreateTextureResult, FsCapability, QuadSpace, RenderCapability, TextCapabilityState,
        UpdateTexture,
    };
    use aether_actor::{Addressable, HandlesKind};
    use aether_data::{Kind, KindId};
    use aether_math::Rgba;
    use aether_render::DrawTexturedQuads;
    use aether_substrate::mail::registry::OwnedDispatch;
    use aether_substrate::testing::{PumpedDriver, boot_bare_test_chassis, fresh_substrate, registered_ref};
    use std::sync::mpsc::{self, Receiver};

    /// A booted `aether.text` on a pumped slot, beside the two dependencies
    /// it declares: a stand-in at `aether.render` that hands every mail it
    /// receives to [`Self::render`], and a discarding stand-in at
    /// `aether.fs`. Every mail reaches the cap through the chassis and runs
    /// through production dispatch when the slot drains.
    struct TextFixture {
        cap: PumpedDriver<TextCapability>,
        render: Receiver<(KindId, Vec<u8>)>,
    }

    impl TextFixture {
        fn boot() -> Self {
            let (registry, mailer) = fresh_substrate();
            let (render_tx, render) = mpsc::channel();
            let render_mailer = Arc::clone(&mailer);
            registered_ref(
                &registry,
                RenderCapability::NAMESPACE,
                Arc::new(move |dispatch: OwnedDispatch| {
                    let _ = render_tx.send((dispatch.kind, dispatch.payload.bytes().to_vec()));
                    render_mailer.record_finished(dispatch.mail_id, dispatch.root);
                    dispatch.discharge();
                }),
            );
            registered_ref(
                &registry,
                FsCapability::NAMESPACE,
                Arc::new(|dispatch: OwnedDispatch| dispatch.discharge()),
            );

            let chassis = boot_bare_test_chassis(&registry, &mailer);
            let cap = PumpedDriver::boot(chassis, (), ());
            Self { cap, render }
        }

        /// [`Self::boot`] with the vendored font resident as `font_id` 0,
        /// seeded in a host turn: the font-load path is covered by the
        /// text scenarios, and the draw tests start past it.
        fn with_font() -> Self {
            let mut fixture = Self::boot();
            fixture
                .cap
                .host_turn(|state, _ctx| {
                    state.fonts.insert(0, Arc::new(test_font()));
                })
                .expect("the booted slot takes a host turn");
            fixture
        }

        /// [`Self::with_font`] whose atlas texture has been created: the
        /// render cap's `CreateTextureResult` arrives as mail.
        fn with_atlas(texture_id: u32) -> Self {
            let mut fixture = Self::with_font();
            fixture.send(&CreateTextureResult::Ok { texture_id });
            fixture
        }

        /// Deliver `mail` to the cap as a tracked chassis root and pump the
        /// slot until that root's chain settles: the cap has handled it and
        /// the render stand-in has received everything it sent.
        fn send<K: Kind>(&mut self, mail: &K)
        where
            TextCapability: HandlesKind<K>,
        {
            self.cap.send_and_settle(self.cap.chassis().actor_ref::<TextCapability>(), mail, None);
        }

        fn draw(&mut self, font_id: u32, text: &str, size_pixels: f32, origin: [f32; 2]) {
            self.send(&DrawText { font_id, size_pixels, ..screen_text_item(text, origin, None) });
        }

        /// Every mail the render stand-in has received since the last read,
        /// in arrival order.
        fn render_mail(&self) -> Vec<(KindId, Vec<u8>)> {
            self.render.try_iter().collect()
        }

        fn render_kinds(&self) -> Vec<KindId> {
            self.render_mail().into_iter().map(|(kind, _)| kind).collect()
        }

        /// The `DrawTexturedQuads` batches among the render mail since the
        /// last read, skipping the uploads beside them.
        fn quad_batches(&self) -> Vec<DrawTexturedQuads> {
            self.render_mail()
                .into_iter()
                .filter(|(kind, _)| *kind == DrawTexturedQuads::ID)
                .map(|(_, payload)| decode(&payload))
                .collect()
        }
    }

    fn decode<K: Kind>(payload: &[u8]) -> K {
        K::decode_from_bytes(payload).expect("test: render mail decodes")
    }

    fn screen_text_item(text: &str, origin: [f32; 2], clip: Option<aether_kinds::ClipRect>) -> DrawText {
        DrawText {
            font_id: 0,
            text: text.to_owned(),
            size_pixels: 24.0,
            color: Rgba::new(1.0, 1.0, 1.0, 1.0),
            origin,
            space: QuadSpace::Screen,
            clip,
        }
    }

    /// Catches a draw for an unknown font that still reaches `aether.render`,
    /// by creating the atlas or sending an empty batch.
    #[test]
    fn draw_with_unknown_font_emits_nothing() {
        let mut text = TextFixture::boot();

        text.draw(99, "hi", 32.0, [0.0, 0.0]);

        assert!(text.render_mail().is_empty(), "an unknown font_id must not emit any render mail");
    }

    /// Catches a lazy atlas create that draws before the texture exists, or
    /// that a burst of draws repeats while the first create is in flight.
    #[test]
    fn first_draw_with_known_font_creates_the_atlas_texture_once() {
        let mut text = TextFixture::with_font();

        text.draw(0, "hi", 32.0, [0.0, 0.0]);
        text.draw(0, "hi", 32.0, [0.0, 0.0]);

        assert_eq!(text.render_kinds(), [CreateTexture::ID], "one create, and no draw until its reply lands");
        let (inflight, texture) = text
            .cap
            .read_state(|state| (state.atlas_create_inflight, state.atlas_texture_id))
            .expect("the slot is live");
        assert!(inflight, "the create stays in flight until its reply");
        assert_eq!(texture, None, "no texture id until create_texture replies");
    }

    /// Catches a `CreateTextureResult` whose id is not the one later uploads
    /// and draws name, or a first glyph that is drawn without its upload.
    #[test]
    fn draw_after_texture_ready_emits_update_and_quads() {
        let mut text = TextFixture::with_font();
        text.draw(0, "A", 48.0, [0.0, 0.0]);
        assert_eq!(text.render_kinds(), [CreateTexture::ID]);

        text.send(&CreateTextureResult::Ok { texture_id: 7 });
        text.draw(0, "A", 48.0, [0.0, 0.0]);

        let [(update_kind, update), (draw_kind, draw)] =
            <[_; 2]>::try_from(text.render_mail()).expect("a new glyph sends one upload, then one quad batch");
        assert_eq!((update_kind, draw_kind), (UpdateTexture::ID, DrawTexturedQuads::ID));
        assert_eq!(decode::<UpdateTexture>(&update).texture_id, 7, "the upload names the created atlas");
        assert_eq!(decode::<DrawTexturedQuads>(&draw).texture_id, 7, "the batch samples the created atlas");
    }

    /// Catches a full atlas that drops the glyph instead of resetting: the
    /// reset re-syncs the whole texture, then the glyph uploads and draws.
    #[test]
    fn draw_after_atlas_full_resets_and_renders_glyph() {
        let mut text = TextFixture::with_atlas(3);
        text.cap
            .host_turn(|state, _ctx| {
                let band_height = 64u32;
                let coverage = vec![255u8; (ATLAS_SIZE * band_height) as usize];
                for glyph_index in 0..32u16 {
                    let key = GlyphKey { font_id: 99, glyph_index, size_pixels: 64 };
                    match state.atlas.get_or_insert(key, ATLAS_SIZE, band_height, &coverage) {
                        GlyphSlot::Placed { .. } => {}
                        GlyphSlot::Full => break,
                        GlyphSlot::Empty => panic!("band coverage is not empty"),
                    }
                }
            })
            .expect("the slot is live");
        assert!(text.cap.read_state(|state| state.atlas.is_full()).expect("the slot is live"), "atlas starts full");

        text.draw(0, "A", 48.0, [0.0, 0.0]);

        assert!(!text.cap.read_state(|state| state.atlas.is_full()).expect("the slot is live"), "the draw reset it");
        let mail = text.render_mail();
        let kinds: Vec<KindId> = mail.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(kinds, [UpdateTexture::ID, UpdateTexture::ID, DrawTexturedQuads::ID]);
        let resync = decode::<UpdateTexture>(&mail[0].1);
        assert_eq!((resync.width, resync.height), (ATLAS_SIZE, ATLAS_SIZE), "the reset re-syncs the whole atlas");
    }

    /// Catches a batch that sends one quad batch per item instead of
    /// coalescing same-key items, or that reorders their glyphs.
    #[test]
    fn draw_batch_coalesces_same_key_screen_items_into_one_quad_send() {
        let mut text = TextFixture::with_atlas(1);

        text.send(&DrawTextBatch {
            items: vec![screen_text_item("A", [0.0, 0.0], None), screen_text_item("B", [24.0, 0.0], None)],
        });

        let batches = text.quad_batches();
        assert_eq!(batches.len(), 1, "two same-key text items emit one DrawTexturedQuads");
        assert_eq!(batches[0].space, QuadSpace::Screen);
        assert_eq!(batches[0].clip, None);
        assert_eq!(batches[0].quads.len(), 2);
        assert!(batches[0].quads[0].x < batches[0].quads[1].x, "glyph quads retain item order");
    }

    /// Catches a coalescer that merges equal clips across an intervening
    /// different clip, which would reorder the authored runs.
    #[test]
    fn draw_batch_preserves_noncontiguous_clip_runs() {
        let mut text = TextFixture::with_atlas(1);
        let left_clip = aether_kinds::ClipRect { x: 0.0, y: 0.0, width: 20.0, height: 20.0 };
        let right_clip = aether_kinds::ClipRect { x: 20.0, y: 0.0, width: 20.0, height: 20.0 };

        text.send(&DrawTextBatch {
            items: vec![
                screen_text_item("A", [0.0, 0.0], Some(left_clip.clone())),
                screen_text_item("B", [24.0, 0.0], Some(right_clip.clone())),
                screen_text_item("C", [48.0, 0.0], Some(left_clip.clone())),
            ],
        });

        let batches = text.quad_batches();
        assert_eq!(batches.len(), 3, "noncontiguous equal clips remain separate authored-order runs");
        assert_eq!(batches[0].clip, Some(left_clip.clone()));
        assert_eq!(batches[1].clip, Some(right_clip));
        assert_eq!(batches[2].clip, Some(left_clip));
    }

    /// Catches an unknown-font item that aborts the batch or splits the run
    /// around it.
    #[test]
    fn draw_batch_drops_an_unknown_font_without_losing_surrounding_items() {
        let mut text = TextFixture::with_atlas(1);
        let unknown = DrawText { font_id: 99, ..screen_text_item("ignored", [24.0, 0.0], None) };

        text.send(&DrawTextBatch {
            items: vec![screen_text_item("A", [0.0, 0.0], None), unknown, screen_text_item("B", [48.0, 0.0], None)],
        });

        let batches = text.quad_batches();
        assert_eq!(batches.len(), 1, "valid items on either side share their run");
        assert_eq!(batches[0].quads.len(), 2, "the unknown-font item alone is dropped");
        assert!(batches[0].quads[0].x < batches[0].quads[1].x, "surviving items retain authored order");
    }

    /// Catches a `Screen` draw that ignores `origin`: the same string drawn
    /// at `[0,0]` and at `[ox, oy]` has every quad shifted by exactly that
    /// offset.
    #[test]
    fn screen_origin_shifts_quad_positions() {
        let mut text = TextFixture::with_atlas(1);

        text.draw(0, "A", 24.0, [0.0, 0.0]);
        let quads_zero = text.quad_batches().remove(0).quads;
        let ox = 30.0f32;
        let oy = 50.0f32;
        text.draw(0, "A", 24.0, [ox, oy]);
        let quads_offset = text.quad_batches().remove(0).quads;

        assert_eq!(quads_zero.len(), quads_offset.len(), "same text must produce the same number of quads");
        for (z, o) in quads_zero.iter().zip(quads_offset.iter()) {
            assert!((o.x - z.x - ox).abs() < 0.01, "quad x should shift by {ox}: zero={}, offset={}", z.x, o.x);
            assert!((o.y - z.y - oy).abs() < 0.01, "quad y should shift by {oy}: zero={}, offset={}", z.y, o.y);
        }
    }

    /// A tiny real font for the draw-path tests — the workspace's
    /// vendored OFL Roboto Mono, the same asset the e2e scenario uses.
    fn test_font() -> fontdue::Font {
        fontdue::Font::from_bytes(
            include_bytes!("../../../aether-text/assets/fonts/RobotoMono.ttf").as_slice(),
            fontdue::FontSettings::default(),
        )
        .expect("test setup: vendored Roboto Mono parses")
    }

    /// `build_font_metrics`'s table scales back to fontdue's draw-path
    /// advance exactly — per glyph and as a run's advance sum — via
    /// the same `scale_units` the guest uses. This is the invariant
    /// the grab rests on: a cached size-independent table reproduces
    /// the cap's layout without re-querying.
    #[test]
    fn font_metrics_table_matches_fontdue_draw_advances() {
        use std::collections::HashMap;

        let font = test_font();
        let metrics = build_font_metrics(&font);
        let by_codepoint: HashMap<u32, f32> =
            metrics.advances.iter().map(|glyph| (glyph.codepoint, glyph.advance_units)).collect();
        let advance_units = |ch: char| by_codepoint.get(&u32::from(ch)).copied().unwrap_or(metrics.default_advance);

        let size = 37.0;
        for ch in "Hello, Aether! 0123".chars() {
            let local = aether_kinds::scale_units(advance_units(ch), size, metrics.units_per_em);
            let drawn = font.metrics(ch, size).advance_width;
            assert_eq!(local, drawn, "advance mismatch for {ch:?}");
        }

        // The advance SUM — a run's extent — matches the draw path's
        // pen walk (`pen_x += advance_width`).
        let mut local_pen = 0.0f32;
        let mut draw_pen = 0.0f32;
        for ch in "Aether".chars() {
            local_pen += aether_kinds::scale_units(advance_units(ch), size, metrics.units_per_em);
            draw_pen += font.metrics(ch, size).advance_width;
        }
        assert_eq!(local_pen, draw_pen);
    }

    /// `register_font` dedups by `(namespace, path)`: a repeat path
    /// reuses the resident id and keeps one resident font, while a
    /// different path gets a fresh id.
    #[test]
    fn register_font_dedups_repeat_path_to_one_id() {
        let mut state = TextCapabilityState::new();
        let first = state.register_font("assets", "font.ttf", Arc::new(test_font()));
        let again = state.register_font("assets", "font.ttf", Arc::new(test_font()));
        assert_eq!(first, again, "a repeat path must reuse the resident id");
        assert!(first.is_some(), "a fresh session has ids to hand out");
        assert_eq!(state.fonts.len(), 1, "only one resident font for the path");

        let other = state.register_font("assets", "other.ttf", Arc::new(test_font()));
        assert_ne!(other, first, "a different path gets a fresh id");
        assert_eq!(state.fonts.len(), 2);
    }
}
