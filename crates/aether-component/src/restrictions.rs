//! Host-owned prohibitions on individual component lifecycle operations.

use core::ops::{BitOr, BitOrAssign};

/// Operations a native bootstrap forbids for one trampoline slot.
///
/// These flags belong to the host, not the wasm guest or its manifest. They
/// remain attached to the trampoline when its resident component changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComponentRestrictions(u8);

impl ComponentRestrictions {
    /// Permit both individual lifecycle operations.
    pub const NONE: Self = Self(0);
    /// Reject an individual `DropComponent` request.
    pub const DROP: Self = Self(1);
    /// Reject a `ReplaceComponent` request.
    pub const REPLACE: Self = Self(2);

    /// Whether every flag in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for ComponentRestrictions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ComponentRestrictions {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}
