//! `aether.text` cap (ADR-0105). Turns a TTF plus a string into the
//! textured quads the render surface draws — a CPU-only actor with no
//! GPU access that composes `aether.render`'s texture surface entirely by
//! mail.
//!
//! Two flows:
//!
//! - **`load_font`** mirrors `aether.audio.load_instrument` (ADR-0103):
//!   park the request keyed `(namespace, path)`, forward `aether.fs.read`,
//!   correlate on `aether.fs.read_result`, parse the font off the hot path
//!   in a `#[handler(task)]` arm, and register it under a session-scoped
//!   `font_id`. The reply is `load_font_result`.
//! - **`draw`** is fire-and-forget immediate mode: lay the string out with
//!   fontdue's horizontal metrics, rasterize any unseen glyph into the
//!   shelf-packed atlas, emit one `update_texture` per new glyph plus the
//!   `draw_textured_quads` batch — all to `aether.render` the same tick.
//!
//! The atlas texture is created lazily: the first `draw` sends
//! `create_texture` (the zeroed atlas) and correlates the
//! `create_texture_result` reply onto the cap's own mailbox. Until the id
//! lands the cap draws nothing; immediate mode resends every frame, so the
//! string appears the moment the texture exists.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings of
// the mod (always-on, outside the cfg gate).
use aether_kinds::{CreateTextureResult, DrawText, LoadFont, ReadResult};

// ADR-0105 shelf-packed RGBA8 glyph atlas (`text/atlas.rs`). Native-only —
// it is pure CPU but only the native cap consumes it, so it rides the same
// `text-native` gate as `fontdue`.
#[cfg(all(not(target_arch = "wasm32"), feature = "text-native"))]
mod atlas;

#[aether_actor::bridge(singleton, feature = "text-native")]
mod native {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Arc;

    use aether_actor::{OutboundReply, actor};
    use aether_data::Source;
    use aether_kinds::{
        CreateTexture, DrawTexturedQuads, LoadFontResult, QuadSpace, Read, TexturedQuad,
        UpdateTexture,
    };
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
    use aether_substrate::chassis::error::BootError;

    use crate::fs::FsCapability;
    use crate::render::RenderCapability;

    use super::atlas::{ATLAS_SIZE, Atlas, AtlasEntry, GlyphKey, GlyphSlot};
    use super::{CreateTextureResult, DrawText, LoadFont, ReadResult};

    /// A `load_font` request parked while its `aether.fs.read` is in
    /// flight, keyed in [`TextCapability::pending_fonts`] by the echoed
    /// `(namespace, path)`. Carries the original requester so the deferred
    /// `LoadFontResult` lands on the `load_font` caller.
    struct PendingFont {
        source: Source,
    }

    /// Context carried through the font-parse task so the completion arm
    /// can shape the `LoadFontResult` reply.
    struct FontParseContext {
        namespace: String,
        path: String,
        name: String,
    }

    /// A successfully parsed font plus the byte length the reply reports as
    /// `resident_bytes`.
    struct ParsedFont {
        font: Arc<fontdue::Font>,
        resident_bytes: u64,
    }

    /// Off-hot-path parse outcome — `Err` carries the reason the cap relays
    /// as `LoadFontResult::Err`.
    type FontParseOutput = Result<ParsedFont, String>;

    /// `aether.text` mailbox cap. CPU-only — no GPU handles, just the font
    /// registry, the glyph atlas, and the parked `load_font` requests.
    pub struct TextCapability {
        /// Session-scoped font registry. Index is the `font_id` a
        /// `LoadFontResult::Ok` handed back and `DrawText.font_id` names.
        fonts: HashMap<u32, Arc<fontdue::Font>>,
        /// Next `font_id` to assign — monotonic, session-scoped.
        next_font_id: u32,
        /// `load_font` requests awaiting their `aether.fs.read` reply,
        /// keyed by the echoed `(namespace, path)`. A `VecDeque` so
        /// concurrent loads of the same path correlate FIFO.
        pending_fonts: HashMap<(String, String), VecDeque<PendingFont>>,
        /// The shelf-packed glyph atlas (CPU-side source of truth).
        atlas: Atlas,
        /// The render-cap `texture_id` backing [`Self::atlas`], once
        /// `create_texture` has replied. `None` until then.
        atlas_texture_id: Option<u32>,
        /// `true` between sending `create_texture` and its reply, so a
        /// burst of `draw`s sends exactly one creation request.
        atlas_create_inflight: bool,
    }

    impl TextCapability {
        fn new() -> Self {
            Self {
                fonts: HashMap::new(),
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
            // (ADR-0099); `send_traced` propagates this handler's chain so the
            // `CreateTextureResult` reply settles back into it.
            let _ = ctx.actor::<RenderCapability>().send_traced(ctx, &create);
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
            let _ = ctx.actor::<RenderCapability>().send_traced(ctx, &update);
        }
    }

