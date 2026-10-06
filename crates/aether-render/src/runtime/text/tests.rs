//! Unit tests for the renderer's text draws, driven through the pumped
//! render rig: mail in through production dispatch, state read in a host
//! turn. No GPU: nothing here sends a frame. The font-creation path, with
//! its staged parse, is covered by the text scenarios; these start with
//! the vendored font registered.

use aether_data::Blob;
use aether_kinds::QuadSpace;
use aether_math::Rgba;

use super::atlas::{ATLAS_SIZE, GlyphKey, GlyphSlot};
use crate::kinds::{
    CreateFontResult, CreateTexture, CreateTextureResult, DestroyTexture, DrawText, TextRun, TextureFormat,
    TextureSampling, TextureUsage, TexturedQuad, UpdateTexture,
};
use crate::runtime::overlay::OverlayBatch;
use crate::runtime::tests::RenderFixture;
use crate::runtime::texture::GLYPH_ATLAS_TEXTURE_ID;
use crate::runtime::{RenderCapabilityState, RenderParams};

/// The declared pixel bytes of the atlas texture.
const ATLAS_BYTES: usize = ATLAS_SIZE as usize * ATLAS_SIZE as usize * 4;

/// A booted renderer with the vendored font registered, and its `font_id`.
fn render_with_font() -> (RenderFixture, u32) {
    let mut render = RenderFixture::boot(RenderParams::default());
    let font = fontdue::Font::from_bytes(
        include_bytes!("../../../assets/fonts/RobotoMono.ttf").as_slice(),
        fontdue::FontSettings::default(),
    )
    .expect("test setup: vendored Roboto Mono parses");

    let created = render.cap.host_turn(|state, _ctx| state.text.register(Ok(font))).expect("the slot is live");
    let CreateFontResult::Ok { font_id } = created else {
        panic!("a fresh session registers its first font: {created:?}");
    };
    (render, font_id)
}

fn run(font_id: u32, text: &str, x: f32) -> TextRun {
    TextRun { font_id, text: text.to_owned(), size_pixels: 24.0, color: Rgba::WHITE, origin: [x, 0.0] }
}

fn draw(runs: Vec<TextRun>) -> DrawText {
    DrawText { clip: None, space: QuadSpace::Screen, runs }
}

/// The quads of the last overlay batch the frame has accumulated, which
/// must be a textured batch over the reserved glyph atlas.
fn glyph_quads(state: &RenderCapabilityState) -> Vec<TexturedQuad> {
    let Some(OverlayBatch::Textured { texture_id, quads, .. }) = state.overlay_frame.last() else {
        panic!("a text draw accumulates a textured batch, got {} batches", state.overlay_frame.len());
    };
    assert_eq!(*texture_id, GLYPH_ATLAS_TEXTURE_ID, "a text batch samples the reserved glyph atlas");
    quads.clone()
}

/// The atlas texture's staged pixels.
fn atlas_pixels(state: &RenderCapabilityState) -> Vec<u8> {
    state.textures.entries[&GLYPH_ATLAS_TEXTURE_ID].pixels.bytes().to_vec()
}

/// Catches a run with an unknown font that aborts the batch, or that
/// takes a neighbouring run down with it.
#[test]
fn a_run_with_an_unknown_font_is_dropped_and_the_runs_around_it_draw() {
    let (mut render, font_id) = render_with_font();

    render.send(&draw(vec![run(font_id, "A", 0.0), run(font_id + 99, "ignored", 24.0), run(font_id, "B", 48.0)]));

    let quads = render.read(glyph_quads);
    assert_eq!(quads.len(), 2, "the unknown-font run alone is dropped");
    assert!(quads[0].x < quads[1].x, "the surviving runs keep their listed order");
}

/// Catches a laid-out glyph reused across sizes: runs of one string at two
/// sizes that round to one raster, and at a third twice as large, each
/// advance and size their quads by their own size.
#[test]
fn a_glyph_laid_out_at_one_size_is_not_reused_at_another() {
    let (mut render, font_id) = render_with_font();
    let sized = |size_pixels: f32| TextRun { size_pixels, ..run(font_id, "AA", 0.0) };

    render.send(&draw(vec![sized(16.0), sized(16.4), sized(32.0)]));

    let quads = render.read(glyph_quads);
    let [small, nearby, large] = [0, 2, 4].map(|first| quads[first + 1].x - quads[first].x);
    assert!(nearby > small, "16.4 px advances further than 16 px though both sample one raster");
    let ratio = large / small;
    assert!((ratio - 2.0).abs() < 0.01, "32 px advances twice as far as 16 px, not {ratio} times");
    assert!(quads[4].width > quads[0].width, "the 32 px glyph's quad is the larger");
}

