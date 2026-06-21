//! ADR-0048 §1 native-transform runtime types + link-time inventory.
//!
//! A transform is a **data-layer primitive** — a pure `Kind -> Kind`
//! function with zero dependence on the actor framework (no `Ctx`, no
//! mail, no lifecycle). Its surface therefore lives here, next to
//! `Kind` and the native descriptor inventory, not in the actor SDK.
//! The authoring macro `#[transform]` re-exports from `aether-data` as
//! `aether_data::transform` (impl in the sibling `aether-data-derive`
//! crate, which `aether-data` cannot itself be — a proc-macro crate
//! can't export runtime items).
//!
//! There is no FFI shim, no `extern "C"`, no wasm custom section — the
//! original wasm-export design was deferred before implementation
//! (ADR-0048 revision 2026-05-20). A transform is plain native Rust
//! collected at link time through [`TransformEntry`].

use alloc::vec::Vec;
use core::fmt;

use crate::KindId;
use crate::ids::TransformId;

/// Why a transform invocation failed (ADR-0048 §6). Encoding /
/// decoding the typed inputs and output is the only failure surface —
/// the transform fn itself returns a `Kind` value (a domain `Err` is a
/// successful, content-addressed output, not a [`TransformError`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformError {
    /// One input slice didn't decode against its declared input kind's
    /// canonical-bytes path. `slot` is the slot index (0-based).
    InputDecode { slot: usize },
    /// The number of supplied input slices didn't match the transform's
    /// declared input arity.
    InputArity { expected: usize, actual: usize },
    /// The encoded output exceeded the executor's output-byte cap
    /// (ADR-0048 §6). The cap itself lives in the executor; the macro's
    /// `invoke` thunk never enforces it, but the variant is reserved
    /// here so the executor and the thunk share one error type.
    OutputOverflow { limit: usize, actual: usize },
}

impl fmt::Display for TransformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputDecode { slot } => {
                write!(f, "transform input slot {slot} failed to decode")
            }
            Self::InputArity { expected, actual } => {
                write!(
                    f,
                    "transform input arity mismatch: expected {expected}, got {actual}"
                )
            }
            Self::OutputOverflow { limit, actual } => {
                write!(
                    f,
                    "transform output exceeded {limit} bytes (encoded {actual})"
                )
            }
        }
    }
}

/// Type-erased invocation thunk shape (ADR-0048 §1). The `#[transform]`
/// macro generates one of these per transform fn: it decodes each input
/// slice against its declared kind, calls the user fn, and encodes the
/// output. Inputs arrive in **slot-index order**.
pub type InvokeFn = fn(&[&[u8]]) -> Result<Vec<u8>, TransformError>;

/// Static-friendly mirror of a registered native transform, submitted
/// at link time by the `#[transform]` macro (the same `inventory`
/// pattern as `aether-data`'s native descriptor inventory). Owns
/// nothing but `&'static` data + a fn pointer so it is
/// const-constructible from `inventory::submit!`.
///
/// The substrate builds its `TransformRegistry`
/// (iamacoffeepot/aether#1012) by iterating [`transforms()`] once at
/// startup, keying on [`Self::transform_id`].
#[cfg(not(target_arch = "wasm32"))]
pub struct TransformEntry {
    /// Stable name-based id (ADR-0048 §1):
    /// `fnv1a_64(TRANSFORM_DOMAIN ++ "{crate}::{module_path}::{fn}")`,
    /// tagged `Tag::Transform`.
    pub transform_id: TransformId,
    /// Declared input kind ids, in parameter (slot) order. Up to 8.
    pub input_kind_ids: &'static [KindId],
    /// Declared output kind id.
    pub output_kind_id: KindId,
    /// `"{crate}::{module_path}::{fn}"` — diagnostics + MCP
    /// introspection.
    pub name: &'static str,
    /// Type-erased decode → call → encode thunk.
    pub invoke: InvokeFn,
}

#[cfg(not(target_arch = "wasm32"))]
inventory::collect!(TransformEntry);

/// Iterate every native transform collected at link time (ADR-0048
/// §1/§2). The substrate's `TransformRegistry` materializes from this
/// at startup; the set is fixed for the process lifetime.
#[cfg(not(target_arch = "wasm32"))]
pub fn transforms() -> impl Iterator<Item = &'static TransformEntry> {
    inventory::iter::<TransformEntry>.into_iter()
}

/// Re-exports the `#[transform]` macro points at so its generated
/// `invoke` thunk + inventory submission compile in a consumer crate
/// without that crate naming `inventory` or the runtime types directly.
/// Not part of the public API; the macro is the only intended caller.
#[doc(hidden)]
pub mod __transform_runtime {
    #[cfg(not(target_arch = "wasm32"))]
    pub use super::TransformEntry;
    pub use super::{InvokeFn, TransformError};
    pub use crate::Kind;
    pub use crate::KindId;
    pub use crate::ids::TransformId;
    #[cfg(not(target_arch = "wasm32"))]
    pub use ::inventory;
    pub use alloc::vec::Vec;
}
