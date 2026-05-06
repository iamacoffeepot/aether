//! Shared `wait_reply` helper used by synchronous SDK wrappers (today
//! only handle round-trips). Each family carries its own error enum
//! (e.g. `SyncHandleError`), so the helper is generic over both the
//! reply kind `K` and an error type that implements [`WaitError`].
//! The transport `T` is the third generic — picks `WasmTransport` for
//! guests and `NativeTransport` for native capabilities (Phase 2).
//!
//! The wasm-side `io::*_sync` / `net::fetch_blocking` wrappers that
//! historically rode this helper retired across issues #589 (net) and
//! #591 (io); the helper stays for the handle path and as the shared
//! shape any future ctx-level `send_sync` lands on.
//!
//! ADR-0042: the substrate echoes the request's correlation id on the
//! reply; the host fn `wait_reply` parks the actor thread until a
//! mail of kind `K` with the matching correlation arrives (or the
//! timeout elapses). The three sentinel return codes (`-1` / `-2` /
//! `-3`) map onto the [`WaitError`] constructors.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use aether_data::Kind;

use crate::transport::MailTransport;

/// Error contract every sync wrapper's error enum needs to implement
/// so [`wait_reply`] can construct the four post-FFI failure modes
/// without knowing which family it's serving.
pub trait WaitError {
    fn timeout() -> Self;
    fn buffer_too_small() -> Self;
    fn cancelled() -> Self;
    fn decode(message: String) -> Self;
}

/// Allocate a `capacity`-sized scratch buffer in actor memory, park
/// on `transport.wait_reply` for a mail of kind `K` with the given
/// `expected_correlation`, and postcard-decode the written bytes.
///
/// `transport` is the actor-bound `MailTransport` instance — see
/// `transport.rs` for the `&self` receiver design and how it
/// type-system-tracks the actor binding.
pub fn wait_reply<K, E, T>(
    transport: &T,
    timeout_ms: u32,
    capacity: usize,
    expected_correlation: u64,
) -> Result<K, E>
where
    K: Kind + serde::de::DeserializeOwned,
    E: WaitError,
    T: MailTransport,
{
    let mut buf: Vec<u8> = vec![0u8; capacity];
    let rc = transport.wait_reply(K::ID.0, &mut buf, timeout_ms, expected_correlation);
    decode_wait_reply::<K, E>(rc, &buf)
}

