//! Shelf-packed RGBA8 glyph atlas for the `aether.text` cap (ADR-0105).
//!
//! Pure CPU, no GPU: a fixed-size pixel buffer plus a shelf packer and a
//! glyph cache keyed by `(font_id, glyph_index, quantized size)`. A
//! rasterized glyph's coverage lands in the alpha channel; rgb is opaque
//! white so the draw quad's `tint` colors the text. The cap uploads the
//! whole (initially zeroed) buffer once via `create_texture`, then one
//! sub-rect per newly-placed glyph via `update_texture` — so a glyph
//! costs an upload the first frame it appears and is a cache hit after.
//!
//! When the atlas fills, further new glyphs log-and-drop for the session
//! (eviction is an ADR-0105 non-goal).

use std::collections::HashMap;

/// Side length of the square atlas in pixels. One fixed texture per
/// session; 512×512 holds a few hundred small glyphs.
pub const ATLAS_SIZE: u32 = 512;

/// One transparent-pixel gutter between packed glyphs so bilinear
/// sampling at a quad edge never bleeds a neighbor's coverage in.
const GLYPH_PADDING: u32 = 1;

/// Cache key for a rasterized glyph: which font, which glyph index, at
/// what integer pixel size. `size_pixels` is quantized (rounded) so two
/// draws at the same nominal size share one raster.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub font_id: u32,
    pub glyph_index: u16,
    pub size_pixels: u32,
}

/// A glyph's placed rect in the atlas — pixel position + size and the
/// matching uv sub-rect (`0,0` top-left .. `1,1` bottom-right) to thread
/// into a `TexturedQuad`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasEntry {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
}

/// Outcome of looking a glyph up in the atlas.
pub enum GlyphSlot {
    /// Cached or newly placed — sample the atlas at `entry`. `uploaded`
    /// is `true` only the frame the glyph was first rasterized, signaling
    /// the caller to emit one `update_texture` for `entry`'s rect.
    Placed { entry: AtlasEntry, uploaded: bool },
    /// The glyph has no coverage (a space, or a zero-area raster) —
    /// nothing to draw, advance the pen and move on.
    Empty,
    /// The atlas is full and could not place this glyph — dropped for the
    /// session.
    Full,
}

/// Map a cache entry (`Some(rect)` placed, `None` empty) to a reused
/// [`GlyphSlot`] — shared by [`Atlas::cached`] and the hit arm of
/// [`Atlas::get_or_insert`].
fn cached_slot(entry: Option<AtlasEntry>) -> GlyphSlot {
    entry.map_or(GlyphSlot::Empty, |entry| GlyphSlot::Placed {
        entry,
        uploaded: false,
    })
}

/// Fixed-size RGBA8 atlas with a left-to-right, top-to-bottom shelf
/// packer.
pub struct Atlas {
    pixels: Vec<u8>,
    cache: HashMap<GlyphKey, Option<AtlasEntry>>,
    shelf_x: u32,
    shelf_y: u32,
    shelf_height: u32,
    full: bool,
}

impl Default for Atlas {
    fn default() -> Self {
        Self::new()
    }
}

impl Atlas {
    /// A zeroed (fully transparent) atlas.
    pub fn new() -> Self {
        Self {
            pixels: vec![0u8; (ATLAS_SIZE * ATLAS_SIZE * 4) as usize],
            cache: HashMap::new(),
            shelf_x: 0,
            shelf_y: 0,
            shelf_height: 0,
            full: false,
        }
    }

    /// The full RGBA8 buffer — uploaded once via `create_texture`.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Cheap cache probe: the cached slot for `key`, or `None` if the
    /// glyph has never been seen (the caller then rasterizes and calls
    /// [`Self::get_or_insert`]). Lets the cap skip rasterization on a hit.
    pub fn cached(&self, key: &GlyphKey) -> Option<GlyphSlot> {
        self.cache.get(key).map(|cached| cached_slot(*cached))
    }

    /// Look the glyph up, rasterizing + packing it on a miss. `coverage`
    /// is fontdue's grayscale bitmap (`width * height` bytes, row-major);
    /// pass an empty slice / zero dimensions for a glyph with no pixels.
    ///
    /// uv coordinates are an exact `pixel / ATLAS_SIZE` ratio; both fit in
    /// `f32` without loss for any in-bounds glyph.
    #[allow(clippy::cast_precision_loss)]
    pub fn get_or_insert(
        &mut self,
        key: GlyphKey,
        width: u32,
        height: u32,
        coverage: &[u8],
    ) -> GlyphSlot {
        if let Some(cached) = self.cache.get(&key) {
            return cached_slot(*cached);
        }

        if width == 0 || height == 0 || coverage.len() < (width * height) as usize {
            self.cache.insert(key, None);
            return GlyphSlot::Empty;
        }

        let Some((x, y)) = self.pack(width, height) else {
            // No cache entry on a full miss: the rect could still fit a
            // smaller later glyph, and the caller log-and-drops this one.
            self.full = true;
            return GlyphSlot::Full;
        };

        self.blit(x, y, width, height, coverage);
        let size = ATLAS_SIZE as f32;
        let entry = AtlasEntry {
            x,
            y,
            width,
            height,
            u0: x as f32 / size,
            v0: y as f32 / size,
            u1: (x + width) as f32 / size,
            v1: (y + height) as f32 / size,
        };
        self.cache.insert(key, Some(entry));
        GlyphSlot::Placed {
            entry,
            uploaded: true,
        }
    }

