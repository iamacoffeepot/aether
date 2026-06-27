//! The `aether.text` runtime half (ADR-0122 identity/runtime split). Compiled
//! only under `feature = "text-native"` (the `mod runtime;` declaration in the
//! parent carries the gate), so a transport-only build of the `TextCapability`
//! identity never names these types nor pulls `fontdue` / `aether_substrate`.
//! The substrate-typed imports are gated once by this module rather than
//! line-by-line. The `#[runtime] impl NativeActor` and its handler bodies live
//! here beside the state they drive; the struct-hosted `#[actor(singleton)]` in
//! the parent reads this module off disk to lift the always-on identity.

use std::collections::HashMap;
use std::collections::VecDeque;

pub(super) use std::sync::Arc;

pub(super) use aether_actor::OutboundReply;
pub(super) use aether_data::Source;
pub(super) use aether_kinds::QuadSpace;
pub(super) use aether_substrate::Manual;
pub(super) use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
pub(super) use aether_substrate::chassis::error::BootError;

pub(super) use crate::fs::{FsCapability, Read, ReadResult};
pub(super) use crate::render::{
    CreateTexture, CreateTextureResult, RenderCapability, TexturedQuad, UpdateTexture,
};

// ADR-0105 shelf-packed RGBA8 glyph atlas (`atlas`) and the pure layout /
// rasterization helpers (`layout`), now nested under this `runtime` directory
// so the one `mod runtime;` gate in the parent covers them (no per-sibling
// `#[cfg]`).
mod atlas;
mod layout;

// The atlas types the state struct + helpers name. Plain `use` (not a
// `pub use` re-export): the submodule items are `pub(super)`, so a wider
// re-export is disallowed — the handler bodies in this module name atlas /
// layout symbols straight from `self::atlas` / `self::layout`.
use self::atlas::{ATLAS_SIZE, Atlas, AtlasEntry, GlyphKey, GlyphSlot};

/// Which reply shape a parked font request is owed once its font is
/// resident. `load_font` and the `font_metrics` grab share the
/// `aether.fs` fetch + parse path; this rides along so the completion
/// arm replies in the caller's shape.
#[derive(Clone, Copy)]
pub enum PendingReply {
    /// Reply `LoadFontResult` — the original `load_font` caller.
    LoadFont,
    /// Reply `FontMetricsResult` — a `font_metrics` grab that missed
    /// the resident registry and triggered a load.
    FontMetrics,
}

/// A font request parked while its `aether.fs.read` is in flight,
/// keyed in [`TextCapabilityState::pending_fonts`] by the echoed
/// `(namespace, path)`. Carries the original requester so the deferred
/// reply lands on the caller, plus the shape that reply takes.
pub struct PendingFont {
    pub source: Source,
    pub reply: PendingReply,
}

/// Context carried through the font-parse task so the completion arm
/// can shape the reply the parked request is owed.
pub struct FontParseContext {
    pub namespace: String,
    pub path: String,
    pub name: String,
    pub reply: PendingReply,
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
/// just the font registry, the glyph atlas, and the parked `load_font`
/// requests. The dispatcher holds this as the cap's state and routes
/// envelopes through the macro-emitted `Dispatch` impl; the addressing
/// identity is the distinct ZST [`super::TextCapability`]. Living in this
/// private module keeps it `pub`-enough to satisfy the
/// `NativeActor::State` interface without exposing it as crate-public API.
pub struct TextCapabilityState {
    /// Session-scoped font registry. Index is the `font_id` a
    /// `LoadFontResult::Ok` handed back and `DrawText.font_id` names.
    fonts: HashMap<u32, Arc<fontdue::Font>>,
    /// Reverse index from `(namespace, path)` to the `font_id` that
    /// file is resident under. Dedups the registry: a repeat load or
    /// a `font_metrics` grab of the same file reuses one resident
    /// font and a stable id rather than parsing a second copy.
    font_ids: HashMap<(String, String), u32>,
    /// Next `font_id` to assign — monotonic, session-scoped.
    next_font_id: u32,
    /// `load_font` requests awaiting their `aether.fs.read` reply,
    /// keyed by the echoed `(namespace, path)`. A `VecDeque` so
    /// concurrent loads of the same path correlate FIFO.
    pending_fonts: HashMap<(String, String), VecDeque<PendingFont>>,
    /// The shelf-packed glyph atlas (CPU-side source of truth).
    pub(super) atlas: Atlas,
    /// The render-cap `texture_id` backing [`Self::atlas`], once
    /// `create_texture` has replied. `None` until then.
    atlas_texture_id: Option<u32>,
    /// `true` between sending `create_texture` and its reply, so a
    /// burst of `draw`s sends exactly one creation request.
    atlas_create_inflight: bool,
}

impl TextCapabilityState {
    fn new() -> Self {
        Self {
            fonts: HashMap::new(),
            font_ids: HashMap::new(),
            next_font_id: 0,
            pending_fonts: HashMap::new(),
            atlas: Atlas::new(),
            atlas_texture_id: None,
            atlas_create_inflight: false,
        }
    }

