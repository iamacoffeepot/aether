use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

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

    #[inline]
    #[must_use]
    pub const fn from_srgb8(r: u8, g: u8, b: u8) -> Self {
        Self::new(srgb8_channel_to_linear(r), srgb8_channel_to_linear(g), srgb8_channel_to_linear(b))
    }

    #[inline]
    #[must_use]
    //noinspection DuplicatedCode -- color and vector constructors are independent public const APIs.
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
    //noinspection DuplicatedCode -- color and vector constructors are independent public const APIs.
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
        let srgb = Rgb::new(red + match_value, green + match_value, blue + match_value);
        Rgb::new(srgb.r * srgb.r, srgb.g * srgb.g, srgb.b * srgb.b)
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
    let c = channel as f32 / 255.0;
    c * c
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
    fn srgb8_to_linear_preserves_approximate_transfer() {
        // Tripwire: RGB keeps the current approximate `(channel / 255)^2` transfer.
        assert_eq!(Rgba::from_srgb8(255, 128, 0, 255), Rgba::new(1.0, 0.251_964_66, 0.0, 1.0));
    }

    #[test]
    fn hsl_primaries_map_to_squared_linear_rgb() {
        // Tripwire: HSL primaries preserve the existing piecewise-chroma math.
        assert_eq!(Hsl::new(0.0, 1.0, 0.5).to_rgb(), Rgb::new(1.0, 0.0, 0.0));
        assert_eq!(Hsl::new(120.0, 1.0, 0.5).to_rgb(), Rgb::new(0.0, 1.0, 0.0));
        assert_eq!(Hsl::new(240.0, 1.0, 0.5).to_rgb(), Rgb::new(0.0, 0.0, 1.0));
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
