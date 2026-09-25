//! Guest-side blob support (ADR-0238). The value type, [`aether_data::Blob`],
//! and its streaming reader live in `aether-data`; this module holds what only
//! a wasm guest needs: the `GuestHold` backing behind a `Shared` value the
//! engine delivers, and its gated grant, re-exported hidden from the crate
//! root. wasm32-only: the host build of the SDK never holds a guest blob.

#[cfg(target_arch = "wasm32")]
pub mod guest;
