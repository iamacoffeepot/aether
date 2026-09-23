//! The receive ctx's flat subscribe verbs (ADR-0232 §3) — `subscribe` and
//! `unsubscribe`, each naming its publisher.

use aether_data::Kind;

use super::WasmCtx;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::model::{CallerAddressable, Contract, DependencyResolver, DependsOn, Publishes, SilentRow, Singleton};

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// Subscribe this actor to kind `K` from publisher `P` (ADR-0232 §3),
    /// inheriting the handler's causal chain like [`Self::send`]. A window
    /// subscribe covers every window.
    ///
    /// Compiles only on a ctx typed by an actor that declares `P` with
    /// `#[actor(depends(P))]`, only when `P` publishes `K`, and only when the
    /// actor's handler for `K` is silent or manual (ADR-0231 §8): a published
    /// event has no one waiting for a reply. The erased ctx has no subscribe
    /// verb. The body is a flat send of the request `P` builds
    /// ([`Publisher::subscribe_request`](crate::Publisher::subscribe_request)).
    ///
    /// Its consumers are the puppet motors' `wire` (`aether.puppet-idle`,
    /// `aether.puppet-turntable`) and the bundle probe fixture's `wire`.
    pub fn subscribe<P, K: Kind>(&mut self)
    where
        P: Publishes<K> + Singleton + CallerAddressable,
        P::Resolver: DependencyResolver,
        A: DependsOn<P> + Contract<K>,
        <A as Contract<K>>::Reply: SilentRow,
    {
        self.send::<P>(&P::subscribe_request::<K>());
    }

    /// Unsubscribe this actor from kind `K` at publisher `P`, the twin of
    /// [`Self::subscribe`]. It carries no handler check: removing a row
    /// cannot make the publisher send anything.
    ///
    /// Its consumer is the bundle probe fixture's `on_unsubscribe_keys`.
    pub fn unsubscribe<P, K: Kind>(&mut self)
    where
        P: Publishes<K> + Singleton + CallerAddressable,
        P::Resolver: DependencyResolver,
        A: DependsOn<P>,
    {
        self.send::<P>(&P::unsubscribe_request::<K>());
    }
}