    #[actor]
    impl NativeActor for TextCapability {
        type Config = ();

        /// ADR-0105 chassis-owned mailbox.
        const NAMESPACE: &'static str = "aether.text";

        /// No substrate resources to claim — the cap holds only CPU state.
        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self::new())
        }

        /// Load a font from a TTF file.
        ///
        /// # Agent
        /// Reply: `LoadFontResult`. The cap forwards an `aether.fs.read`
        /// for `namespace://path`, parses the TTF off the hot path, and
        /// replies `Ok { font_id, name, resident_bytes }` once registered
        /// or `Err` with the failure reason (bad path, or an unparseable
        /// file). The `font_id` is session-scoped — thread it into `draw`.
        #[handler]
        fn on_load_font(&mut self, ctx: &mut NativeCtx<'_>, mail: LoadFont) {
            let source = ctx.reply_target();
            let key = (mail.namespace.clone(), mail.path.clone());
            self.pending_fonts
                .entry(key)
                .or_default()
                .push_back(PendingFont { source });

            // Forward the read to the single fs resolver (ADR-0041); the
            // `ReadResult` routes back to `on_read_result`, which parses it.
            let read = Read {
                namespace: mail.namespace,
                path: mail.path,
            };
            let _ = ctx.actor::<FsCapability>().send_traced(ctx, &read);
        }

