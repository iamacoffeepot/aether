//! [`ReplyMode`] — the phantom marker a per-handler ctx carries to
//! select which reply surface its handler class permits (ADR-0112,
//! ADR-0134).
//!
//! One ctx type per target (`WasmCtx` / `NativeCtx`) is parameterized by
//! a [`ReplyMode`] marker that defaults to [`Single`], so the common
//! signature stays `WasmCtx<'_>` / `NativeCtx<'_>`. The reply surface is
//! selected by which traits the per-mode ctx implements:
//!
//! - [`Single`] — 0-or-1 reply via the return value (ADR-0109). No
//!   `reply` / `reply_to` (a transitional exception holds while the
//!   migration runs; ADR-0112 §Consequences).
//! - [`Manual`] — the handler issues its own replies; `OutboundReply`
//!   (`reply` / `reply_to`) is implemented for this mode.
//! - [`Multi<K>`] — the handler emits 0..n mails of the declared kind
//!   `K` (ADR-0134); [`crate::Emit`] (`emit`) is implemented for this
//!   mode, each emission a detached chain root at the dispatch source.
//!
//! The trait is sealed through a private supertrait so a guest crate
//! cannot add a fourth mode — the closed set of three classes is what the
//! `#[actor]` macro's downgrade-only coercions (`as_single` / `as_multi`)
//! rely on.

use core::marker::PhantomData;

mod sealed {
    /// Private supertrait sealing [`super::ReplyMode`] — only the marker
    /// types in this module can implement it, so the mode set is closed.
    pub trait Sealed {}
}

/// Marker selecting a per-handler ctx's reply surface (ADR-0112,
/// ADR-0134). Sealed: the only implementors are [`Single`], [`Manual`],
/// and [`Multi<K>`].
pub trait ReplyMode: sealed::Sealed {}

/// single-class marker (ADR-0112): the handler replies 0-or-1 through
/// its return value (ADR-0109). The default `M` on both ctx types, so
/// an unmarked `WasmCtx<'_>` / `NativeCtx<'_>` is the single-mode view.
pub struct Single;

/// manual-class marker (ADR-0112): the handler issues its own replies
/// via `OutboundReply` (`reply` / `reply_to`), which is implemented
/// only for this mode.
pub struct Manual;

/// multi-class marker (ADR-0134): the handler emits 0..n mails of the
/// declared kind `K` via [`crate::Emit`] (`emit`), each a detached chain
/// root addressed at the dispatch source. `K` is the element kind the
/// `#[actor]` macro reads off the `Multi<K>` ctx signature to publish
/// `ReplyContract::Multi(K::ID)` on the inputs manifest. A ZST for every
/// `K` (a `PhantomData<K>` field), so the layout-identity reborrow in
/// `WasmCtx::as_multi` / `NativeCtx::as_multi` holds.
pub struct Multi<K>(PhantomData<K>);

impl sealed::Sealed for Single {}
impl sealed::Sealed for Manual {}
impl<K> sealed::Sealed for Multi<K> {}

impl ReplyMode for Single {}
impl ReplyMode for Manual {}
impl<K> ReplyMode for Multi<K> {}

#[cfg(test)]
mod tests {
    use super::{Manual, Multi, Single};
    use core::mem::size_of;

    /// The mode markers are zero-sized — the invariant the layout-
    /// identity reborrow in `WasmCtx::as_single` / `WasmCtx::as_multi`
    /// (and the native counterparts) rests on (a `PhantomData<M>` field
    /// stays a ZST for every `M`, and `Multi<K>` is a ZST for every `K`).
    #[test]
    fn reply_mode_types_are_zsts() {
        assert_eq!(size_of::<Single>(), 0);
        assert_eq!(size_of::<Manual>(), 0);
        assert_eq!(size_of::<Multi<u32>>(), 0);
        assert_eq!(size_of::<Multi<[u8; 64]>>(), 0);
    }
}
