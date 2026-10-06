//! The glyph atlas's packer and cache (ADR-0105, ADR-0248 §10).
//!
//! Pure CPU bookkeeping: a shelf packer over a fixed square and a glyph
//! cache keyed by `(font_id, glyph_index, quantized size)`. It holds no
//! pixels. The atlas image lives once, as the staged pixels of the reserved
//! glyph-atlas texture in the renderer's texture registry; a newly placed
//! glyph comes back with its RGBA rows and the layout writes them there. A
//! rasterized glyph's coverage is the alpha channel and rgb is opaque white,
//! so the draw quad's `tint` colours the text.
//!
//! When a glyph cannot be placed, `is_full()` turns `true`. The layout calls
//! `reset()` at the top of the next text draw and zeroes the texture, then
//! places that draw's glyphs into the empty atlas as cache misses. The
//! saturating draw is missing the glyphs that did not fit; the next one
//! draws them all.

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
    /// Placed by an earlier lookup — sample the atlas at the entry.
    Cached(AtlasEntry),
    /// Placed by this lookup. `rgba` is the glyph's `entry.width *
    /// entry.height` RGBA8 pixels, row-major, which the caller writes into
    /// the atlas texture at the entry's rect before sampling it.
    Placed { entry: AtlasEntry, rgba: Vec<u8> },
    /// The glyph has no coverage (a space, or a zero-area raster) —
    /// nothing to draw, advance the pen and move on.
    Empty,
    /// The atlas is full and could not place this glyph. The layout resets
    /// the atlas at the top of the next draw so the glyph can be placed
    /// then.
    Full,
}

/// Map a cache entry (`Some(rect)` placed, `None` empty) to its
/// [`GlyphSlot`] — shared by [`Atlas::cached`] and the hit arm of
/// [`Atlas::get_or_insert`].
fn cached_slot(entry: Option<AtlasEntry>) -> GlyphSlot {
    entry.map_or(GlyphSlot::Empty, GlyphSlot::Cached)
}

/// A glyph's coverage as RGBA8: opaque-white rgb with the coverage in
/// alpha.
fn coverage_rgba(coverage: &[u8]) -> Vec<u8> {
    coverage.iter().flat_map(|&alpha| [255, 255, 255, alpha]).collect()
}

/// A left-to-right, top-to-bottom shelf packer over the fixed atlas square,
/// with the cache of what it has placed.
pub struct Atlas {
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
    /// An empty atlas.
    pub fn new() -> Self {
        Self { cache: HashMap::new(), shelf_x: 0, shelf_y: 0, shelf_height: 0, full: false }
    }

    /// `true` once any glyph failed to pack into the atlas. The layout
    /// checks this before each draw and calls [`Self::reset`] to free space
    /// for that draw's glyphs.
    pub fn is_full(&self) -> bool {
        self.full
    }

    /// Clear the glyph cache and return the shelf cursor to the origin so
    /// the atlas accepts new glyphs again. The caller zeroes the atlas
    /// texture in the same step, so no stale glyph is left where a later
    /// one's gutter would sample it.
    pub fn reset(&mut self) {
        self.cache.clear();
        self.shelf_x = 0;
        self.shelf_y = 0;
        self.shelf_height = 0;
        self.full = false;
    }

    /// Cheap cache probe: the cached slot for `key`, or `None` if the
    /// glyph has never been seen (the caller then rasterizes and calls
    /// [`Self::get_or_insert`]). Lets the layout skip rasterization on a
    /// hit.
    pub fn cached(&self, key: &GlyphKey) -> Option<GlyphSlot> {
        self.cache.get(key).map(|cached| cached_slot(*cached))
    }