/// Catches a full atlas that drops the draw's glyphs instead of resetting,
/// a reset that clears the packer but leaves the old image in the texture,
/// where a later glyph's gutter would sample it, and a glyph laid out
/// before the reset that keeps the rect it had then.
#[test]
fn a_draw_after_the_atlas_fills_resets_it_and_still_draws() {
    let (mut render, font_id) = render_with_font();
    let last = ATLAS_BYTES - 4;
    render.send(&draw(vec![run(font_id, "BA", 0.0)]));
    let placed_second = render.read(glyph_quads)[1].u0;
    assert!(placed_second > 0.0, "precondition: `A` was first placed beside `B`, away from the origin");
    render
        .cap
        .host_turn(|state, _ctx| {
            // Stand in for an old glyph in the far corner, then fill the
            // packer with bands until one no longer fits.
            let stale =
                state.textures.ensure_glyph_atlas().apply_subrect(ATLAS_SIZE - 1, ATLAS_SIZE - 1, 1, 1, &[9; 4]);
            assert!(stale, "the far corner is inside the atlas");
            let band_height = ATLAS_SIZE / 4;
            let coverage = vec![255u8; ((ATLAS_SIZE - 2) * band_height) as usize];
            for glyph_index in 0..32u16 {
                let key = GlyphKey { font_id: u32::MAX, glyph_index, size_pixels: 64 };
                let slot = state.text.atlas.get_or_insert(key, ATLAS_SIZE - 2, band_height, &coverage);
                if matches!(slot, GlyphSlot::Full) {
                    break;
                }
            }
            assert!(state.text.atlas.is_full(), "precondition: the atlas is full");
        })
        .expect("the slot is live");

    render.send(&draw(vec![run(font_id, "A", 0.0)]));

    let (quads, pixels, full) =
        render.read(|state| (glyph_quads(state), atlas_pixels(state), state.text.atlas.is_full()));
    assert!(!full, "the draw reset the atlas");
    assert_eq!((quads[0].u0, quads[0].v0), (0.0, 0.0), "the glyph packs at the origin of the reset atlas");
    assert_eq!(&pixels[last..], &[0; 4], "the old image is zeroed out of the texture");
    assert!(pixels.iter().any(|&byte| byte != 0), "the draw's own glyph is written into the texture");
}

/// Catches the atlas leaking into the caller's id space: an update or a
/// destroy naming its reserved id changes nothing, and the first texture a
/// caller creates still gets id 0.
#[test]
fn the_reserved_atlas_id_is_refused_and_the_first_created_texture_is_still_zero() {
    let (mut render, font_id) = render_with_font();
    render.send(&draw(vec![run(font_id, "A", 0.0)]));
    let before = render.read(atlas_pixels);

    render.send(&UpdateTexture {
        texture_id: GLYPH_ATLAS_TEXTURE_ID,
        x: 0,
        y: 0,
        width: ATLAS_SIZE,
        height: 1,
        pixels: vec![7; ATLAS_SIZE as usize * 4],
    });
    render.send(&DestroyTexture { texture_id: GLYPH_ATLAS_TEXTURE_ID });
    let created: CreateTextureResult = render.request(&CreateTexture {
        width: 1,
        height: 1,
        format: TextureFormat::Rgba8,
        sampling: TextureSampling::Linear,
        usage: TextureUsage::Sampled,
        pixels: Blob::from(vec![0; 4]),
    });

    assert_eq!(render.read(atlas_pixels), before, "the atlas survives the destroy with its pixels as they were");
    assert!(
        matches!(created, CreateTextureResult::Ok { texture_id: 0 }),
        "the caller's first texture id is not shifted by the atlas: {created:?}",
    );
}

/// Catches an atlas registered uncounted, or counted again on every draw.
#[test]
fn the_atlas_is_counted_once_on_the_texture_gauge() {
    let (mut render, font_id) = render_with_font();
    let counted = |render: &RenderFixture| render.read(|state| state.textures.memory.bytes());
    assert_eq!(counted(&render), 0, "precondition: nothing is counted before the first text draw");

    render.send(&draw(vec![run(font_id, "A", 0.0)]));
    let after_first = counted(&render);
    render.send(&draw(vec![run(font_id, "B", 0.0)]));

    assert_eq!(after_first, ATLAS_BYTES, "the first draw counts the atlas at its pixel bytes");
    assert_eq!(counted(&render), ATLAS_BYTES, "a second draw adds nothing");
}
