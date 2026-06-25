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
//!
//! When the atlas fills, the cap resets it at the top of the next `draw`:
//! zeros the pixel buffer, clears the glyph cache, resets the shelf
//! cursor, and emits one full-rect `update_texture` to re-sync the GPU
//! side. Every glyph for that frame is then a cache miss and re-uploads
//! normally. The cost is at most one frame of missing overflow glyphs on
//! the saturating frame; the next frame fully recovers.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the decoded
// bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers against the
// identity (always-on, outside the runtime gate). The `aether.text` mail
// kinds (ADR-0121) live in `kinds` and re-export here; `ReadResult` comes
// from the `aether.fs` cap, and `CreateTextureResult` from the render cap —
// both are replies the text cap receives.
use crate::fs::ReadResult;
use crate::render::CreateTextureResult;

// ADR-0121: the cap owns its mail kinds. Always-on + wasm-safe (only
// `aether-data` + `serde`), re-exported so callers address them as
// `aether_capabilities::text::DrawText`.
mod kinds;
pub use kinds::*;

// ADR-0105 shelf-packed RGBA8 glyph atlas (`text/atlas.rs`). Native-only —
// it is pure CPU but only the native cap consumes it, so it rides the same
// `text-native` gate as `fontdue`.
#[cfg(all(not(target_arch = "wasm32"), feature = "text-native"))]
mod atlas;

// Pure layout / rasterization helpers (ADR-0121). Same `text-native` gate —
// they run fontdue off the hot path.
#[cfg(all(not(target_arch = "wasm32"), feature = "text-native"))]
mod layout;

/// `aether.text` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry, all
/// emitted always-on by `#[actor]`. The state-bearing runtime
/// (`TextCapabilityState`, which holds the `fontdue` font registry, the
/// glyph atlas, and the parked `load_font` requests) lives behind the one
/// `feature = "text-native"` gate, so a transport-only build never names
/// `TextCapabilityState` nor pulls `fontdue` / `aether_substrate` through
/// this cap.
pub struct TextCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names a `fontdue` /
// `aether_substrate` type — the handler/init ctx, the runtime state, the
// helper methods — lives in the `runtime` module below, gated once by
// `feature = "text-native"` and written cfg-free within; the `#[actor] impl`
// reaches all of it through the single `use runtime::*` glob. The kind types
// (`DrawText` / `LoadFont` / …) stay always-on via `pub use kinds::*` at
// module root — the always-on `HandlesKind<K>` markers name them.
use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the impl
// reaches all of it (state, ctx types, helpers) through this single seam, so
// the glob is intentional rather than a dozen one-line imports.
#[cfg(feature = "text-native")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

// The atlas / layout helpers the handlers name live in their own
// `pub(super)` submodules (not in `runtime`, which therefore can't re-export
// them past their visibility), so the handlers import them straight from
// here. Same `text-native` gate as the runtime emission that uses them.
#[cfg(feature = "text-native")]
use atlas::{AtlasEntry, GlyphKey, GlyphSlot};
#[cfg(feature = "text-native")]
use layout::{
    build_font_metrics, emit_draw, font_name_from_path, glyph_dimensions, glyph_quad, quantize_size,
};

// The text cap's runtime half gates on `text-native` (which implies `native`
// + the `fontdue` dep), not the generic `runtime` feature — so the `#[actor]`
// macro's runtime emission is steered onto the same gate via
// `runtime_feature = "text-native"` (iamacoffeepot/aether#2330).
#[actor(singleton, runtime_feature = "text-native")]
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

// The runtime half — the whole `fontdue` / `aether_substrate`-typed surface
// (imports, `TextCapabilityState`, the helper methods) — lives in
// `runtime.rs`, gated once here. The `#[actor] impl` above reaches it through
// the `use runtime::*` glob.
#[cfg(feature = "text-native")]
mod runtime;

#[cfg(all(test, feature = "text-native"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::atlas::{ATLAS_SIZE, GlyphKey, GlyphSlot};
    use super::layout::build_font_metrics;
    use super::runtime::{
        Arc, CreateTexture, NativeCtx, QuadSpace, Read, Source, TextCapabilityState, UpdateTexture,
    };
    use super::*;
    use crate::fs::FsError;
    use crate::render::DrawTexturedQuads;
    use crate::test_chassis::{
        TestChassis, decode_session_reply, drive_task_completion, fresh_substrate,
        test_mailer_and_rx,
    };
    use aether_actor::Addressable;
    use aether_data::{Kind, MailId, SessionToken, SourceAddr, Uuid};
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
        let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_draw_text(
            &mut state,
            &mut ctx,
            DrawText {
                font_id: 99,
                text: "hi".to_owned(),
                size_pixels: 32.0,
                color: [1.0; 4],
                origin: [0.0, 0.0],
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
        let mut state = TextCapabilityState::new();
        // Register a font directly — the parse path is covered above;
        // here we exercise the lazy-create branch of `draw`.
        let font = test_font();
        state.fonts.insert(0, Arc::new(font));
        let (binding, rx) = ctx_binding();
        let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_draw_text(
            &mut state,
            &mut ctx,
            DrawText {
                font_id: 0,
                text: "hi".to_owned(),
                size_pixels: 32.0,
                color: [1.0; 4],
                origin: [0.0, 0.0],
                space: QuadSpace::Screen,
            },
        );
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

        let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_draw_text(
            &mut state,
            &mut ctx,
            DrawText {
                font_id: 0,
                text: "A".to_owned(),
                size_pixels: 48.0,
                color: [1.0, 1.0, 1.0, 1.0],
                origin: [0.0, 0.0],
                space: QuadSpace::Screen,
            },
        );
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
        // `GlyphSlot` are in scope via the `use super::runtime::{…}` import
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
        let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_draw_text(
            &mut state,
            &mut ctx,
            DrawText {
                font_id: 0,
                text: "A".to_owned(),
                size_pixels: 48.0,
                color: [1.0, 1.0, 1.0, 1.0],
                origin: [0.0, 0.0],
                space: QuadSpace::Screen,
            },
        );

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
        let mut ctx = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_draw_text(
            &mut state,
            &mut ctx,
            DrawText {
                font_id: 0,
                text: "A".to_owned(),
                size_pixels: 24.0,
                color: [1.0, 1.0, 1.0, 1.0],
                origin: [0.0, 0.0],
                space: QuadSpace::Screen,
            },
        );
        let quads_zero = collect_draw_textured_quads(&binding, &rx).quads;

        // Second draw at a non-zero origin — glyph is cached, so only
        // DrawTexturedQuads is emitted (no UpdateTexture).
        let ox = 30.0f32;
        let oy = 50.0f32;
        let mut ctx2 = NativeCtx::new(&binding, session_sender(), MailId::NONE, MailId::NONE);
        TextCapability::on_draw_text(
            &mut state,
            &mut ctx2,
            DrawText {
                font_id: 0,
                text: "A".to_owned(),
                size_pixels: 24.0,
                color: [1.0, 1.0, 1.0, 1.0],
                origin: [ox, oy],
                space: QuadSpace::Screen,
            },
        );
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
        fontdue::Font::from_bytes(test_font_bytes(), fontdue::FontSettings::default())
            .expect("test setup: vendored Roboto Mono parses")
    }

    /// The raw bytes of [`test_font`], for the read-result tests that
    /// feed the parse path a real TTF.
    fn test_font_bytes() -> &'static [u8] {
        include_bytes!("../../../aether-substrate-bundle/assets/fonts/RobotoMono.ttf")
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