    /// Look the glyph up, packing it on a miss. `coverage` is fontdue's
    /// grayscale bitmap (`width * height` bytes, row-major); pass an empty
    /// slice / zero dimensions for a glyph with no pixels.
    ///
    /// uv coordinates are an exact `pixel / ATLAS_SIZE` ratio; both fit in
    /// `f32` without loss for any in-bounds glyph.
    #[allow(clippy::cast_precision_loss)]
    pub fn get_or_insert(&mut self, key: GlyphKey, width: u32, height: u32, coverage: &[u8]) -> GlyphSlot {
        if let Some(cached) = self.cache.get(&key) {
            return cached_slot(*cached);
        }

        let covered = (width * height) as usize;
        if width == 0 || height == 0 || coverage.len() < covered {
            self.cache.insert(key, None);
            return GlyphSlot::Empty;
        }

        let Some((x, y)) = self.pack(width, height) else {
            // No cache entry on a full miss: the rect could still fit a
            // smaller later glyph, and the caller drops this one.
            self.full = true;
            return GlyphSlot::Full;
        };

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
        GlyphSlot::Placed { entry, rgba: coverage_rgba(&coverage[..covered]) }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(glyph_index: u16) -> GlyphKey {
        GlyphKey { font_id: 0, glyph_index, size_pixels: 32 }
    }

    /// Fill the atlas shelf by shelf with bands a quarter of its height
    /// until one no longer fits, and report whether it said so.
    fn fill(atlas: &mut Atlas) -> bool {
        let band_height = ATLAS_SIZE / 4;
        let coverage = vec![255u8; ((ATLAS_SIZE - 2) * band_height) as usize];
        for glyph_index in 0..32u16 {
            match atlas.get_or_insert(key(glyph_index), ATLAS_SIZE - 2, band_height, &coverage) {
                GlyphSlot::Placed { .. } => {}
                GlyphSlot::Full => return true,
                GlyphSlot::Cached(_) | GlyphSlot::Empty => panic!("a fresh band is newly placed"),
            }
        }
        false
    }

    /// Catches a cache hit that packs a second rect, or hands back pixels
    /// to write a second time.
    #[test]
    fn a_cached_glyph_is_not_placed_twice() {
        let mut atlas = Atlas::new();
        let coverage = vec![128u8; 5 * 5];
        let GlyphSlot::Placed { entry: first, rgba } = atlas.get_or_insert(key(2), 5, 5, &coverage) else {
            panic!("the first lookup places the glyph");
        };
        assert_eq!(&rgba[..4], &[255, 255, 255, 128], "coverage rides alpha under opaque white");
        assert_eq!(rgba.len(), 5 * 5 * 4, "one RGBA pixel per coverage byte");

        let GlyphSlot::Cached(again) = atlas.get_or_insert(key(2), 5, 5, &coverage) else {
            panic!("the second lookup is a cache hit");
        };
        assert_eq!(again, first, "the hit names the rect the first lookup placed");
    }

    /// Catches a zero-area glyph that packs a rect, or that is rasterized
    /// again on every draw because its emptiness was not cached.
    #[test]
    fn a_zero_area_glyph_is_cached_empty() {
        let mut atlas = Atlas::new();

        assert!(matches!(atlas.get_or_insert(key(3), 0, 0, &[]), GlyphSlot::Empty));

        assert!(matches!(atlas.cached(&key(3)), Some(GlyphSlot::Empty)), "the empty glyph is a cache hit");
    }

    /// Catches a packer that overlaps a glyph onto a full row instead of
    /// opening a shelf below it.
    #[test]
    fn a_new_shelf_starts_when_the_row_fills() {
        let mut atlas = Atlas::new();
        let wide = vec![255u8; (ATLAS_SIZE as usize - 2) * 4];
        let GlyphSlot::Placed { entry: first, .. } = atlas.get_or_insert(key(10), ATLAS_SIZE - 2, 4, &wide) else {
            panic!("the wide glyph places on the first shelf");
        };

        let GlyphSlot::Placed { entry, .. } = atlas.get_or_insert(key(11), 4, 4, &[255u8; 16]) else {
            panic!("the small glyph opens a new shelf");
        };

        assert_eq!(entry.x, 0, "the new shelf restarts at the left edge");
        assert!(entry.y > first.y, "the new shelf sits below the full first row");
    }

    /// Catches a packer that places past the bottom edge instead of
    /// reporting that it cannot grow.
    #[test]
    fn the_atlas_reports_full_when_it_cannot_grow() {
        let mut atlas = Atlas::new();

        assert!(fill(&mut atlas), "the atlas reports Full once a band no longer fits");
        assert!(atlas.is_full());
    }

    /// Catches a reset that leaves the full flag set, the shelf cursor
    /// where it was, or the old glyphs in the cache.
    #[test]
    fn a_reset_atlas_accepts_new_glyphs_at_the_origin() {
        let mut atlas = Atlas::new();
        assert!(fill(&mut atlas), "precondition: the atlas is full");

        atlas.reset();

        assert!(!atlas.is_full());
        assert!(atlas.cached(&key(0)).is_none(), "a glyph placed before the reset is no longer cached");
        let GlyphSlot::Placed { entry, .. } = atlas.get_or_insert(key(200), 4, 4, &[255u8; 16]) else {
            panic!("a reset atlas places a new glyph");
        };
        assert_eq!((entry.x, entry.y), (0, 0), "the shelf cursor is back at the origin");
    }
}
