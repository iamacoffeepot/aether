use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

// The three colour kinds keep the explicit derive: `#[aether_data::kind]`'s
// `pod` option drops serde, and these do cross the JSON boundary — the
// `serde_json_shape_is_named_components` tripwire below round-trips `Rgba`
// through `serde_json`. Naming the bytemuck pair through the attribute's
// `derive(...)` escape hatch would state the set exactly but leave the crate
// with no first-party mention of `serde`, trading a shorter attribute for a
// `cargo-machete` suppression.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Pod,
    Zeroable,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
    aether_data::StorageLeaf,
)]
#[kind(name = "aether.color.rgb")]
pub struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Pod,
    Zeroable,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
    aether_data::StorageLeaf,
)]
#[kind(name = "aether.color.rgba")]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Pod,
    Zeroable,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
    aether_data::StorageLeaf,
)]
#[kind(name = "aether.color.hsl")]
pub struct Hsl {
    pub h: f32,
    pub s: f32,
    pub l: f32,
}

impl Rgb {
    pub const BLACK: Self = Self::new(0.0, 0.0, 0.0);
    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0);

    #[inline]
    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    /// Builds a linear colour from 8-bit sRGB bytes by the IEC 61966-2-1 decode.
    #[inline]
    #[must_use]
    pub const fn from_srgb8(r: u8, g: u8, b: u8) -> Self {
        Self::new(srgb8_channel_to_linear(r), srgb8_channel_to_linear(g), srgb8_channel_to_linear(b))
    }

    #[inline]
    #[must_use]
    pub const fn from_array(a: [f32; 3]) -> Self {
        Self::new(a[0], a[1], a[2])
    }

    #[inline]
    #[must_use]
    pub const fn to_array(self) -> [f32; 3] {
        [self.r, self.g, self.b]
    }

    #[inline]
    #[must_use]
    pub const fn extend(self, a: f32) -> Rgba {
        Rgba::new(self.r, self.g, self.b, a)
    }

    #[inline]
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        Self::new(self.r + (other.r - self.r) * t, self.g + (other.g - self.g) * t, self.b + (other.b - self.b) * t)
    }
}

impl Rgba {
    pub const BLACK: Self = Self::new(0.0, 0.0, 0.0, 1.0);
    pub const TRANSPARENT: Self = Self::new(0.0, 0.0, 0.0, 0.0);
    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0, 1.0);

    #[inline]
    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Builds a linear colour from 8-bit sRGB bytes by the IEC 61966-2-1 decode; alpha is `a / 255` with no curve.
    #[inline]
    #[must_use]
    pub const fn from_srgb8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::new(
            srgb8_channel_to_linear(r),
            srgb8_channel_to_linear(g),
            srgb8_channel_to_linear(b),
            alpha8_to_linear(a),
        )
    }

    #[inline]
    #[must_use]
    pub fn from_hsl(hsl: Hsl) -> Self {
        hsl.to_rgba()
    }

    #[inline]
    #[must_use]
    pub const fn from_array(a: [f32; 4]) -> Self {
        Self::new(a[0], a[1], a[2], a[3])
    }

    #[inline]
    #[must_use]
    pub const fn to_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a]
    }

    #[inline]
    #[must_use]
    pub const fn truncate(self) -> Rgb {
        Rgb::new(self.r, self.g, self.b)
    }

    #[inline]
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        Self::new(
            self.r + (other.r - self.r) * t,
            self.g + (other.g - self.g) * t,
            self.b + (other.b - self.b) * t,
            self.a + (other.a - self.a) * t,
        )
    }
}

impl Hsl {
    #[inline]
    #[must_use]
    pub const fn new(h: f32, s: f32, l: f32) -> Self {
        Self { h, s, l }
    }

    /// Hue, saturation and lightness describe an sRGB-encoded colour; the result is linear by the IEC 61966-2-1 decode.
    #[inline]
    #[must_use]
    pub fn to_rgb(self) -> Rgb {
        let saturation = self.s.clamp(0.0, 1.0);
        let lightness = self.l.clamp(0.0, 1.0);
        let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
        let hue_sector = (((self.h % 360.0) + 360.0) % 360.0) / 60.0;
        let secondary = chroma * (1.0 - ((hue_sector % 2.0) - 1.0).abs());
        let (red, green, blue) = if hue_sector < 1.0 {
            (chroma, secondary, 0.0)
        } else if hue_sector < 2.0 {
            (secondary, chroma, 0.0)
        } else if hue_sector < 3.0 {
            (0.0, chroma, secondary)
        } else if hue_sector < 4.0 {
            (0.0, secondary, chroma)
        } else if hue_sector < 5.0 {
            (secondary, 0.0, chroma)
        } else {
            (chroma, 0.0, secondary)
        };
        let match_value = lightness - chroma / 2.0;
        Rgb::new(
            srgb_channel_to_linear(red + match_value),
            srgb_channel_to_linear(green + match_value),
            srgb_channel_to_linear(blue + match_value),
        )
    }

    #[inline]
    #[must_use]
    pub fn to_rgba(self) -> Rgba {
        self.to_rgb().extend(1.0)
    }
}

#[inline]
#[must_use]
const fn srgb8_channel_to_linear(channel: u8) -> f32 {
    SRGB8_TO_LINEAR[channel as usize]
}