/// Pure rc → `Result<K, E>` mapping extracted from [`wait_reply`] so
/// the four sentinel branches and the unexpected-rc fallback are
/// testable without a transport (`MailTransport` impls panic / no-op
/// off their target). The happy path postcard-decodes
/// `&buf[..rc as usize]`, matching what the in-FFI version does after
/// the host fn writes `rc` bytes into the scratch buffer.
pub fn decode_wait_reply<K, E>(rc: i32, buf: &[u8]) -> Result<K, E>
where
    K: serde::de::DeserializeOwned,
    E: WaitError,
{
    match rc {
        -1 => Err(E::timeout()),
        -2 => Err(E::buffer_too_small()),
        -3 => Err(E::cancelled()),
        n if n >= 0 => {
            let len = n as usize;
            postcard::from_bytes(&buf[..len]).map_err(|e| E::decode(alloc::format!("{e}")))
        }
        _ => Err(E::decode(alloc::format!(
            "unexpected wait_reply return: {rc}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    // Per-impl mapping tests cover `SyncHandleError` (and any future
    // sync-wait error enum). They live in their owning module's test
    // block so each enum's variant set stays next to its definition.
    // The helper-level tests below exercise the rc → branch mapping
    // itself via a dummy `WaitError` that just records which
    // constructor fired.

    /// Tag-recording stub `WaitError` so [`decode_wait_reply`] tests
    /// can assert which sentinel branch the helper picked without
    /// depending on any of the three production enums.
    #[derive(Debug, PartialEq, Eq)]
    enum DummyTag {
        Timeout,
        BufferTooSmall,
        Cancelled,
        Decode(String),
    }

    impl WaitError for DummyTag {
        fn timeout() -> Self {
            DummyTag::Timeout
        }
        fn buffer_too_small() -> Self {
            DummyTag::BufferTooSmall
        }
        fn cancelled() -> Self {
            DummyTag::Cancelled
        }
        fn decode(message: String) -> Self {
            DummyTag::Decode(message)
        }
    }

    /// Tiny payload type — postcard-encodes to a single byte so the
    /// happy-path test can hand-craft the buffer without pulling in
    /// any of the substrate kinds. Lives behind `Deserialize` only;
    /// the helper doesn't need `Kind` (the kind id only flows into
    /// the FFI call, which the extracted helper sidesteps).
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Payload(u8);

    #[test]
    fn rc_minus_one_maps_to_timeout() {
        let res = decode_wait_reply::<Payload, DummyTag>(-1, &[]);
        assert_eq!(res, Err(DummyTag::Timeout));
    }

    #[test]
    fn rc_minus_two_maps_to_buffer_too_small() {
        let res = decode_wait_reply::<Payload, DummyTag>(-2, &[]);
        assert_eq!(res, Err(DummyTag::BufferTooSmall));
    }

    #[test]
    fn rc_minus_three_maps_to_cancelled() {
        let res = decode_wait_reply::<Payload, DummyTag>(-3, &[]);
        assert_eq!(res, Err(DummyTag::Cancelled));
    }

    #[test]
    fn unexpected_negative_rc_maps_to_decode() {
        // -4 isn't a documented sentinel; it should fall through to
        // the catch-all decode branch with a diagnostic that names
        // the rc, so a future substrate-side sentinel addition is
        // visible in the error.
        let res = decode_wait_reply::<Payload, DummyTag>(-4, &[]);
        match res {
            Err(DummyTag::Decode(msg)) => {
                assert!(
                    msg.contains("-4"),
                    "expected unexpected-rc message to mention -4, got {msg:?}"
                );
                assert!(
                    msg.contains("unexpected wait_reply return"),
                    "expected unexpected-rc message to be tagged, got {msg:?}"
                );
            }
            other => panic!("expected DummyTag::Decode for rc=-4, got {other:?}"),
        }
    }

    #[test]
    fn nonnegative_rc_postcard_decodes_buffer_prefix() {
        // postcard encoding of `Payload(0x42)` is the single byte
        // `0x42`; rc=1 tells the helper to decode the first byte.
        // Trailing bytes in the buffer must be ignored — the FFI
        // contract is that the host fn wrote exactly `rc` bytes.
        let buf = [0x42u8, 0xff, 0xff];
        let res = decode_wait_reply::<Payload, DummyTag>(1, &buf);
        assert_eq!(res, Ok(Payload(0x42)));
    }

    #[test]
    fn nonnegative_rc_decode_failure_maps_to_decode_variant() {
        // postcard's varint decoder rejects an empty slice for a
        // u8-shaped payload. The error message is postcard's, so we
        // only assert the variant + that *some* message landed.
        let res = decode_wait_reply::<Payload, DummyTag>(0, &[]);
        match res {
            Err(DummyTag::Decode(msg)) => {
                assert!(!msg.is_empty(), "decode error message should be non-empty");
            }
            other => panic!("expected DummyTag::Decode for empty buffer, got {other:?}"),
        }
    }

    /// Confirms each constructor on the trait is wired to a distinct
    /// `DummyTag` variant — guards against a future trait-method
    /// rename silently re-pointing one of the rc branches.
    #[test]
    fn dummy_wait_error_constructors_are_distinct() {
        let tags = [
            DummyTag::timeout(),
            DummyTag::buffer_too_small(),
            DummyTag::cancelled(),
            DummyTag::decode(String::from("x")),
        ];
        // All four must be pairwise distinct.
        for (i, a) in tags.iter().enumerate() {
            for (j, b) in tags.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "constructors {i} and {j} produced the same tag");
                }
            }
        }
    }
}
