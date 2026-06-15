//! Pure, wasm-safe font-unit → pixel scaling for the ADR-0105 text
//! surface. The `aether.text.font_metrics` grab replies with a
//! [`FontMetrics`] table whose every measure is in
//! font units; [`scale_units`] is the one primitive that turns a
//! font-unit measure into pixels at a draw size, with no `fontdue`
//! dependency so a wasm guest can run it.
//!
//! The operation order is load-bearing. fontdue computes a glyph's pixel
//! advance as `(size_pixels / units_per_em) * advance_units` — the
//! division first, then a single multiply. Reproducing that order here
//! makes a guest's local measurement match the `aether.text` cap's
//! draw-path advance bit-for-bit (advances carry no kerning or shaping,
//! so a run's extent is the plain left-to-right sum of per-glyph
//! advances).

use alloc::collections::BTreeMap;

use crate::FontMetrics;

/// Scale a font-unit measure — a glyph advance, an ascent, a descent, a
/// line gap — to pixels at `size_pixels` for a font whose em square has
/// `units_per_em` subdivisions.
///
/// Computes `(size_pixels / units_per_em) * units` with the division
/// first, matching fontdue's `scale_factor(px) * value` ordering exactly,
/// so a guest reproduces the cap's draw-path advances without `fontdue`.
#[must_use]
pub fn scale_units(units: f32, size_pixels: f32, units_per_em: f32) -> f32 {
    let scale = size_pixels / units_per_em;
    units * scale
}

/// A cached font metric table with a per-codepoint advance lookup built
/// once from the wire [`FontMetrics`]. Construct it from the
/// `FontMetricsResult::Ok` payload and reuse it for every measurement at
/// any draw size.
///
/// Pure data over [`FontMetrics`] + [`scale_units`] — no actor or FFI
/// dependency — so a guest component and a native consumer measure text
/// the same way.
pub struct CachedFontMetrics {
    units_per_em: f32,
    default_advance: f32,
    advances: BTreeMap<u32, f32>,
}

impl CachedFontMetrics {
    /// Build the cache from a grabbed [`FontMetrics`] table.
    #[must_use]
    pub fn new(metrics: &FontMetrics) -> Self {
        let advances = metrics
            .advances
            .iter()
            .map(|glyph| (glyph.codepoint, glyph.advance_units))
            .collect();
        Self {
            units_per_em: metrics.units_per_em,
            default_advance: metrics.default_advance,
            advances,
        }
    }

    /// A codepoint's advance in font units, falling back to the
    /// `.notdef` advance for a codepoint the font has no glyph for — the
    /// same fallback the draw path takes.
    fn advance_units(&self, ch: char) -> f32 {
        self.advances
            .get(&u32::from(ch))
            .copied()
            .unwrap_or(self.default_advance)
    }

    /// The pixel width `text` occupies at `size_pixels` — the sum of its
    /// glyph advances, the run's extent the cap would draw.
    #[must_use]
    pub fn measure(&self, text: &str, size_pixels: f32) -> f32 {
        let mut pen = 0.0;
        for ch in text.chars() {
            pen += scale_units(self.advance_units(ch), size_pixels, self.units_per_em);
        }
        pen
    }

    /// The pixel x of the caret sitting after the first `char_index`
    /// characters of `text` at `size_pixels` (clamped to the string's
    /// length). Monotonic in `char_index`: `caret_x(text, i, ..)` never
    /// exceeds `caret_x(text, i + 1, ..)`, and the caret past the last
    /// character equals [`measure`](Self::measure).
    #[must_use]
    pub fn caret_x(&self, text: &str, char_index: usize, size_pixels: f32) -> f32 {
        let mut pen = 0.0;
        for ch in text.chars().take(char_index) {
            pen += scale_units(self.advance_units(ch), size_pixels, self.units_per_em);
        }
        pen
    }
}

#[cfg(test)]
mod cached_font_metrics_tests {
    use super::CachedFontMetrics;
    use crate::{FontMetrics, GlyphAdvance};
    use alloc::vec;

    /// A 1000-upm em where every mapped glyph is 600 units wide — a
    /// monospace table, so the arithmetic is easy to check by hand.
    fn monospace_metrics() -> FontMetrics {
        FontMetrics {
            units_per_em: 1000.0,
            ascent: 800.0,
            descent: -200.0,
            line_gap: 0.0,
            default_advance: 600.0,
            advances: vec![
                GlyphAdvance {
                    codepoint: u32::from('a'),
                    advance_units: 600.0,
                },
                GlyphAdvance {
                    codepoint: u32::from('b'),
                    advance_units: 600.0,
                },
                GlyphAdvance {
                    codepoint: u32::from('c'),
                    advance_units: 600.0,
                },
            ],
        }
    }

    #[test]
    fn measure_sums_scaled_advances() {
        let cache = CachedFontMetrics::new(&monospace_metrics());
        // 3 glyphs × 600 units × (100 / 1000) px = 180 px.
        assert_eq!(cache.measure("abc", 100.0), 180.0);
        // Linear in size: doubling the size doubles the extent.
        assert_eq!(cache.measure("abc", 200.0), 360.0);
    }

    #[test]
    fn unmapped_codepoint_uses_default_advance() {
        let cache = CachedFontMetrics::new(&monospace_metrics());
        // 'z' is absent → default_advance (600), the same as 'a'.
        assert_eq!(cache.measure("z", 100.0), cache.measure("a", 100.0));
    }

    #[test]
    fn caret_x_is_monotonic_and_ends_at_measure() {
        let cache = CachedFontMetrics::new(&monospace_metrics());
        let text = "abc";
        let size = 50.0;
        let count = text.chars().count();

        let mut prev = 0.0;
        for index in 0..=count {
            let x = cache.caret_x(text, index, size);
            assert!(x >= prev, "caret must not move backward at {index}");
            prev = x;
        }
        assert_eq!(cache.caret_x(text, count, size), cache.measure(text, size));
        // Past-the-end clamps to the full width.
        assert_eq!(cache.caret_x(text, 99, size), cache.measure(text, size));
    }
}

#[cfg(test)]
mod tests {
    use super::scale_units;

    #[test]
    fn scales_proportionally_to_size() {
        // 600 units in a 1000-upm em is 0.6 em; at 100 px that is 60 px.
        assert_eq!(scale_units(600.0, 100.0, 1000.0), 60.0);
    }

    #[test]
    fn advance_is_linear_in_size() {
        // Doubling the size doubles the advance — the property the grab
        // relies on to ship one size-independent table.
        let units_per_em = 2048.0;
        let units = 1234.0;
        let small = scale_units(units, 16.0, units_per_em);
        let large = scale_units(units, 32.0, units_per_em);
        let tolerance = f32::EPSILON * large.abs().max(1.0);
        assert!((large - 2.0 * small).abs() <= tolerance);
    }

    #[test]
    fn zero_units_is_zero_pixels() {
        assert_eq!(scale_units(0.0, 48.0, 1000.0), 0.0);
    }
}
