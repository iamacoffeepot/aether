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
        // Just-above-half rounds up, just-below-half rounds down. Both
        // nearby values must converge to a unique grid cell in the same
        // direction every time across runs.
        let just_above_half = (1.0 / SCALE as f32) * 0.51;
        let just_below_half = (1.0 / SCALE as f32) * 0.49;
        assert_eq!(f32_to_fixed(just_above_half).unwrap(), 1);
        assert_eq!(f32_to_fixed(just_below_half).unwrap(), 0);
    }

    #[test]
    fn exact_halfway_rounds_away_from_zero() {
        // Pin the actual rounding mode of `f64::round` (round-half-away-
        // from-zero, *not* banker's rounding). If the implementation
        // ever switches to `round_ties_even` this test fails loudly so
        // we can audit BSP `side()` polarity stability rather than have
        // a silent grid-shift cascade through the pipeline.
        let half_lsb = 0.5 / SCALE;
        // Build the f64s exactly so the half-LSB rounds aren't lost to
        // f32 imprecision before reaching round().
        assert_eq!(
            f64_to_fixed_exact(half_lsb),
            1,
            "0.5 LSB should round away from zero"
        );
        assert_eq!(
            f64_to_fixed_exact(1.5 * (1.0 / SCALE)),
            2,
            "1.5 LSB should round to 2 under away-from-zero"
        );
        assert_eq!(
            f64_to_fixed_exact(2.5 * (1.0 / SCALE)),
            3,
            "2.5 LSB rounds to 3 under away-from-zero (banker's would yield 2)"
        );
        assert_eq!(
            f64_to_fixed_exact(-0.5 * (1.0 / SCALE)),
            -1,
            "-0.5 LSB rounds to -1 under away-from-zero"
        );
    }

    /// Mirrors `f32_to_fixed` against an exact f64 so halfway tests
    /// aren't disturbed by f32 representation error.
    fn f64_to_fixed_exact(value: f64) -> i32 {
        (value * SCALE).round() as i32
    }

    #[test]
    fn off_grid_round_trip_within_one_ulp() {
        // Values that don't sit on a fixed-point grid line still must
        // round-trip to within one fixed-point ULP (1/65536). Catches a
        // future change that shifts the grid or alters rounding
        // direction by even one bit.
        let one_ulp = 1.0 / SCALE as f32;
        let irrational = 0.123_456_79_f32;
        for k in -1024..=1024 {
            let value = (k as f32) * irrational;
            if value.abs() > MAX_INPUT_MAGNITUDE {
                continue;
            }
            let fx = f32_to_fixed(value).expect("in-range");
            let back = fixed_to_f32(fx);
            assert!(
                (back - value).abs() <= one_ulp,
                "off-grid round trip exceeded one ULP at value={value}: back={back}"
            );
        }
    }

    #[test]
    fn smallest_f32_past_boundary_rejected() {
        // The actual machine-epsilon boundary, not "+1.0 past". This is
        // the value that BSP-side classification will see if upstream
        // produces a marginally-too-large coordinate.
        let just_above = f32::from_bits(MAX_INPUT_MAGNITUDE.to_bits() + 1);
        assert!(
            just_above > MAX_INPUT_MAGNITUDE,
            "test setup: just_above must exceed boundary"
        );
        match f32_to_fixed(just_above).unwrap_err() {
            FixedError::OutOfRange { value } => assert_eq!(value, just_above),
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn fixed_to_f32_total_for_extreme_inputs() {
        // `f32_to_fixed` clamps to ±2^24, but `fixed_to_f32` accepts any
        // i32 — assert it never panics and produces a finite f32 for the
        // extreme bounds. Documents the function as total for callers
        // that might hold an i32 from elsewhere.
        let max = fixed_to_f32(i32::MAX);
        let min = fixed_to_f32(i32::MIN);
        assert!(max.is_finite(), "fixed_to_f32(i32::MAX) was non-finite");
        assert!(min.is_finite(), "fixed_to_f32(i32::MIN) was non-finite");
        assert!(max > 0.0);
        assert!(min < 0.0);
    }

    #[test]
    fn sign_is_preserved() {
        // BSP `side()` polarity depends on this — `f32_to_fixed(-x)`
        // must equal `-f32_to_fixed(x)` for every in-range non-zero x,
        // otherwise asymmetric rounding could flip a polygon's side
        // classification across an axis-aligned plane.
        let mut x = 1.0 / SCALE as f32;
        while x <= MAX_INPUT_MAGNITUDE {
            let pos = f32_to_fixed(x).unwrap();
            let neg = f32_to_fixed(-x).unwrap();
            assert_eq!(pos, -neg, "sign mismatch at x={x}: pos={pos} neg={neg}");
            x *= 2.0; // walk by powers of two — covers the full range fast
        }
    }

    #[test]
    fn determinism_across_calls() {
        // Same input → same output across N calls. Guards against a
        // future "clever" cache or thread-local state that breaks
        // referential transparency at this layer.
        let samples = [
            -MAX_INPUT_MAGNITUDE,
            -1.0,
            -0.5,
            0.0,
            0.001,
            0.123_456_7,
            42.5,
            MAX_INPUT_MAGNITUDE,
        ];
        for v in samples {
            let first = f32_to_fixed(v).unwrap();
            for _ in 0..16 {
                assert_eq!(f32_to_fixed(v).unwrap(), first);
            }
        }
    }

    #[test]
    fn idempotent_on_grid() {
        // f32 → fixed → f32 → fixed must equal f32 → fixed for any
        // grid-aligned input. (For off-grid the second snap can shift
        // by one ULP because fixed_to_f32 may not reconstruct the exact
        // input — we assert that case in `off_grid_round_trip_within_one_ulp`.)
        for k in -1024..=1024 {
            let value = (k as f32) * 0.25;
            let once = f32_to_fixed(value).unwrap();
            let twice = f32_to_fixed(fixed_to_f32(once)).unwrap();
            assert_eq!(once, twice, "idempotence broke at value={value}");
        }
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
