//! Writing typed paths for declared links (ADR-0230 §2): [`WasmCtx::link`]
//! and [`WasmCtx::link_child`], the guest verbs `#[actor(links(R))]` opens.

use aether_data::{ActorPathError, LoadName};

use super::WasmCtx;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::path::ActorPath;
use crate::{Addressable, ChildOf, Instanced, LinksTo, Root};

impl<A, M: ReplyMode> WasmCtx<'_, A, M> {
    /// Write the canonical path of the root instance of `R` under `key`,
    /// `R::NAMESPACE:key`: the name the registry gives the instance spawned
    /// under that key. Compiles only for an actor that declares `links(R)`.
    ///
    /// Writing reads no registry and folds nothing, and the path claims
    /// nothing until `resolve` proves it where it is received. Its consumer
    /// is the Bloomery bootstrap, which writes its unit's journal and driver
    /// paths (ADR-0240 D8) and narrows the journal's for `Import.source`
    /// (D7). A [`WireCtx`](super::WireCtx) derefs here.
    #[must_use]
    pub fn link<R: Root + Instanced>(&self, key: &LoadName) -> ActorPath<R>
    where
        A: LinksTo<R>,
    {
        ActorPath::root_instance(key)
    }

    /// Write the canonical path of the instanced `C` under `key` beneath the
    /// actor at `parent`, `<parent>/C::NAMESPACE:key`: the name the registry
    /// gives that child. Compiles only for an actor that declares `links(C)`.
    ///
    /// Like [`Self::link`], it reads no registry, folds nothing, and claims
    /// nothing until `resolve` proves the path. Its consumer is the Bloomery
    /// bootstrap, which writes its unit's member paths beneath the unit's
    /// (ADR-0240 D8).
    ///
    /// # Errors
    ///
    /// [`ActorPathError::Scope`] when the written path would pass the depth
    /// or byte cap; nothing else can refuse it.
    pub fn link_child<P: Addressable, C: ChildOf<P> + Instanced>(
        &self,
        parent: &ActorPath<P>,
        key: &LoadName,
    ) -> Result<ActorPath<C>, ActorPathError>
    where
        A: LinksTo<C>,
    {
        parent.child_instance(key)
    }
}
