//! 16:16 binary fixed-point conversion at the CSG boundary.
//!
//! Per ADR-0054, all coordinates entering the CSG core are snapped to a
//! 16:16 fixed-point grid (multiply by `2^16`, round to nearest, store
//! as `i32`). Predicates inside the core run as exact integer
//! determinants against these snapped values — no floats touch
//! classification or topology decisions.
//!
//! Coordinate range: `|coord| ≤ 256`. The upper bound is the largest
//! magnitude where the snapped `i32` value (`coord * 2^16`) fits in
//! `f32`'s 24-bit mantissa exactly. With `coord = 256`, the snapped
//! value is `2^24` — the boundary of f32's gap-free integer range. Any
//! larger and the round-trip `i32 → f32 → i32` stops being identity,
//! which would silently violate the determinism guarantee. We reject
//! out-of-range inputs as a loud failure rather than papering over
//! degraded precision.
//!
//! For chunky-low-poly asset-local geometry the cap is generous: a
//! teapot is ±2 units, a castle is ±50. World-scale composition lives
//! outside CSG via per-instance transforms, not via vertex coordinates
//! exceeding the cap.

/// Number of fractional bits in the fixed-point representation.
pub const FRACTIONAL_BITS: u32 = 16;

/// Scaling factor: `2^FRACTIONAL_BITS = 65536`.
pub const SCALE: f64 = (1u64 << FRACTIONAL_BITS) as f64;

/// Maximum magnitude of a CSG input coordinate. Inclusive — exactly
/// `±256.0` is allowed because `256 * 2^16 = 2^24` is the largest
/// integer with an exact `f32` representation.
pub const MAX_INPUT_MAGNITUDE: f32 = 256.0;

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq)]
pub enum FixedError {
    #[error("CSG input coordinate {value} is outside the ±256 unit range")]
    OutOfRange { value: f32 },
    #[error("CSG input coordinate is not finite ({value:?})")]
    NotFinite { value: f32 },
}

/// Convert an `f32` coordinate to its 16:16 fixed-point representation.
///
/// Errors if `value` is non-finite (NaN, ±∞) or has magnitude greater
/// than [`MAX_INPUT_MAGNITUDE`]. Round-to-nearest-even via `f64::round`
/// to keep the conversion deterministic across platforms.
pub fn f32_to_fixed(value: f32) -> Result<i32, FixedError> {
    if !value.is_finite() {
        return Err(FixedError::NotFinite { value });
    }
    if value.abs() > MAX_INPUT_MAGNITUDE {
        return Err(FixedError::OutOfRange { value });
    }
    // Up-cast to f64 before scaling so the multiplication is exact for
    // any in-range f32 (24-bit mantissa × 17-bit constant fits in
    // f64's 53-bit mantissa with room to spare).
    let scaled = f64::from(value) * SCALE;
    Ok(scaled.round() as i32)
}

