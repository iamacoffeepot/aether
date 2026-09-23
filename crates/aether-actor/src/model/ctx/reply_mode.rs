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
//!
//! The trait is sealed through a private supertrait so a guest crate
//! cannot add a third mode — the closed set of two classes is what the
//! `#[actor]` macro's one downgrade-only coercion (`as_single`) relies on.

mod sealed {
    /// Private supertrait sealing [`super::ReplyMode`] — only the marker
    /// types in this module can implement it, so the mode set is closed.
    pub trait Sealed {}
}

/// Marker selecting a per-handler ctx's reply surface (ADR-0112,
/// ADR-0134). Sealed: the only implementors are [`Single`] and [`Manual`].
pub trait ReplyMode: sealed::Sealed {}

/// single-class marker (ADR-0112): the handler replies 0-or-1 through
/// its return value (ADR-0109). The default `M` on both ctx types, so
/// an unmarked `WasmCtx<'_>` / `NativeCtx<'_>` is the single-mode view.
pub struct Single;

/// manual-class marker (ADR-0112): the handler issues its own replies
/// via `OutboundReply` (`reply` / `reply_to`), which is implemented
/// only for this mode.
pub struct Manual;

impl sealed::Sealed for Single {}
impl sealed::Sealed for Manual {}

impl ReplyMode for Single {}
impl ReplyMode for Manual {}

#[cfg(test)]
mod tests {
    use super::{Manual, Single};
    use core::mem::size_of;

    /// The mode markers are zero-sized — the invariant the layout-
    /// identity reborrow in `WasmCtx::as_single` (and the native
    /// counterpart) rests on (a `PhantomData<M>` field stays a ZST for
    /// every `M`).
    #[test]
    fn reply_mode_types_are_zsts() {
        assert_eq!(size_of::<Single>(), 0);
        assert_eq!(size_of::<Manual>(), 0);
    }
}
