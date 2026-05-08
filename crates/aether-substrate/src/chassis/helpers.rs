//! Chassis-side helpers used by capability dispatchers.
//!
//! `decode_payload` is the one error-message shape every control-plane
//! handler uses for postcard decode. The mail-side resolve helpers
//! (`resolve_bundle`, `register_or_match_all`) live in `mail/helpers.rs`.

/// Postcard-decode a control-plane payload with the one error-message
/// shape every handler uses. Handlers wrap the `String` in their own
/// `*Result::Err` variant — the shape is uniform, the enum differs.
pub fn decode_payload<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| format!("postcard decode failed: {e}"))
}
