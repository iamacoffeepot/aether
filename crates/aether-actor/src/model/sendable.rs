//! [`SendableTo`]: the payload bound of the flat typed send verbs
//! (ADR-0232 §2).

use aether_data::Kind;

use super::HandlesKind;

mod sealed {
    use aether_data::Kind;

    use crate::model::HandlesKind;

    /// The seal: only the blanket impl below makes a kind sendable, so no
    /// crate outside `aether-actor` can mark a kind sendable to an actor that
    /// does not handle it.
    pub trait Sealed<R> {}

    impl<K: Kind, R: HandlesKind<K>> Sealed<R> for K {}
}

/// A kind that may be mailed to actor `R`: `K: SendableTo<R>` holds exactly
/// when `R: HandlesKind<K>`.
///
/// The flat typed verbs (`ctx.send::<R>(&kind)` and its siblings) take the
/// payload as `&impl SendableTo<R>`, so the kind is inferred from the argument
/// and the turbofish names only the recipient. The check lives on the payload
/// because the recipient's own markers ([`HandlesKind`],
/// [`Contract`](super::Contract)) are implemented on the target, not on the
/// kind, and Rust has no partial turbofish that could leave `K` to inference.
///
/// Sealed: the one blanket impl is the only way in, so the bound refuses a
/// kind the recipient has no handler for rather than letting it warn-drop at
/// run time.
#[diagnostic::on_unimplemented(
    message = "`{R}` has no handler for `{Self}`",
    label = "`{R}` does not handle `{Self}`",
    note = "a `#[fallback]` does not count as handling a kind"
)]
pub trait SendableTo<R>: Kind + sealed::Sealed<R> {}

impl<K: Kind, R: HandlesKind<K>> SendableTo<R> for K {}
