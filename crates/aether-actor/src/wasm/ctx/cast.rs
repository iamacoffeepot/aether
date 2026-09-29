//! Typing a reference this actor already holds as a protocol —
//! [`WasmCtx::cast`], the guest twin of the native `NativeCtx::cast`
//! (ADR-0231 §4's guard cast).

use super::WasmCtx;
use crate::model::CastTarget;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::reference::{ErasedActorRef, ProtocolRef};
use crate::wasm::bridge::address;

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// Type an erased reference this actor already holds as the protocol `T`
    /// (ADR-0231 §4's guard cast): `Some` when the reference's route is
    /// `Live` and the rows it published answer `T`, `None` otherwise.
    ///
    /// The guest twin of the native `NativeCtx::cast`: one host call reads
    /// the rows the route published from the same row source the native cast
    /// reads, and this verb applies the same sealed [`CastTarget::admits`]
    /// rule to them, so the two answers cannot drift apart. `T` is sealed to
    /// two arms: [`Subscriber<K>`](crate::Subscriber) admits a silent or
    /// manual row for `K`, and a `#[protocol]` type admits a route that
    /// publishes every one of its rows with the exact reply.
    ///
    /// The reference usually arrived untyped, as [`Self::sender`] does, and
    /// the cast is how a handler that must send through it later gets a
    /// typed proof to keep. It costs one host call and one published-view
    /// read, so run it once, when the reference is armed, and keep the
    /// result; never cast at a send.
    ///
    /// Its first consumers are the HTTP stream handles (ADR-0133): a
    /// streaming handler casts the dispatching sender to the sink protocol
    /// its handle emits.
    #[must_use]
    pub fn cast<T: CastTarget>(&self, reference: ErasedActorRef) -> Option<ProtocolRef<T>> {
        let position = reference.id();
        let rows = address::published_rows(position.0).rows?;

        T::admits(&rows).then(|| ProtocolRef::new(position))
    }
}
