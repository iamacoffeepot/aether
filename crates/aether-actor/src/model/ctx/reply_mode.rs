//! [`ReplyMode`] — the phantom marker a per-handler ctx carries to
//! select which reply surface its handler class permits (ADR-0112).
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
//! - [`Stream`] — a stream of replies; reserved (ADR-0112), the macro
//!   rejects `#[handler::stream]` until the stream surface is built.
//!
//! The trait is sealed through a private supertrait so a guest crate
//! cannot add a fourth mode — the closed set of three is what the
//! `#[actor]` macro's downgrade-only coercion (`as_single`) relies on.

mod sealed {
    /// Private supertrait sealing [`super::ReplyMode`] — only the three
    /// marker types in this module can implement it, so the mode set is
    /// closed.
    pub trait Sealed {}
}

/// Marker selecting a per-handler ctx's reply surface (ADR-0112).
/// Sealed: the only implementors are [`Single`], [`Manual`], and
/// [`Stream`].
pub trait ReplyMode: sealed::Sealed {}

/// single-class marker (ADR-0112): the handler replies 0-or-1 through
/// its return value (ADR-0109). The default `M` on both ctx types, so
/// an unmarked `WasmCtx<'_>` / `NativeCtx<'_>` is the single-mode view.
pub struct Single;

/// manual-class marker (ADR-0112): the handler issues its own replies
/// via `OutboundReply` (`reply` / `reply_to`), which is implemented
/// only for this mode.
pub struct Manual;

/// stream-class marker (ADR-0112): a stream of replies over time.
/// Reserved — no emit surface is built yet, and the `#[actor]` macro
/// rejects `#[handler::stream]` until it is.
pub struct Stream;

impl sealed::Sealed for Single {}
impl sealed::Sealed for Manual {}
impl sealed::Sealed for Stream {}

impl ReplyMode for Single {}
impl ReplyMode for Manual {}
impl ReplyMode for Stream {}

#[cfg(test)]
mod tests {
    use super::{Manual, Single, Stream};
    use core::mem::size_of;

    /// The mode markers are zero-sized — the invariant the layout-
    /// identity reborrow in `WasmCtx::as_single` / `NativeCtx::as_single`
    /// rests on (a `PhantomData<M>` field stays a ZST for every `M`).
    #[test]
    fn reply_mode_types_are_zsts() {
        assert_eq!(size_of::<Single>(), 0);
        assert_eq!(size_of::<Manual>(), 0);
        assert_eq!(size_of::<Stream>(), 0);
    }
}
