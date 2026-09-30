//! The hidden selector the `#[actor]` dispatch arms answer a refused typed
//! path through (ADR-0231 §3).
//!
//! An arm writes `<Refusal<{ <K as Kind>::PROVES_ROUTES }> as
//! RefusalAnswer<O>>::answer` with its row's concrete request kind `K` and
//! reply `O`. A kind that carries no `ProtocolPath` selects `Refusal<false>`,
//! which accepts any reply and answers nothing, so the arm drops a refused
//! decode as before. A kind that carries one selects `Refusal<true>`, which
//! is implemented only for a reply that is `From<PathRefused>`: the arm then
//! answers the refusal, and a reply that cannot say it fails to compile.

use aether_data::wire::Error as WireError;

use crate::PathRefused;

/// The selector over a request kind's path marker.
pub struct Refusal<const PROVES_ROUTES: bool>;

/// How a dispatch arm answers a request whose typed path did not prove.
#[diagnostic::on_unimplemented(
    message = "`{O}` cannot answer a request whose typed path failed to prove",
    label = "this reply cannot name a refused path",
    note = "a request carrying a ProtocolPath replies with a kind that implements `From<PathRefused>`, \
            its `Err` arm naming the refusal (ADR-0231 §3)"
)]
pub trait RefusalAnswer<O> {
    /// The reply for `refused`, or `None` when the kind carries no path and
    /// the refusal is dropped.
    fn answer(refused: PathRefused) -> Option<O>;
}

impl<O> RefusalAnswer<O> for Refusal<false> {
    fn answer(_refused: PathRefused) -> Option<O> {
        None
    }
}

impl<O: From<PathRefused>> RefusalAnswer<O> for Refusal<true> {
    fn answer(refused: PathRefused) -> Option<O> {
        Some(O::from(refused))
    }
}

/// A guest arm's answer to a refused decode (ADR-0231 §3): the reply
/// `answer` makes of the typed-path refusal `error` carries, or `None` when
/// the decode refused for another reason (a kind or count mismatch arrives as
/// `None`), or the kind carries no path.
#[must_use]
pub fn refused_reply<O>(error: Option<&WireError>, answer: impl FnOnce(PathRefused) -> Option<O>) -> Option<O> {
    error.and_then(PathRefused::from_wire).and_then(answer)
}