    /// Pop the oldest `load_font` parked under `(namespace, path)`.
    fn take_pending(&mut self, namespace: &str, path: &str) -> Option<PendingFont> {
        let key = (namespace.to_owned(), path.to_owned());
        let queue = self.pending_fonts.get_mut(&key)?;
        let pending = queue.pop_front();
        if queue.is_empty() {
            self.pending_fonts.remove(&key);
        }
        pending
    }

    /// Register a parsed font under a session-scoped `font_id`,
    /// deduped by `(namespace, path)`: a path already resident
    /// returns its existing id (and drops the freshly-parsed `font`),
    /// so repeat loads and metric grabs of one file share a single
    /// resident font and a stable id.
    fn register_font(&mut self, namespace: &str, path: &str, font: Arc<fontdue::Font>) -> u32 {
        let key = (namespace.to_owned(), path.to_owned());
        if let Some(&existing) = self.font_ids.get(&key) {
            return existing;
        }
        let font_id = self.next_font_id;
        self.next_font_id = self.next_font_id.saturating_add(1);
        self.fonts.insert(font_id, font);
        self.font_ids.insert(key, font_id);
        font_id
    }

    /// Park a font request keyed `(namespace, path)` and forward its
    /// `aether.fs.read`. The `ReadResult` routes back to
    /// `on_read_result`, which parses the bytes and replies in the
    /// shape `reply` selects.
    fn forward_font_read(
        &mut self,
        ctx: &mut NativeCtx<'_, Manual>,
        namespace: String,
        path: String,
        reply: PendingReply,
    ) {
        let source = ctx.reply_target();
        self.pending_fonts
            .entry((namespace.clone(), path.clone()))
            .or_default()
            .push_back(PendingFont { source, reply });

        // Forward the read to the single fs resolver (ADR-0041); the
        // `ReadResult` routes back to `on_read_result`, which parses
        // it.
        let read = Read { namespace, path };
        ctx.actor::<FsCapability>().send(&read);
    }

    /// Send `create_texture` for the zeroed atlas, unless a creation is
    /// already in flight. The reply (`CreateTextureResult`) routes back
    /// to this cap's own mailbox, where `on_create_texture_result`
    /// stores the assigned id.
    fn ensure_atlas_texture(&mut self, ctx: &mut NativeCtx<'_>) {
        if self.atlas_texture_id.is_some() || self.atlas_create_inflight {
            return;
        }
        let create = CreateTexture {
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            pixels: self.atlas.pixels().to_vec(),
        };
        // Address the render cap through the lineage-correct resolver
        // (ADR-0099); `send` propagates this handler's chain by default
        // so the `CreateTextureResult` reply settles back into it.
        ctx.actor::<RenderCapability>().send(&create);
        self.atlas_create_inflight = true;
    }

    /// Send one `update_texture` for a newly-rasterized glyph's rect.
    fn upload_glyph(&self, ctx: &mut NativeCtx<'_>, texture_id: u32, entry: &AtlasEntry) {
        let update = UpdateTexture {
            texture_id,
            x: entry.x,
            y: entry.y,
            width: entry.width,
            height: entry.height,
            pixels: self.atlas.rect_rgba(entry),
        };
        ctx.actor::<RenderCapability>().send(&update);
    }

