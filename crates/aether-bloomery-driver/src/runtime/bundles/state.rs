//! One bundle digest's shared load lifecycle (ADR-0226 decision 2).

use aether_bloomery_kinds::Detail;

use super::DeclaredRoles;

/// Shared load lifecycle of one bundle digest (ADR-0226 decision 2).
#[derive(Debug)]
pub enum LoadState {
    /// The one artifact read is in flight.
    Reading,
    /// Decoded; the wasm is held only until the load.
    Declared {
        /// The roles the bundle declares.
        roles: DeclaredRoles,
        /// Wasm bytes, moved into the load command.
        wasm: Vec<u8>,
    },
    /// The one `LoadComponent` is in flight.
    Loading {
        /// The roles the bundle declares.
        roles: DeclaredRoles,
    },
    /// Loaded for the engine's life; the shell holds the root's reference.
    Ready {
        /// The roles the bundle declares.
        roles: DeclaredRoles,
    },
    /// Read, decode, or load failed; blocks both roles.
    Unavailable(Detail),
}

impl LoadState {
    /// The roles the bundle declares, once its sections have decoded.
    pub fn roles(&self) -> Option<&DeclaredRoles> {
        match self {
            Self::Declared { roles, .. } | Self::Loading { roles } | Self::Ready { roles, .. } => Some(roles),
            Self::Reading | Self::Unavailable(_) => None,
        }
    }
}