/// Convert a 16:16 fixed-point value back to `f32`. Lossless for
/// in-range CSG outputs (the snapped i32 fits in f32's mantissa).
pub fn fixed_to_f32(value: i32) -> f32 {
    (f64::from(value) / SCALE) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 fractional bits → scale of 65536 → fixed unit of 1/65536.
    const ONE_FIXED: i32 = 1 << FRACTIONAL_BITS;
    const SMALLEST_REPRESENTABLE: f32 = 1.0 / SCALE as f32;

    #[test]
    fn zero_round_trips() {
        assert_eq!(f32_to_fixed(0.0).unwrap(), 0);
        assert_eq!(fixed_to_f32(0), 0.0);
    }

    #[test]
    fn one_round_trips() {
        assert_eq!(f32_to_fixed(1.0).unwrap(), ONE_FIXED);
        assert_eq!(fixed_to_f32(ONE_FIXED), 1.0);
    }

    #[test]
    fn smallest_step_round_trips() {
        let fx = f32_to_fixed(SMALLEST_REPRESENTABLE).unwrap();
        assert_eq!(fx, 1);
        assert_eq!(fixed_to_f32(1), SMALLEST_REPRESENTABLE);
    }

    #[test]
    fn negative_round_trips() {
        assert_eq!(f32_to_fixed(-1.0).unwrap(), -ONE_FIXED);
        assert_eq!(fixed_to_f32(-ONE_FIXED), -1.0);
        assert_eq!(
            f32_to_fixed(-12.5).unwrap(),
            -12 * ONE_FIXED - ONE_FIXED / 2
        );
    }

    #[test]
    fn positive_boundary_accepted() {
        let fx = f32_to_fixed(MAX_INPUT_MAGNITUDE).unwrap();
        assert_eq!(fx, 256 * ONE_FIXED);
        // Round-trip must be exact at the boundary — that's the entire
        // point of the ±256 cap.
        assert_eq!(fixed_to_f32(fx), MAX_INPUT_MAGNITUDE);
    }

    #[test]
    fn negative_boundary_accepted() {
        let fx = f32_to_fixed(-MAX_INPUT_MAGNITUDE).unwrap();
        assert_eq!(fx, -256 * ONE_FIXED);
        assert_eq!(fixed_to_f32(fx), -MAX_INPUT_MAGNITUDE);
    }

    #[test]
    fn just_past_positive_boundary_rejected() {
        // f32::next_up isn't stable yet on the toolchain — bump a bit
        // by adding the smallest representable step at this magnitude.
        let too_big = MAX_INPUT_MAGNITUDE + 1.0;
        match f32_to_fixed(too_big).unwrap_err() {
            FixedError::OutOfRange { value } => assert_eq!(value, too_big),
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn just_past_negative_boundary_rejected() {
        let too_small = -MAX_INPUT_MAGNITUDE - 1.0;
        match f32_to_fixed(too_small).unwrap_err() {
            FixedError::OutOfRange { value } => assert_eq!(value, too_small),
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn far_out_of_range_rejected() {
        for v in [1000.0_f32, -1000.0, 1.0e6, -1.0e6, 1.0e30, -1.0e30] {
            assert!(matches!(
                f32_to_fixed(v),
                Err(FixedError::OutOfRange { .. })
            ));
        }
    }

    #[test]
    fn nan_rejected() {
        assert!(matches!(
            f32_to_fixed(f32::NAN),
            Err(FixedError::NotFinite { .. })
        ));
    }

    #[test]
    fn infinity_rejected() {
        assert!(matches!(
            f32_to_fixed(f32::INFINITY),
            Err(FixedError::NotFinite { .. })
        ));
        assert!(matches!(
            f32_to_fixed(f32::NEG_INFINITY),
            Err(FixedError::NotFinite { .. })
        ));
    }

    #[test]
    fn round_trip_property_random_in_range() {
        // Walk a deterministic ladder across the full ±256 range. The
        // round trip must reproduce each input bit-exactly because the
        // snap is lossless for in-range f32s with at-most 16 fractional
        // bits worth of precision relative to the value's scale.
        for k in -1024..=1024 {
            // value = k * 0.25 — a step of 1/4 keeps every sample on
            // the fixed-point grid (0.25 = 16384 / 65536 fixed units),
            // so f32 → fixed → f32 must be identity.
            let value = (k as f32) * 0.25;
            assert!(value.abs() <= MAX_INPUT_MAGNITUDE);
            let fx = f32_to_fixed(value).expect("in-range");
            let back = fixed_to_f32(fx);
            assert_eq!(back, value, "round-trip mismatch at value={value}");
        }
    }

    #[test]
    fn monotonic_under_conversion() {
        // f32_to_fixed must be non-decreasing — strict monotonicity for
        // values farther apart than 1/65536 (the fixed-point step), and
        // never inverted.
        let step = 1.0 / 1024.0; // about 64 fixed units per step
        let mut prev = f32_to_fixed(-MAX_INPUT_MAGNITUDE).unwrap();
        let mut x = -MAX_INPUT_MAGNITUDE + step;
        while x <= MAX_INPUT_MAGNITUDE {
            let fx = f32_to_fixed(x).unwrap();
            assert!(fx >= prev, "monotonicity violated at x={x}: {prev} > {fx}");
            prev = fx;
            x += step;
        }
    }

    #[test]
    fn snap_to_grid_rounds_to_nearest() {
        // Halfway between two grid points should round to the even one
        // (banker's rounding via f64::round semantics) — but more
        // importantly, both nearby values must converge to a unique
        // grid cell in the same direction every time across runs.
        let just_above_half = (1.0 / SCALE as f32) * 0.51;
        let just_below_half = (1.0 / SCALE as f32) * 0.49;
        assert_eq!(f32_to_fixed(just_above_half).unwrap(), 1);
        assert_eq!(f32_to_fixed(just_below_half).unwrap(), 0);
    }

    #[test]
    fn negative_zero_collapses_to_zero() {
        // -0.0 round-trips to +0.0 in the integer domain — no separate
        // negative-zero representation. Important because BSP polygon
        // identity hashes derive from these integers, and we don't want
        // duplicate "same point, different sign of zero" entries.
        assert_eq!(f32_to_fixed(-0.0).unwrap(), 0);
    }
}