    /// Re-sync the GPU side after an atlas reset by uploading the full
    /// zeroed buffer. This ensures the render cap's staged pixels are a
    /// clean mirror of the reset CPU atlas before per-glyph uploads layer
    /// on top. Uses the same `update_texture` path as `upload_glyph`.
    fn resync_atlas(&self, ctx: &mut NativeCtx<'_>, texture_id: u32) {
        let update = UpdateTexture {
            texture_id,
            x: 0,
            y: 0,
            width: ATLAS_SIZE,
            height: ATLAS_SIZE,
            pixels: self.atlas.pixels().to_vec(),
        };
        ctx.actor::<RenderCapability>().send(&update);
    }
}

// The cap mail kinds (`LoadFont`, `DrawText`, …) plus the layout helpers the
// moved handler bodies name. The `#[runtime]` attribute emits the gated native
// runtime surface for the struct-hosted identity in the parent.
use self::layout::{
    build_font_metrics, emit_draw, font_name_from_path, glyph_dimensions, glyph_quad, quantize_size,
};
use super::TextCapability;
use super::kinds::{
    DrawText, FontMetricsRequest, FontMetricsResult, FontRef, LoadFont, LoadFontResult,
};
use aether_actor::runtime;

#[runtime]
impl NativeActor for TextCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// font registry, glyph atlas, and parked `load_font` requests.
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
    #[handler::manual]
    fn on_load_font(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: LoadFont) {
        state.forward_font_read(ctx, mail.namespace, mail.path, PendingReply::LoadFont);
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
    #[handler::manual]
    fn on_font_metrics(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        mail: FontMetricsRequest,
    ) {
        match mail.font {
            FontRef::Id(font_id) => {
                let reply = state.fonts.get(&font_id).map_or_else(
                    || FontMetricsResult::Err {
                        error: format!("unknown font_id {font_id}"),
                    },
                    |font| FontMetricsResult::Ok {
                        metrics: build_font_metrics(font),
                    },
                );
                ctx.reply(&reply);
            }
            FontRef::Path { namespace, path } => {
                if let Some(&font_id) = state.font_ids.get(&(namespace.clone(), path.clone())) {
                    // Already resident — measure from the cached font
                    // now, no fs round trip.
                    let metrics = build_font_metrics(&state.fonts[&font_id]);
                    ctx.reply(&FontMetricsResult::Ok { metrics });
                } else {
                    // Load on the miss; `on_font_parsed` replies once
                    // the font is parsed and registered.
                    state.forward_font_read(ctx, namespace, path, PendingReply::FontMetrics);
                }
            }
        }
    }

    /// Correlate a forwarded `aether.fs.read` reply. `Ok` dispatches the
    /// font parse off the hot path, pinning its deferred reply to the
    /// original `load_font` caller; `Err` relays the fs error to that
    /// caller as `LoadFontResult::Err`.
    #[handler::manual]
    fn on_read_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: ReadResult) {
        match mail {
            ReadResult::Ok {
                namespace,
                path,
                bytes,
            } => {
                let Some(pending) = state.take_pending(&namespace, &path) else {
                    // A stray / late reply with no parked request.
                    return;
                };
                let name = font_name_from_path(&path);
                let context = FontParseContext {
                    namespace,
                    path,
                    name,
                    reply: pending.reply,
                };
                let hold = ctx.acquire_settlement_hold();
                ctx.dispatch_blocking_resumed_with::<FontParseOutput, _, _>(
                    hold,
                    pending.source,
                    context,
                    move || match fontdue::Font::from_bytes(
                        bytes.as_slice(),
                        fontdue::FontSettings::default(),
                    ) {
                        Ok(font) => Ok(ParsedFont {
                            font: Arc::new(font),
                            resident_bytes: bytes.len() as u64,
                        }),
                        Err(e) => Err(format!("font parse failed: {e}")),
                    },
                );
            }
            ReadResult::Err {
                namespace,
                path,
                error,
            } => {
                if let Some(pending) = state.take_pending(&namespace, &path) {
                    let reason = format!("file read failed: {error:?}");
                    match pending.reply {
                        PendingReply::LoadFont => ctx.reply_to(
                            pending.source,
                            &LoadFontResult::Err {
                                namespace,
                                path,
                                error: reason,
                            },
                        ),
                        PendingReply::FontMetrics => {
                            ctx.reply_to(pending.source, &FontMetricsResult::Err { error: reason });
                        }
                    }
                }
            }
        }
    }