    /// The RGBA8 bytes of a placed glyph's rect, row-major — the payload
    /// for the `update_texture` that uploads it.
    pub fn rect_rgba(&self, entry: &AtlasEntry) -> Vec<u8> {
        let mut out = Vec::with_capacity((entry.width * entry.height * 4) as usize);
        for row in 0..entry.height {
            let start = (((entry.y + row) * ATLAS_SIZE + entry.x) * 4) as usize;
            let end = start + (entry.width * 4) as usize;
            out.extend_from_slice(&self.pixels[start..end]);
        }
        out
    }

    /// Reserve a `width × height` rect (plus padding) on the current or a
    /// fresh shelf. `None` once the atlas can hold no more.
    fn pack(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        let padded_width = width + GLYPH_PADDING;
        let padded_height = height + GLYPH_PADDING;
        if padded_width > ATLAS_SIZE {
            return None;
        }
        if self.shelf_x + padded_width > ATLAS_SIZE {
            self.shelf_y = self.shelf_y.checked_add(self.shelf_height)?;
            self.shelf_x = 0;
            self.shelf_height = 0;
        }
        if self.shelf_y + padded_height > ATLAS_SIZE {
            return None;
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        self.shelf_x += padded_width;
        self.shelf_height = self.shelf_height.max(padded_height);
        Some((x, y))
    }

    /// Write a glyph's coverage into the atlas at `(x, y)` as opaque-white
    /// RGB with coverage in alpha.
    fn blit(&mut self, x: u32, y: u32, width: u32, height: u32, coverage: &[u8]) {
        for row in 0..height {
            for col in 0..width {
                let cov = coverage[(row * width + col) as usize];
                let px = (((y + row) * ATLAS_SIZE + (x + col)) * 4) as usize;
                self.pixels[px] = 255;
                self.pixels[px + 1] = 255;
                self.pixels[px + 2] = 255;
                self.pixels[px + 3] = cov;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(glyph_index: u16) -> GlyphKey {
        GlyphKey {
            font_id: 0,
            glyph_index,
            size_pixels: 32,
        }
    }

    #[test]
    fn packs_a_glyph_and_reports_it_uploaded() {
        let mut atlas = Atlas::new();
        let coverage = vec![200u8; 4 * 6];
        match atlas.get_or_insert(key(1), 4, 6, &coverage) {
            GlyphSlot::Placed { entry, uploaded } => {
                assert!(uploaded, "first placement must signal an upload");
                assert_eq!(entry.width, 4);
                assert_eq!(entry.height, 6);
                assert_eq!((entry.x, entry.y), (0, 0));
                // The rect's alpha carries the coverage, rgb is white.
                let rect = atlas.rect_rgba(&entry);
                assert_eq!(&rect[0..4], &[255, 255, 255, 200]);
            }
            _ => panic!("expected a placed glyph"),
        }
    }

    #[test]
    fn reuses_a_cached_glyph_without_re_upload() {
        let mut atlas = Atlas::new();
        let coverage = vec![128u8; 5 * 5];
        let first = match atlas.get_or_insert(key(2), 5, 5, &coverage) {
            GlyphSlot::Placed { entry, uploaded } => {
                assert!(uploaded);
                entry
            }
            _ => panic!("expected a placed glyph"),
        };
        match atlas.get_or_insert(key(2), 5, 5, &coverage) {
            GlyphSlot::Placed { entry, uploaded } => {
                assert!(!uploaded, "a cache hit must not re-upload");
                assert_eq!(entry, first, "cache hit returns the same rect");
            }
            _ => panic!("expected a cached glyph"),
        }
    }

    #[test]
    fn zero_area_glyph_is_empty_and_cached() {
        let mut atlas = Atlas::new();
        assert!(matches!(
            atlas.get_or_insert(key(3), 0, 0, &[]),
            GlyphSlot::Empty
        ));
        // Cached as empty — a second lookup is still Empty, no panic on
        // the empty coverage slice.
        assert!(matches!(
            atlas.get_or_insert(key(3), 0, 0, &[]),
            GlyphSlot::Empty
        ));
    }

    #[test]
    fn new_shelf_starts_when_the_row_fills() {
        let mut atlas = Atlas::new();
        // A glyph almost as wide as the atlas leaves no room beside it, so
        // the next glyph must drop to a fresh shelf below.
        let wide = vec![255u8; (ATLAS_SIZE as usize - 2) * 4];
        let GlyphSlot::Placed { entry: first, .. } =
            atlas.get_or_insert(key(10), ATLAS_SIZE - 2, 4, &wide)
        else {
            panic!("wide glyph should place on the first shelf");
        };
        let small = vec![255u8; 4 * 4];
        let GlyphSlot::Placed { entry, .. } = atlas.get_or_insert(key(11), 4, 4, &small) else {
            panic!("small glyph should open a new shelf");
        };
        assert_eq!(entry.x, 0, "new shelf restarts at the left edge");
        assert!(entry.y > first.y, "new shelf sits below the full first row");
    }

    #[test]
    fn reports_full_when_the_atlas_cannot_grow() {
        let mut atlas = Atlas::new();
        // Fill shelf by shelf with tall bands until one no longer fits.
        let band_height = 64u32;
        let coverage = vec![255u8; (ATLAS_SIZE * band_height) as usize];
        let mut saw_full = false;
        for glyph_index in 0..32u16 {
            match atlas.get_or_insert(key(glyph_index), ATLAS_SIZE, band_height, &coverage) {
                GlyphSlot::Placed { .. } => {}
                GlyphSlot::Full => {
                    saw_full = true;
                    break;
                }
                GlyphSlot::Empty => panic!("a full band is not empty"),
            }
        }
        assert!(saw_full, "the atlas should report Full once it cannot grow");
    }
}
