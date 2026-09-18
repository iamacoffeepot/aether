//! The host's persistence bundle (ADR-0137, issue 2687).
//!
//! One entry written by `on_dehydrate` and read by `on_rehydrate`, serialized
//! into the host's own parent state bytes: the script source, the resident
//! running script copy, the script's opaque `state_save` blob, and the
//! wrapped child's alias id. On reload the composite walk reconstructs the
//! **wrapped child itself** from its own real config + runtime state (#2694),
//! so the host does *not* re-spawn it — it re-instantiates only its own script
//! from `script_bytes` (no fs re-fetch), offers `script_state` to the fresh
//! script's `state_load`, and restores `wrapped_child_id` so the fallback's
//! lane-direction check works on the first post-reload mail. An undecodable
//! blob boots the script fresh with a warning (fail-open).

use alloc::vec::Vec;

use aether_data::wire;
use serde::{Deserialize, Serialize};

use crate::host::config::ScriptSource;

/// Leading byte on an encoded [`HostPersist`]. Bumped when the bundle shape
/// changes so a host decoding an older/newer blob boots fresh rather than
/// misreading it.
pub const HOST_PERSIST_VERSION: u8 = 1;

/// The host's durable state across a `replace_component` swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPersist {
    /// The script source, so a reload records where the script came from
    /// (a subsequent `load_script` correlates against it).
    pub script_source: ScriptSource,
    /// The resident running script's bytes — what a reload re-instantiates,
    /// no fs re-fetch.
    pub script_bytes: Vec<u8>,
    /// The script's opaque `state_save` blob, offered to the fresh script's
    /// `state_load`.
    pub script_state: Vec<u8>,
    /// The wrapped child's alias id (`0` when the host runs wrapper-less),
    /// restored so the fallback's direction check works immediately.
    pub wrapped_child_id: u64,
}

impl HostPersist {
    /// Encode to the host's parent state bytes: a [`HOST_PERSIST_VERSION`]
    /// byte then the `aether_data::wire` body.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let body = wire::to_vec(self).unwrap_or_default();
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(HOST_PERSIST_VERSION);
        out.extend_from_slice(&body);
        out
    }

    /// Decode a bundle written by [`Self::encode`]. `None` on an empty buffer,
    /// an unrecognized version byte, or a malformed body — the host boots
    /// fresh (fail-open) in each case.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&version, body) = bytes.split_first()?;
        if version != HOST_PERSIST_VERSION {
            return None;
        }
        wire::from_bytes(body).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    // Tripwire: a bundle round-trips through the versioned frame so a reload
    // restores the running script from its resident bytes (no fs call) and the
    // wrapped-child id for the direction check; and an undecodable blob decodes
    // to `None` so the host boots fresh rather than misreading it.
    #[test]
    fn bundle_round_trips_and_rejects_garbage() {
        let bundle = HostPersist {
            script_source: ScriptSource::FsRef { namespace: "assets".into(), path: "scripts/knob.wasm".into() },
            script_bytes: vec![0, 97, 115, 109],
            script_state: vec![1, 2, 3, 4],
            wrapped_child_id: 0xDEAD_BEEF,
        };
        let encoded = bundle.encode();
        assert_eq!(HostPersist::decode(&encoded), Some(bundle));

        // Empty and wrong-version buffers both fail open to `None`.
        assert_eq!(HostPersist::decode(&[]), None);
        let mut wrong = encoded;
        wrong[0] = HOST_PERSIST_VERSION.wrapping_add(1);
        assert_eq!(HostPersist::decode(&wrong), None);
    }
}