    /// Font-parse completion (ADR-0093 §3). On success register the
    /// parsed font (deduped by path) and reply in the shape the parked
    /// request is owed — `LoadFontResult::Ok` for a `load_font`,
    /// `FontMetricsResult::Ok` for a `font_metrics` grab; on a parse
    /// failure reply the matching `Err`. Either way `resolve_value`
    /// re-replies through the captured caller and drops the hold.
    #[handler(task)]
    fn on_font_parsed(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<FontParseOutput, FontParseContext>,
    ) {
        // Pull everything off `done` before consuming it: the context
        // (which reply shape, plus path for the dedup key) and the
        // parse outcome (the font + byte length, or the error text).
        let (namespace, path, name, reply) = {
            let cx = done.context();
            (
                cx.namespace.clone(),
                cx.path.clone(),
                cx.name.clone(),
                cx.reply,
            )
        };
        let parsed = match done.output() {
            Ok(parsed) => Ok((Arc::clone(&parsed.font), parsed.resident_bytes)),
            Err(error) => Err(error.clone()),
        };

        match parsed {
            Ok((font, resident_bytes)) => {
                let font_id = state.register_font(&namespace, &path, Arc::clone(&font));
                tracing::info!(
                    target: "aether_substrate::text",
                    font_id,
                    name = %name,
                    resident_bytes,
                    "font loaded",
                );
                match reply {
                    PendingReply::LoadFont => done.resolve_value(
                        ctx,
                        &LoadFontResult::Ok {
                            font_id,
                            name,
                            resident_bytes,
                        },
                    ),
                    PendingReply::FontMetrics => done.resolve_value(
                        ctx,
                        &FontMetricsResult::Ok {
                            metrics: build_font_metrics(&font),
                        },
                    ),
                }
            }
            Err(error) => match reply {
                PendingReply::LoadFont => done.resolve_value(
                    ctx,
                    &LoadFontResult::Err {
                        namespace,
                        path,
                        error,
                    },
                ),
                PendingReply::FontMetrics => {
                    done.resolve_value(ctx, &FontMetricsResult::Err { error });
                }
            },
        }
    }

    /// Store the atlas `texture_id` once `create_texture` replies. The
    /// cap creates exactly one texture, so the single reply is always
    /// its atlas — no correlation key needed.
    #[handler]
    fn on_create_texture_result(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: CreateTextureResult,
    ) {
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
    #[handler]
    fn on_draw_text(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: DrawText) {
        let Some(font) = state.fonts.get(&mail.font_id).cloned() else {
            tracing::warn!(
                target: "aether_substrate::text",
                font_id = mail.font_id,
                "draw for unknown font_id; dropping",
            );
            return;
        };
        if !(mail.size_pixels.is_finite() && mail.size_pixels > 0.0) {
            return;
        }
        let Some(texture_id) = state.atlas_texture_id else {
            // No atlas texture yet — kick off creation; immediate mode
            // resends this draw next frame once the id lands.
            state.ensure_atlas_texture(ctx);
            return;
        };

        // Reset the atlas when full so the frame's glyphs can re-pack
        // from a clean slate. The render cap's staged buffer is re-synced
        // with one full-rect upload; per-glyph uploads follow as cache
        // misses. This costs one frame of partial text (the overflow
        // glyphs missing on the saturating frame) and then fully recovers.
        if state.atlas.is_full() {
            tracing::info!(
                target: "aether_substrate::text",
                "glyph atlas full; resetting for next frame",
            );
            state.atlas.reset();
            state.resync_atlas(ctx, texture_id);
        }

        let size = mail.size_pixels;
        // Quantize the size for the glyph cache key — two draws at the
        // same nominal size share one raster.
        let size_key = quantize_size(size);
        let baseline = font
            .horizontal_line_metrics(size)
            .map_or(size, |line| line.ascent);

        let mut pen_x = 0.0f32;
        let mut quads: Vec<TexturedQuad> = Vec::new();
        let mut uploads: Vec<AtlasEntry> = Vec::new();

        for ch in mail.text.chars() {
            let glyph_index = font.lookup_glyph_index(ch);
            let metrics = font.metrics(ch, size);
            let key = GlyphKey {
                font_id: mail.font_id,
                glyph_index,
                size_pixels: size_key,
            };
            let (glyph_width, glyph_height) = glyph_dimensions(&metrics);

            // Rasterize only on a cache miss.
            let slot = if let Some(hit) = state.atlas.cached(&key) {
                hit
            } else {
                let (_m, coverage) = font.rasterize(ch, size);
                state
                    .atlas
                    .get_or_insert(key, glyph_width, glyph_height, &coverage)
            };

            match slot {
                GlyphSlot::Placed { entry, uploaded } => {
                    if uploaded {
                        uploads.push(entry);
                    }
                    quads.push(glyph_quad(&metrics, pen_x, baseline, &entry, mail.color));
                }
                // Empty: no pixels, just advance the pen.
                // Full: the atlas saturated during this frame's layout pass;
                // the reset fires at the top of the next draw so this
                // glyph will re-pack and render then.
                GlyphSlot::Empty | GlyphSlot::Full => {}
            }
            pen_x += metrics.advance_width;
        }

        for entry in uploads {
            state.upload_glyph(ctx, texture_id, &entry);
        }
        if !quads.is_empty() {
            if matches!(mail.space, QuadSpace::World { .. }) {
                // World quads carry pixel offsets relative to the
                // anchor, not absolute screen positions. Center the
                // string horizontally and shift so the baseline sits
                // at y=0 — the anchor is the baseline point, and text
                // appears above it (negative y in screen y-down
                // convention = above the anchor in world space).
                let half_width = pen_x / 2.0;
                for q in &mut quads {
                    q.x -= half_width;
                    q.y -= baseline;
                }
            } else {
                // Screen quads flow from the top-left of the window by
                // default (pen starts at 0,0). Apply the caller's origin
                // offset so a string can sit at an arbitrary screen pixel.
                let [ox, oy] = mail.origin;
                for q in &mut quads {
                    q.x += ox;
                    q.y += oy;
                }
            }
            emit_draw(ctx, texture_id, mail.space, quads);
        }
    }
}

#[cfg(all(test, feature = "text-native"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::*;
    use super::atlas::{ATLAS_SIZE, GlyphKey, GlyphSlot};
    use super::layout::build_font_metrics;
    use super::{
        Arc, CreateTexture, NativeCtx, QuadSpace, Read, Source, TextCapabilityState, UpdateTexture,
    };
    use crate::fs::FsError;
    use crate::render::DrawTexturedQuads;
    use crate::test_chassis::{decode_session_reply, drive_task_completion, test_mailer_and_rx};
    use aether_data::{Kind, MailId, SessionToken, SourceAddr, Uuid};
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::outbound::EgressEvent;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    fn session_sender() -> Source {
        Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
    }

