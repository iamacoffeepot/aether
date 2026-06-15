//! Pure, wasm-safe font-unit → pixel scaling for the ADR-0105 text
//! surface. The `aether.text.font_metrics` grab replies with a
//! [`FontMetrics`](crate::FontMetrics) table whose every measure is in
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