        /// Correlate a forwarded `aether.fs.read` reply. `Ok` dispatches the
        /// font parse off the hot path, pinning its deferred reply to the
        /// original `load_font` caller; `Err` relays the fs error to that
        /// caller as `LoadFontResult::Err`.
        #[handler]
        fn on_read_result(&mut self, ctx: &mut NativeCtx<'_>, mail: ReadResult) {
            match mail {
                ReadResult::Ok {
                    namespace,
                    path,
                    bytes,
                } => {
                    let Some(pending) = self.take_pending(&namespace, &path) else {
                        // A stray / late reply with no parked request.
                        return;
                    };
                    let name = font_name_from_path(&path);
                    let context = FontParseContext {
                        namespace,
                        path,
                        name,
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
                    if let Some(pending) = self.take_pending(&namespace, &path) {
                        ctx.reply_to(
                            pending.source,
                            &LoadFontResult::Err {
                                namespace,
                                path,
                                error: format!("file read failed: {error:?}"),
                            },
                        );
                    }
                }
            }
        }

        /// Font-parse completion (ADR-0093 §3). On success assign the next
        /// `font_id`, register the parsed font, and reply `Ok`; on a parse
        /// failure reply `Err`. Either way `resolve_with` re-replies through
        /// the captured `load_font` caller and drops the hold.
        #[handler(task)]
        fn on_font_parsed(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            done: TaskDone<FontParseOutput, FontParseContext>,
        ) {
            let outcome: LoadFontResult = match done.output() {
                Ok(parsed) => {
                    let font_id = self.next_font_id;
                    self.next_font_id = self.next_font_id.saturating_add(1);
                    self.fonts.insert(font_id, Arc::clone(&parsed.font));
                    let cx = done.context();
                    tracing::info!(
                        target: "aether_substrate::text",
                        font_id,
                        name = %cx.name,
                        resident_bytes = parsed.resident_bytes,
                        "font loaded",
                    );
                    LoadFontResult::Ok {
                        font_id,
                        name: cx.name.clone(),
                        resident_bytes: parsed.resident_bytes,
                    }
                }
                Err(error) => {
                    let cx = done.context();
                    LoadFontResult::Err {
                        namespace: cx.namespace.clone(),
                        path: cx.path.clone(),
                        error: error.clone(),
                    }
                }
            };
            done.resolve_with(ctx, move |_out, _cx| outcome);
        }

        /// Store the atlas `texture_id` once `create_texture` replies. The
        /// cap creates exactly one texture, so the single reply is always
        /// its atlas — no correlation key needed.
        #[handler]
        fn on_create_texture_result(
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            mail: CreateTextureResult,
        ) {
            self.atlas_create_inflight = false;
            match mail {
                CreateTextureResult::Ok { texture_id } => {
                    self.atlas_texture_id = Some(texture_id);
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
        /// warn-drops; a full atlas drops the overflow glyphs. The first
        /// `draw` lazily creates the atlas texture and draws nothing until
        /// the reply lands — resend every frame (immediate-mode contract).
        #[handler]
        fn on_draw_text(&mut self, ctx: &mut NativeCtx<'_>, mail: DrawText) {
            let Some(font) = self.fonts.get(&mail.font_id).cloned() else {
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
            let Some(texture_id) = self.atlas_texture_id else {
                // No atlas texture yet — kick off creation; immediate mode
                // resends this draw next frame once the id lands.
                self.ensure_atlas_texture(ctx);
                return;
            };

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
                let slot = if let Some(hit) = self.atlas.cached(&key) {
                    hit
                } else {
                    let (_m, coverage) = font.rasterize(ch, size);
                    self.atlas
                        .get_or_insert(key, glyph_width, glyph_height, &coverage)
                };

                match slot {
                    GlyphSlot::Placed { entry, uploaded } => {
                        if uploaded {
                            uploads.push(entry);
                        }
                        quads.push(glyph_quad(&metrics, pen_x, baseline, &entry, mail.color));
                    }
                    GlyphSlot::Empty => {}
                    GlyphSlot::Full => {
                        tracing::warn!(
                            target: "aether_substrate::text",
                            "glyph atlas full; dropping glyph for the session",
                        );
                    }
                }
                pen_x += metrics.advance_width;
            }

            for entry in uploads {
                self.upload_glyph(ctx, texture_id, &entry);
            }
            if !quads.is_empty() {
                // World quads carry pixel offsets relative to the
                // anchor, not absolute screen positions. Center the
                // string horizontally and shift so the baseline sits
                // at y=0 — the anchor is the baseline point, and text
                // appears above it (negative y in screen y-down
                // convention = above the anchor in world space).
                if matches!(mail.space, QuadSpace::World { .. }) {
                    let half_width = pen_x / 2.0;
                    for q in &mut quads {
                        q.x -= half_width;
                        q.y -= baseline;
                    }
                }
                emit_draw(ctx, texture_id, mail.space, quads);
            }
        }
    }

    /// Emit the accumulated quad batch to `aether.render`.
    fn emit_draw(
        ctx: &mut NativeCtx<'_>,
        texture_id: u32,
        space: QuadSpace,
        quads: Vec<TexturedQuad>,
    ) {
        let draw = DrawTexturedQuads {
            texture_id,
            space,
            quads,
        };
        let _ = ctx.actor::<RenderCapability>().send_traced(ctx, &draw);
    }

    /// A glyph bitmap's pixel dimensions. fontdue bounds these well below
    /// `u32::MAX`, so the `usize → u32` narrowing is exact.
    #[allow(clippy::cast_possible_truncation)]
    fn glyph_dimensions(metrics: &fontdue::Metrics) -> (u32, u32) {
        (metrics.width as u32, metrics.height as u32)
    }

    /// Place a glyph's quad in screen pixels. fontdue uses +y up with
    /// `ymin` the glyph's bottom above the baseline; screen space is y-down
    /// with the baseline at `baseline`, so the top row sits at
    /// `baseline - (ymin + height)` and the left edge at `pen_x + xmin`.
    /// Glyph extents are small integers, exact in `f32`.
    #[allow(clippy::cast_precision_loss)]
    fn glyph_quad(
        metrics: &fontdue::Metrics,
        pen_x: f32,
        baseline: f32,
        entry: &AtlasEntry,
        tint: [f32; 4],
    ) -> TexturedQuad {
        let top = baseline - (metrics.ymin as f32 + metrics.height as f32);
        let left = pen_x + metrics.xmin as f32;
        TexturedQuad {
            x: left,
            y: top,
            width: metrics.width as f32,
            height: metrics.height as f32,
            u0: entry.u0,
            v0: entry.v0,
            u1: entry.u1,
            v1: entry.v1,
            tint,
        }
    }

    /// Round a pixel size to its nearest integer for the glyph cache key,
    /// clamped to at least 1.
    fn quantize_size(size_pixels: f32) -> u32 {
        // Caller already checked `size_pixels` is finite and positive.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rounded = size_pixels.round().max(1.0) as u32;
        rounded
    }

    /// The font's display name — the file stem of its path (e.g.
    /// `fonts/RobotoMono.ttf` → `RobotoMono`), or the whole path when it
    /// has no stem.
    fn font_name_from_path(path: &str) -> String {
        Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .map_or_else(|| path.to_owned(), ToOwned::to_owned)
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]

        use super::*;
        use crate::test_chassis::{
            TestChassis, decode_session_reply, drive_task_completion, fresh_substrate,
            test_mailer_and_rx,
        };
        use aether_actor::Actor;
        use aether_data::{Kind, MailId, SessionToken, SourceAddr, Uuid};
        use aether_kinds::FsError;
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::mail::outbound::EgressEvent;
        use aether_substrate::mail::registry;
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

        #[test]
        fn load_font_parks_request_and_forwards_read() {
            let mut cap = TextCapability::new();
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_load_font(
                &mut ctx,
                LoadFont {
                    namespace: "assets".to_owned(),
                    path: "fonts/RobotoMono.ttf".to_owned(),
                },
            );
            assert_eq!(cap.pending_fonts.len(), 1, "request not parked");
            assert_next_send_kind::<Read>(&binding, &rx);
        }

        #[test]
        fn read_err_replies_load_font_err_and_clears_pending() {
            let mut cap = TextCapability::new();
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_load_font(
                &mut ctx,
                LoadFont {
                    namespace: "assets".to_owned(),
                    path: "missing.ttf".to_owned(),
                },
            );
            // Skip the forwarded read.
            assert_next_send_kind::<Read>(&binding, &rx);

            let mut read_ctx =
                NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_read_result(
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
            assert!(cap.pending_fonts.is_empty(), "pending never cleared");
        }

        #[test]
        fn malformed_font_bytes_reply_err() {
            let mut cap = TextCapability::new();
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_load_font(
                &mut ctx,
                LoadFont {
                    namespace: "assets".to_owned(),
                    path: "junk.ttf".to_owned(),
                },
            );
            assert_next_send_kind::<Read>(&binding, &rx);

            let mut read_ctx =
                NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_read_result(
                &mut read_ctx,
                ReadResult::Ok {
                    namespace: "assets".to_owned(),
                    path: "junk.ttf".to_owned(),
                    bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
                },
            );
            drive_task_completion(&mut cap, &binding, &rx);
            match decode_session_reply::<LoadFontResult>(&rx) {
                LoadFontResult::Err { error, .. } => {
                    assert!(error.contains("parse"), "unexpected error: {error}");
                }
                LoadFontResult::Ok { .. } => panic!("expected Err for malformed font bytes"),
            }
            assert!(
                cap.fonts.is_empty(),
                "no font should register on a parse failure"
            );
        }

        #[test]
        fn draw_with_unknown_font_emits_nothing() {
            let mut cap = TextCapability::new();
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_draw_text(
                &mut ctx,
                DrawText {
                    font_id: 99,
                    text: "hi".to_owned(),
                    size_pixels: 32.0,
                    color: [1.0; 4],
                    space: QuadSpace::Screen,
                },
            );
            assert!(
                rx.try_recv().is_err(),
                "an unknown font_id must not emit any render mail",
            );
        }

        #[test]
        fn first_draw_with_known_font_creates_the_atlas_texture() {
            let mut cap = TextCapability::new();
            // Register a font directly — the parse path is covered above;
            // here we exercise the lazy-create branch of `draw`.
            let font = test_font();
            cap.fonts.insert(0, Arc::new(font));
            let (binding, rx) = ctx_binding();
            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_draw_text(
                &mut ctx,
                DrawText {
                    font_id: 0,
                    text: "hi".to_owned(),
                    size_pixels: 32.0,
                    color: [1.0; 4],
                    space: QuadSpace::Screen,
                },
            );
            assert!(
                cap.atlas_create_inflight,
                "first draw should kick off atlas creation",
            );
            assert!(
                cap.atlas_texture_id.is_none(),
                "no texture id until create_texture replies",
            );
            assert_next_send_kind::<CreateTexture>(&binding, &rx);
        }

        #[test]
        fn draw_after_texture_ready_emits_update_and_quads() {
            let mut cap = TextCapability::new();
            cap.fonts.insert(0, Arc::new(test_font()));
            // Simulate the create_texture reply landing.
            cap.atlas_create_inflight = true;
            let (binding, rx) = ctx_binding();
            {
                let mut ctx =
                    NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
                cap.on_create_texture_result(&mut ctx, CreateTextureResult::Ok { texture_id: 7 });
            }
            assert_eq!(cap.atlas_texture_id, Some(7));

            let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
            cap.on_draw_text(
                &mut ctx,
                DrawText {
                    font_id: 0,
                    text: "A".to_owned(),
                    size_pixels: 48.0,
                    color: [1.0, 1.0, 1.0, 1.0],
                    space: QuadSpace::Screen,
                },
            );
            // A printable glyph rasterizes once: first an update_texture for
            // the new glyph, then the draw_textured_quads batch.
            assert_next_send_kind::<UpdateTexture>(&binding, &rx);
            assert_next_send_kind::<DrawTexturedQuads>(&binding, &rx);
        }

        /// Builder rejects a duplicate claim of the well-known mailbox.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let (registry, mailer) = fresh_substrate();
            registry.register_inbox(TextCapability::NAMESPACE, registry::noop_handler());
            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<TextCapability>(())
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == TextCapability::NAMESPACE
            ));
        }

        /// A tiny real font for the draw-path tests — the workspace's
        /// vendored OFL Roboto Mono, the same asset the e2e scenario uses.
        fn test_font() -> fontdue::Font {
            const TTF: &[u8] =
                include_bytes!("../../aether-substrate-bundle/assets/fonts/RobotoMono.ttf");
            fontdue::Font::from_bytes(TTF, fontdue::FontSettings::default())
                .expect("test setup: vendored Roboto Mono parses")
        }
    }
}
