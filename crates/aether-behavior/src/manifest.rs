//! The exports-manifest custom section (ADR-0137).
//!
//! A behavior script carries no `aether.kinds` section — it declares no
//! kinds, only handles kinds already flowing past it. What it does carry is
//! the id set its `#[on(K)]` handlers cover, so the host can skip the
//! interpreter entirely for undeclared kinds (a map lookup, not a wasm
//! call). The `#[behavior]` macro pins those ids into the
//! [`EXPORTS_SECTION`] custom section: a leading [`EXPORTS_MANIFEST_VERSION`]
//! byte then each handled kind id as a little-endian `u64`.

use aether_data::KindId;

/// Name of the wasm custom section the `#[behavior]` macro pins the handled
/// kind ids into.
pub const EXPORTS_SECTION: &str = "aether.behavior.exports";

/// Leading byte on the [`EXPORTS_SECTION`] bytes. Bumped when the framing
/// changes so a host reading an older/newer script's manifest fails loudly.
pub const EXPORTS_MANIFEST_VERSION: u8 = 1;

/// Recover the handled kind ids from an [`EXPORTS_SECTION`] byte buffer: a
/// version byte then little-endian `u64` words. A buffer that is empty, on
/// an unrecognized version, or whose body is not a whole number of `u64`
/// words yields no ids (the trailing partial word, if any, is dropped).
pub fn decode_exports_manifest(bytes: &[u8]) -> impl Iterator<Item = KindId> + '_ {
    let body = match bytes.split_first() {
        Some((&version, rest)) if version == EXPORTS_MANIFEST_VERSION => rest,
        _ => &[][..],
    };
    body.chunks_exact(8).map(|word| {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(word);
        KindId(u64::from_le_bytes(buf))
    })
}

#[cfg(test)]
mod tests {
    use aether_data::KindId;
    use alloc::vec::Vec;

    use super::{EXPORTS_MANIFEST_VERSION, decode_exports_manifest};

    // Tripwire: the exact framing the host reads a script's handled-kind
    // set out of — version byte then LE `u64` words. A hand-built buffer is
    // decoded back to its id list, so any drift in the byte layout (word
    // order, width, version handling) trips here rather than silently
    // reshaping every script's manifest.
    #[test]
    fn decode_exports_manifest_recovers_hand_built_ids() {
        let ids = [KindId(0x0102_0304_0506_0708), KindId(0xDEAD_BEEF_CAFE_F00D)];
        let mut buf = Vec::new();
        buf.push(EXPORTS_MANIFEST_VERSION);
        for id in ids {
            buf.extend_from_slice(&id.0.to_le_bytes());
        }

        let decoded: Vec<KindId> = decode_exports_manifest(&buf).collect();
        assert_eq!(decoded, ids);

        // A wrong version byte yields no ids.
        let mut wrong = buf.clone();
        wrong[0] = EXPORTS_MANIFEST_VERSION.wrapping_add(1);
        assert_eq!(decode_exports_manifest(&wrong).count(), 0);
    }
}