/// IEC 61966-2-1 decode of one encoded channel in `0.0..=1.0`.
///
/// The power `base^2.4` is written as `base^2 * (base^2)^(1/5)` because no `const` power function exists on the
/// pinned toolchain and `from_srgb8` must stay `const`.
const fn srgb_channel_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.04045 {
        return encoded / 12.92;
    }

    let base = (encoded + 0.055) / 1.055;
    let squared = base * base;

    squared * fifth_root(squared)
}

/// Newton's iteration for the fifth root, a fixed number of steps so it terminates by construction.
const fn fifth_root(value: f32) -> f32 {
    let mut root = 1.0;
    let mut step = 0;
    while step < 16 {
        root = (4.0 * root + value / (root * root * root * root)) / 5.0;
        step += 1;
    }

    root
}

const SRGB8_TO_LINEAR: [f32; 256] = srgb8_table();

const fn srgb8_table() -> [f32; 256] {
    let mut table = [0.0; 256];
    let mut channel = 0u8;
    loop {
        table[channel as usize] = srgb_channel_to_linear(channel as f32 / 255.0);
        if channel == u8::MAX {
            break;
        }
        channel += 1;
    }

    table
}

#[inline]
#[must_use]
const fn alpha8_to_linear(channel: u8) -> f32 {
    channel as f32 / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn repr_c_layout_matches_float_arrays() {
        // Tripwire: colors stay byte-identical to the float arrays they replace.
        assert_eq!(size_of::<Rgb>(), 12);
        assert_eq!(size_of::<Rgba>(), 16);
        assert_eq!(size_of::<Hsl>(), 12);
        assert_eq!(align_of::<Rgb>(), 4);
        assert_eq!(align_of::<Rgba>(), 4);
        assert_eq!(align_of::<Hsl>(), 4);
    }

    #[test]
    fn from_srgb8_decodes_every_byte_by_the_srgb_curve() {
        for byte in 0..=u8::MAX {
            let encoded = f64::from(byte) / 255.0;
            let reference = if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                libm::pow((encoded + 0.055) / 1.055, 2.4)
            };
            let error = (f64::from(Rgb::from_srgb8(byte, byte, byte).r) - reference).abs();
            assert!(error <= 1e-6, "byte {byte}: error {error}");
        }
    }

    #[test]
    fn from_srgb8_keeps_the_ends_exact_and_alpha_linear() {
        // Tripwire: black and white through the iteration are exactly 0.0 and 1.0, so a colour built from bytes still
        // equals `Rgba::BLACK` / `Rgba::WHITE`; alpha never takes the curve.
        assert_eq!(Rgba::from_srgb8(0, 0, 0, 255), Rgba::BLACK);
        assert_eq!(Rgba::from_srgb8(255, 255, 255, 255), Rgba::WHITE);
        assert_eq!(Rgba::from_srgb8(0, 0, 0, 128).a, 128.0 / 255.0);
    }

    #[test]
    fn hsl_primaries_map_to_linear_rgb() {
        // Tripwire: HSL primaries preserve the piecewise-chroma math; 0.0 and 1.0 are fixed points of the curve.
        assert_eq!(Hsl::new(0.0, 1.0, 0.5).to_rgb(), Rgb::new(1.0, 0.0, 0.0));
        assert_eq!(Hsl::new(120.0, 1.0, 0.5).to_rgb(), Rgb::new(0.0, 1.0, 0.0));
        assert_eq!(Hsl::new(240.0, 1.0, 0.5).to_rgb(), Rgb::new(0.0, 0.0, 1.0));
    }

    #[test]
    fn hsl_grey_matches_the_same_bytes() {
        let grey = Hsl::new(0.0, 0.0, 128.0 / 255.0).to_rgb();
        let bytes = Rgb::from_srgb8(128, 128, 128);
        assert!((grey.r - bytes.r).abs() <= 1e-6);
        assert!((grey.g - bytes.g).abs() <= 1e-6);
        assert!((grey.b - bytes.b).abs() <= 1e-6);
    }

    #[test]
    fn array_bridges_round_trip() {
        // Tripwire: legacy array bridges keep field order stable.
        let rgb = [0.1, 0.2, 0.3];
        let rgba = [0.1, 0.2, 0.3, 0.4];
        assert_eq!(Rgb::from_array(rgb).to_array(), rgb);
        assert_eq!(Rgba::from_array(rgba).to_array(), rgba);
    }

    #[test]
    fn defaults_are_zero() {
        // Tripwire: default remains the bytemuck zero value for nested kind migration.
        assert_eq!(Rgb::default(), Rgb::new(0.0, 0.0, 0.0));
        assert_eq!(Rgba::default(), Rgba::new(0.0, 0.0, 0.0, 0.0));
        assert_eq!(Hsl::default(), Hsl::new(0.0, 0.0, 0.0));
    }

    #[test]
    fn serde_json_shape_is_named_components() {
        // Tripwire: serde-path parents expose named color components to callers.
        let color = Rgba::new(0.1, 0.2, 0.3, 0.4);
        let json = serde_json::to_string(&color).expect("serialize rgba");
        assert_eq!(json, r#"{"r":0.1,"g":0.2,"b":0.3,"a":0.4}"#);
        assert_eq!(serde_json::from_str::<Rgba>(&json).expect("deserialize rgba"), color);
    }
}
