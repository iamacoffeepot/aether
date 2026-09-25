//! Guest-side blob support (ADR-0238). The value type, [`aether_data::Blob`],
//! and its streaming reader live in `aether-data`; this module holds what only
//! a wasm guest needs: the `GuestHold` backing behind a `Shared` value the
//! engine delivers, its gated grant, re-exported hidden from the crate root,
//! and the encoder a guest's sends use, which writes a held value by hash.
//! The backing and the grant are wasm32-only, since the host build of the SDK
//! never holds a guest blob; there the encoder is the plain one.

pub mod guest;