    /// Flush the cap's buffered sends, then drain egress asserting the
    /// next `UnresolvedMail` carries kind `K`. The bare registry has no
    /// `aether.render` / `aether.fs`, so a forwarded send bubbles to the
    /// loopback outbound; `flush_outbound` is what `NativeCtx::Drop`
    /// would otherwise do at the end of a real dispatch turn.
    fn assert_next_send_kind<K: Kind>(binding: &NativeBinding, rx: &Receiver<EgressEvent>) {
        binding.flush_outbound();
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("test: egress event arrives within deadline");
            if let EgressEvent::UnresolvedMail { kind_id, .. } = event {
                assert_eq!(kind_id, K::ID, "unexpected bubbled kind");
                return;
            }
        }
    }

    fn ctx_binding() -> (Arc<NativeBinding>, Receiver<EgressEvent>) {
        let (mailer, rx) = test_mailer_and_rx();
        let binding = Arc::new(NativeBinding::new_for_test(
            mailer,
            aether_data::MailboxId(0),
        ));
        (binding, rx)
    }

    /// Run `on_draw_text` for a `Screen`-space white string over a fresh
    /// `NativeCtx` on `binding` — the shape the draw tests repeat. Varies
    /// only `font_id`, `text`, `size_pixels`, and `origin`; color is
    /// always opaque white and the space is always `Screen`.
    fn draw_screen(
        state: &mut TextCapabilityState,
        binding: &Arc<NativeBinding>,
        font_id: u32,
        text: &str,
        size_pixels: f32,
        origin: [f32; 2],
    ) {
        let mut ctx = NativeCtx::new(binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_draw_text(
            state,
            &mut ctx,
            DrawText {
                font_id,
                text: text.to_owned(),
                size_pixels,
                color: [1.0, 1.0, 1.0, 1.0],
                origin,
                space: QuadSpace::Screen,
            },
        );
    }

    #[test]
    fn load_font_parks_request_and_forwards_read() {
        let mut state = TextCapabilityState::new();
        let (binding, rx) = ctx_binding();
        let mut ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_load_font(
            &mut state,
            &mut ctx,
            LoadFont {
                namespace: "assets".to_owned(),
                path: "fonts/RobotoMono.ttf".to_owned(),
            },
        );
        assert_eq!(state.pending_fonts.len(), 1, "request not parked");
        assert_next_send_kind::<Read>(&binding, &rx);
    }

    #[test]
    fn read_err_replies_load_font_err_and_clears_pending() {
        let mut state = TextCapabilityState::new();
        let (binding, rx) = ctx_binding();
        let mut ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_load_font(
            &mut state,
            &mut ctx,
            LoadFont {
                namespace: "assets".to_owned(),
                path: "missing.ttf".to_owned(),
            },
        );
        // Skip the forwarded read.
        assert_next_send_kind::<Read>(&binding, &rx);

        let mut read_ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_read_result(
            &mut state,
            &mut read_ctx,
            ReadResult::Err {
                namespace: "assets".to_owned(),
                path: "missing.ttf".to_owned(),
                error: FsError::NotFound,
            },
        );
        match decode_session_reply::<LoadFontResult>(&rx) {
            LoadFontResult::Err { path, .. } => assert_eq!(path, "missing.ttf"),
            LoadFontResult::Ok { .. } => panic!("expected Err for a missing file"),
        }
        assert!(state.pending_fonts.is_empty(), "pending never cleared");
    }

    #[test]
    fn malformed_font_bytes_reply_err() {
        let mut state = TextCapabilityState::new();
        let (binding, rx) = ctx_binding();
        let mut ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_load_font(
            &mut state,
            &mut ctx,
            LoadFont {
                namespace: "assets".to_owned(),
                path: "junk.ttf".to_owned(),
            },
        );
        assert_next_send_kind::<Read>(&binding, &rx);

        let mut read_ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_read_result(
            &mut state,
            &mut read_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "junk.ttf".to_owned(),
                bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
            },
        );
        drive_task_completion::<TextCapability>(&mut state, &binding, &rx);
        match decode_session_reply::<LoadFontResult>(&rx) {
            LoadFontResult::Err { error, .. } => {
                assert!(error.contains("parse"), "unexpected error: {error}");
            }
            LoadFontResult::Ok { .. } => panic!("expected Err for malformed font bytes"),
        }
        assert!(
            state.fonts.is_empty(),
            "no font should register on a parse failure"
        );
    }

    #[test]
    fn draw_with_unknown_font_emits_nothing() {
        let mut state = TextCapabilityState::new();
        let (binding, rx) = ctx_binding();
        draw_screen(&mut state, &binding, 99, "hi", 32.0, [0.0, 0.0]);
        assert!(
            rx.try_recv().is_err(),
            "an unknown font_id must not emit any render mail",
        );
    }

    #[test]
    fn first_draw_with_known_font_creates_the_atlas_texture() {
        let mut state = TextCapabilityState::new();
        // Register a font directly — the parse path is covered above;
        // here we exercise the lazy-create branch of `draw`.
        let font = test_font();
        state.fonts.insert(0, Arc::new(font));
        let (binding, rx) = ctx_binding();
        draw_screen(&mut state, &binding, 0, "hi", 32.0, [0.0, 0.0]);
        assert!(
            state.atlas_create_inflight,
            "first draw should kick off atlas creation",
        );
        assert!(
            state.atlas_texture_id.is_none(),
            "no texture id until create_texture replies",
        );
        assert_next_send_kind::<CreateTexture>(&binding, &rx);
    }

    #[test]
    fn draw_after_texture_ready_emits_update_and_quads() {
        let mut state = TextCapabilityState::new();
        state.fonts.insert(0, Arc::new(test_font()));
        // Simulate the create_texture reply landing.
        state.atlas_create_inflight = true;
        let (binding, rx) = ctx_binding();
        {
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            TextCapability::on_create_texture_result(
                &mut state,
                &mut ctx,
                CreateTextureResult::Ok { texture_id: 7 },
            );
        }
        assert_eq!(state.atlas_texture_id, Some(7));

        draw_screen(&mut state, &binding, 0, "A", 48.0, [0.0, 0.0]);
        // A printable glyph rasterizes once: first an update_texture for
        // the new glyph, then the draw_textured_quads batch.
        assert_next_send_kind::<UpdateTexture>(&binding, &rx);
        assert_next_send_kind::<DrawTexturedQuads>(&binding, &rx);
    }

    #[test]
    fn draw_after_atlas_full_resets_and_renders_glyph() {
        let mut state = TextCapabilityState::new();
        state.fonts.insert(0, Arc::new(test_font()));
        state.atlas_create_inflight = true;
        let (binding, rx) = ctx_binding();
        {
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            TextCapability::on_create_texture_result(
                &mut state,
                &mut ctx,
                CreateTextureResult::Ok { texture_id: 3 },
            );
        }
        assert_eq!(state.atlas_texture_id, Some(3));

        // Fill the atlas by directly calling get_or_insert with wide bands
        // until the atlas reports full. `ATLAS_SIZE`, `GlyphKey`, and
        // `GlyphSlot` are in scope via the `use super::{…}` import
        // (the runtime half re-exports the atlas types).
        {
            let band_height = 64u32;
            let coverage = vec![255u8; (ATLAS_SIZE * band_height) as usize];
            for glyph_index in 0..32u16 {
                let key = GlyphKey {
                    font_id: 99,
                    glyph_index,
                    size_pixels: 64,
                };
                match state
                    .atlas
                    .get_or_insert(key, ATLAS_SIZE, band_height, &coverage)
                {
                    GlyphSlot::Placed { .. } => {}
                    GlyphSlot::Full => break,
                    GlyphSlot::Empty => panic!("band coverage is not empty"),
                }
            }
        }
        assert!(state.atlas.is_full(), "atlas must be full before draw");

        // A draw now: the cap should reset the atlas (emitting a full-rect
        // update_texture for the resync), rasterize the glyph (another
        // update_texture), then send draw_textured_quads. The glyph renders
        // rather than drops — proving the reset freed space.
        draw_screen(&mut state, &binding, 0, "A", 48.0, [0.0, 0.0]);

        assert!(
            !state.atlas.is_full(),
            "atlas must be clear after reset-triggered draw"
        );

        // The full-rect resync and the per-glyph upload both arrive as
        // UpdateTexture; the quad batch follows as DrawTexturedQuads.
        assert_next_send_kind::<UpdateTexture>(&binding, &rx);
        assert_next_send_kind::<UpdateTexture>(&binding, &rx);
        assert_next_send_kind::<DrawTexturedQuads>(&binding, &rx);
    }

    /// `Screen` draws at a non-zero `origin` shift every glyph quad by
    /// that offset. Draw the same string twice — once at `[0,0]` and once
    /// at `[ox, oy]` — and assert each quad in the offset batch sits
    /// exactly `(ox, oy)` further right/down than its zero-origin peer.
    #[test]
    fn screen_origin_shifts_quad_positions() {
        let mut state = TextCapabilityState::new();
        state.fonts.insert(0, Arc::new(test_font()));
        state.atlas_create_inflight = true;
        let (binding, rx) = ctx_binding();
        {
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            TextCapability::on_create_texture_result(
                &mut state,
                &mut ctx,
                CreateTextureResult::Ok { texture_id: 1 },
            );
        }
        assert_eq!(state.atlas_texture_id, Some(1));

        // Draw at origin [0, 0] — the glyph rasterizes on the first draw
        // (cache miss), so drain UpdateTexture before collecting quads.
        draw_screen(&mut state, &binding, 0, "A", 24.0, [0.0, 0.0]);
        let quads_zero = collect_draw_textured_quads(&binding, &rx).quads;

        // Second draw at a non-zero origin — glyph is cached, so only
        // DrawTexturedQuads is emitted (no UpdateTexture).
        let ox = 30.0f32;
        let oy = 50.0f32;
        draw_screen(&mut state, &binding, 0, "A", 24.0, [ox, oy]);
        let quads_offset = collect_draw_textured_quads(&binding, &rx).quads;

        assert_eq!(
            quads_zero.len(),
            quads_offset.len(),
            "same text must produce the same number of quads",
        );
        for (z, o) in quads_zero.iter().zip(quads_offset.iter()) {
            assert!(
                (o.x - z.x - ox).abs() < 0.01,
                "quad x should shift by {ox}: zero={}, offset={}",
                z.x,
                o.x,
            );
            assert!(
                (o.y - z.y - oy).abs() < 0.01,
                "quad y should shift by {oy}: zero={}, offset={}",
                z.y,
                o.y,
            );
        }
    }

    /// Drain egress until the next `DrawTexturedQuads` `UnresolvedMail`
    /// arrives, skipping any prior `UpdateTexture` or other sends.
    fn collect_draw_textured_quads(
        binding: &NativeBinding,
        rx: &Receiver<EgressEvent>,
    ) -> DrawTexturedQuads {
        binding.flush_outbound();
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("test: egress event arrives within deadline");
            if let EgressEvent::UnresolvedMail {
                kind_id, payload, ..
            } = event
                && kind_id == DrawTexturedQuads::ID
            {
                return DrawTexturedQuads::decode_from_bytes(&payload)
                    .expect("test: DrawTexturedQuads payload decodes");
            }
        }
    }

    /// A tiny real font for the draw-path tests — the workspace's
    /// vendored OFL Roboto Mono, the same asset the e2e scenario uses.
    fn test_font() -> fontdue::Font {
        fontdue::Font::from_bytes(test_font_bytes(), fontdue::FontSettings::default())
            .expect("test setup: vendored Roboto Mono parses")
    }

    /// The raw bytes of [`test_font`], for the read-result tests that
    /// feed the parse path a real TTF.
    fn test_font_bytes() -> &'static [u8] {
        include_bytes!("../../../../aether-substrate-bundle/assets/fonts/RobotoMono.ttf")
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
        let by_codepoint: HashMap<u32, f32> = metrics
            .advances
            .iter()
            .map(|glyph| (glyph.codepoint, glyph.advance_units))
            .collect();
        let advance_units = |ch: char| {
            by_codepoint
                .get(&u32::from(ch))
                .copied()
                .unwrap_or(metrics.default_advance)
        };

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
        assert_eq!(state.fonts.len(), 1, "only one resident font for the path");

        let other = state.register_font("assets", "other.ttf", Arc::new(test_font()));
        assert_ne!(other, first, "a different path gets a fresh id");
        assert_eq!(state.fonts.len(), 2);
    }

    /// A `font_metrics` grab by a resident `font_id` replies `Ok`
    /// synchronously; an unknown id replies `Err`.
    #[test]
    fn font_metrics_by_id_replies_ok_or_err() {
        let mut state = TextCapabilityState::new();
        state.fonts.insert(0, Arc::new(test_font()));
        let (binding, rx) = ctx_binding();

        let mut ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_font_metrics(
            &mut state,
            &mut ctx,
            FontMetricsRequest {
                font: FontRef::Id(0),
            },
        );
        match decode_session_reply::<FontMetricsResult>(&rx) {
            FontMetricsResult::Ok { metrics } => {
                assert!(metrics.units_per_em > 0.0);
                assert!(!metrics.advances.is_empty(), "a real font has glyphs");
            }
            FontMetricsResult::Err { error } => panic!("expected Ok: {error}"),
        }

        let mut ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_font_metrics(
            &mut state,
            &mut ctx,
            FontMetricsRequest {
                font: FontRef::Id(99),
            },
        );
        match decode_session_reply::<FontMetricsResult>(&rx) {
            FontMetricsResult::Err { error } => assert!(error.contains("99")),
            FontMetricsResult::Ok { .. } => panic!("expected Err for an unknown font_id"),
        }
    }

    /// A `font_metrics` grab by a path with no resident font loads on
    /// the miss: it parks the request, forwards an `aether.fs.read`,
    /// and — once the bytes come back and parse — registers the font
    /// (indexed by path) and replies `FontMetricsResult::Ok`.
    #[test]
    fn font_metrics_by_path_loads_on_miss() {
        let mut state = TextCapabilityState::new();
        let (binding, rx) = ctx_binding();
        let mut ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_font_metrics(
            &mut state,
            &mut ctx,
            FontMetricsRequest {
                font: FontRef::Path {
                    namespace: "assets".to_owned(),
                    path: "font.ttf".to_owned(),
                },
            },
        );
        assert_eq!(state.pending_fonts.len(), 1, "a miss must park the request");
        assert_next_send_kind::<Read>(&binding, &rx);

        let mut read_ctx =
            NativeCtx::new_dispatching(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_read_result(
            &mut state,
            &mut read_ctx,
            ReadResult::Ok {
                namespace: "assets".to_owned(),
                path: "font.ttf".to_owned(),
                bytes: test_font_bytes().to_vec(),
            },
        );
        drive_task_completion::<TextCapability>(&mut state, &binding, &rx);
        match decode_session_reply::<FontMetricsResult>(&rx) {
            FontMetricsResult::Ok { metrics } => {
                assert!(!metrics.advances.is_empty());
            }
            FontMetricsResult::Err { error } => panic!("expected Ok: {error}"),
        }
        assert_eq!(state.fonts.len(), 1, "load-on-miss registers the font");
        assert_eq!(state.font_ids.len(), 1, "and indexes it by path");
    }
}
