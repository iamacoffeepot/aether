//! What the renderer holds to draw text (ADR-0105, ADR-0248 §10): the
//! fonts registered by `create_font`, and the glyph atlas's packer and
//! cache. The atlas pixels are the reserved glyph-atlas entry of the
//! texture registry the render state already owns.
//!
//! All of it is plain state of the render actor, behind no lock, atomic or
//! shared handle: every reader and writer is a handler of that one actor,
//! which handles one mail at a time. The font parse runs on a blocking
//! worker that shares nothing with the actor: it takes the bytes by move
//! and its output comes back as the task-completion mail.

use std::collections::HashMap;

use aether_data::Blob;
use aether_kinds::QuadSpace;
use aether_substrate::actor::native::{Held, NativeCtx};
use aether_substrate::session_ids::SessionIds;

use super::texture::TextureRegistry;
use crate::kinds::{CreateFontResult, DrawText, FontMetricsResult, TexturedQuad};

// The shelf packer and glyph cache over the reserved atlas texture.
mod atlas;
// Per-run layout into textured quads, and the font metrics table.
mod layout;
#[cfg(test)]
mod tests;

pub use self::atlas::ATLAS_SIZE;
use self::atlas::Atlas;
use self::layout::{build_font_metrics, lay_out_run};

/// What a staged font parse hands back: the parsed font, or the reason the
/// bytes are not one.
pub type FontParseOutput = Result<fontdue::Font, String>;

/// Context a staged font parse carries into its completion (ADR-0243 §9):
/// the held reply of the `create_font` that asked for it.
#[aether_data::kind(name = "aether.render.font_parse")]
pub struct FontParse {
    pub held: Held<CreateFontResult>,
}

/// Parse a font from the whole of `blob`. Runs on a blocking worker.
fn parse_font_bytes(blob: &Blob) -> FontParseOutput {
    let Some(bytes) = blob.contiguous() else {
        return Err("font bytes are not resident in this process".to_owned());
    };

    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
        .map_err(|error| format!("font parse failed: {error}"))
}

/// Stage the parse of a font's `bytes` off the renderer's turn (ADR-0243
/// §9), with the `create_font` request's held reply as the task's context.
/// The completion takes the context back and answers it.
pub fn stage_font_parse<A>(ctx: &mut NativeCtx<'_, A>, held: Held<CreateFontResult>, bytes: Blob) {
    ctx.stage_blocking_with::<FontParseOutput, FontParse>(FontParse { held })
        .start(ctx, move || parse_font_bytes(&bytes));
}

/// The render actor's text state: the fonts and the glyph atlas.
pub struct TextState {
    /// The registered fonts by the `font_id` a `CreateFontResult::Ok`
    /// handed back. Each is owned here and borrowed for the length of one
    /// layout.
    fonts: HashMap<u32, fontdue::Font>,
    /// Source of the `font_id`s: in sequence, never reused in a session.
    font_ids: SessionIds<u32>,
    /// Where each rasterized glyph sits in the reserved atlas texture.
    atlas: Atlas,
}

impl TextState {
    pub fn new() -> Self {
        Self { fonts: HashMap::new(), font_ids: SessionIds::new(), atlas: Atlas::new() }
    }

    /// Register the outcome of a font parse and build the reply its
    /// `create_font` is owed: the new `font_id`, the parse error, or that
    /// the session has no font id left.
    pub fn register(&mut self, parsed: FontParseOutput) -> CreateFontResult {
        let font = match parsed {
            Ok(font) => font,
            Err(error) => return CreateFontResult::Err { error },
        };
        let Some(font_id) = self.font_ids.allocate() else {
            return CreateFontResult::Err { error: "this session has run out of font ids".to_owned() };
        };

        self.fonts.insert(font_id, font);
        tracing::info!(target: "aether_render", font_id, "font created");
        CreateFontResult::Ok { font_id }
    }

    /// The metrics table of the font `font_id` names, or `Err` naming an id
    /// nothing is registered under.
    pub fn metrics(&self, font_id: u32) -> FontMetricsResult {
        self.fonts.get(&font_id).map_or_else(
            || FontMetricsResult::Err { error: format!("unknown font_id {font_id}") },
            |font| FontMetricsResult::Ok { metrics: build_font_metrics(font) },
        )
    }

    /// Lay every run of `mail` out into the quads of one batch over the
    /// reserved glyph-atlas texture in `textures`, registering that texture
    /// on first use and writing each glyph not yet in it. A full atlas is
    /// reset first, its texture zeroed with it, so this draw's glyphs pack
    /// into an empty one. A run naming an unknown font, or a size that is
    /// not finite and positive, is dropped with a warning and the runs
    /// around it still draw.
    pub fn lay_out(&mut self, textures: &mut TextureRegistry, mail: &DrawText) -> Vec<TexturedQuad> {
        let texture = textures.ensure_glyph_atlas();
        if self.atlas.is_full() {
            tracing::info!(target: "aether_render", "glyph atlas full; resetting it for this draw");
            self.atlas.reset();
            texture.clear();
        }

        let world = matches!(mail.space, QuadSpace::World { .. });
        let mut quads = Vec::new();
        for run in &mail.runs {
            let Some(font) = self.fonts.get(&run.font_id) else {
                tracing::warn!(target: "aether_render", font_id = run.font_id, "draw_text run for unknown font_id; dropping");
                continue;
            };
            let sized = run.size_pixels.is_finite() && run.size_pixels > 0.0;
            if !sized {
                tracing::warn!(
                    target: "aether_render",
                    size_pixels = run.size_pixels,
                    "draw_text run whose size is not finite and positive; dropping",
                );
                continue;
            }

            lay_out_run(font, &mut self.atlas, texture, run, world, &mut quads);
        }
        quads
    }
}
