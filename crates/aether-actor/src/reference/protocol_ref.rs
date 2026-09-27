//! [`ProtocolRef`]: proof that an actor covering a protocol reached `Live` at
//! an id.

use core::any::type_name;
use core::fmt;
use core::marker::PhantomData;

use aether_data::MailboxId;

use super::ErasedActorRef;

/// Proof that an actor whose published rows cover the protocol `P` reached
/// `Live` at an id, in this engine session (ADR-0231 §3).
///
/// The proven target plus a phantom protocol. A send through it compiles only
/// for a kind `P` lists (see [`Target`](crate::Target)), whatever else the
/// target handles, and costs what a send through an
/// [`ActorRef`](crate::ActorRef) costs: the check is the compiler's.
///
/// Its door is the native `ctx.resolve` of a
/// [`ProtocolPath<P>`](crate::ProtocolPath), which checks the route's
/// published rows once, at receipt. Like every proven reference it is
/// memory-only, with no codec of any kind: what crosses a boundary is the
/// protocol path, and the receiver proves it again on its own side.
///
/// ```compile_fail,E0277
/// # use aether_actor::ProtocolRef;
/// #
/// fn assert_serialize<T: serde::Serialize>() {}
///
/// assert_serialize::<ProtocolRef<()>>();
/// ```
///
/// ```compile_fail,E0277
/// # use aether_actor::ProtocolRef;
/// #
/// fn assert_schema<T: aether_data::Schema>() {}
///
/// assert_schema::<ProtocolRef<()>>();
/// ```
///
/// ```compile_fail,E0277
/// # use aether_actor::ProtocolRef;
/// #
/// fn assert_wire_encode<T: aether_data::wire::WireEncode>() {}
///
/// assert_wire_encode::<ProtocolRef<()>>();
/// ```
pub struct ProtocolRef<P> {
    target: ErasedActorRef,
    _protocol: PhantomData<fn() -> P>,
}

impl<P> ProtocolRef<P> {
    /// Mint a reference for a confirmed-`Live` id whose published rows cover
    /// `P`. Native code goes through the gated mint.
    pub(crate) const fn new(id: MailboxId) -> Self {
        Self { target: ErasedActorRef::new(id), _protocol: PhantomData }
    }

    /// The proof this reference sends through, for [`Target`](crate::Target).
    pub(crate) const fn target(self) -> ErasedActorRef {
        self.target
    }
}

impl<P> Clone for ProtocolRef<P> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P> Copy for ProtocolRef<P> {}

/// A reference prints its protocol type, never its position.
impl<P> fmt::Debug for ProtocolRef<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProtocolRef<{}>", type_name::<P>())
    }
}
