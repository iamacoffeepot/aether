//! The ground-material vocabulary and its wire-byte conversions.

/// The ground-material vocabulary. Rides the wire as a raw `u8` inside a
/// `Bytes` plane, never as a schema enum, so a plane has one canonical
/// byte-array form. `Void` (`0`) is "nothing here".
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Material {
    #[default]
    Void = 0,
    Grass = 1,
    Dirt = 2,
    Stone = 3,
    Sand = 4,
    Water = 5,
}

impl Material {
    /// The raw wire byte.
    #[must_use]
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    /// Decode a wire byte, degrading an unknown value to [`Material::Void`]
    /// rather than erroring — a malformed plane byte becomes empty space,
    /// not a panic.
    #[must_use]
    pub fn from_u8_or_void(byte: u8) -> Self {
        Self::try_from(byte).unwrap_or(Self::Void)
    }
}

impl TryFrom<u8> for Material {
    type Error = u8;

    fn try_from(byte: u8) -> Result<Self, u8> {
        match byte {
            0 => Ok(Self::Void),
            1 => Ok(Self::Grass),
            2 => Ok(Self::Dirt),
            3 => Ok(Self::Stone),
            4 => Ok(Self::Sand),
            5 => Ok(Self::Water),
            other => Err(other),
        }
    }
}

/// Decode a cliff-material byte: an unknown value or `Void` reads as
/// [`Material::Stone`] — a cliff face always wears a paintable material.
/// Shared with [`SetRegion::into_region`](super::super::kinds::SetRegion) in the
/// sibling `kinds` module.
pub(in crate::world) fn cliff_material_from_u8(byte: u8) -> Material {
    match Material::try_from(byte) {
        Ok(Material::Void) | Err(_) => Material::Stone,
        Ok(material) => material,
    }
}
