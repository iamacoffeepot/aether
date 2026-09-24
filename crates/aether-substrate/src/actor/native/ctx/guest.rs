//! What a guest host declares, and the two verbs that declaration unlocks.
//!
//! A guest host is a native actor whose receive surface is a guest's rather
//! than its own `#[handler]` set: the wasm trampoline, whose accept set is the
//! surface of whichever module it hosts. The actor declares that surface by
//! implementing [`GuestHost`], and the substrate reads the declaration: the
//! actor asks for its record to be brought in line, with no capabilities in
//! hand, and never learns its own mailbox position (ADR-0230).

use aether_actor::ReplyMode;
use aether_data::{ActorPath, KindId};
use aether_kinds::ComponentCapabilities;

use crate::actor::native::{Dispatch, NativeActor};

use super::NativeCtx;

/// A native actor that runs a guest whose receive surface is not its own
/// `#[handler]` set. Implementing it is the declaration: only a type that
/// implements it can reach [`NativeCtx::sync_guest`] or [`NativeCtx::path`].
///
/// Its consumer is `WasmTrampoline`, whose guest is the wasm module it hosts.
pub trait GuestHost: NativeActor {
    /// The receive surface of the guest `state` hosts right now: `Some` while
    /// a guest is resident, `None` for an empty slot.
    fn guest(state: &Self::State) -> Option<&ComponentCapabilities>;
}

impl<M: ReplyMode, A: GuestHost> NativeCtx<'_, A, M> {
    /// Make this actor's accept set and cost rows match what `A` declares for
    /// `state`. `Some`: the accept set becomes exactly the guest's, and cost
    /// cells are seeded, reusing existing ones, for `A`'s measured framework
    /// arms plus every guest handler. `None`: the actor accepts nothing, its
    /// cost rows are dropped, and the framework arms are re-seeded.
    /// Idempotent.
    ///
    /// The framework arms ride along in both arms (iamacoffeepot/aether#4269):
    /// the actor goes on dispatching its own handlers whatever it hosts, so
    /// they stay measured across a replace and a drop.
    ///
    /// Its consumer is `WasmTrampoline`: its `wire` hook (load, module boot,
    /// sibling spawn), its replace handler, and its unload.
    pub fn sync_guest(&self, state: &A::State) {
        let mut measured = <A as Dispatch<A::State>>::measured_kinds();
        match A::guest(state) {
            Some(guest) => {
                let added: Vec<KindId> =
                    guest.handlers.iter().map(|h| h.id).filter(|id| !measured.contains(id)).collect();
                measured.extend(added);
                self.binding.host_guest(guest, &measured);
            }
            None => self.binding.release_guest(&measured),
        }
    }

    /// This guest host's canonical path, as text, for naming it in a log line
    /// or a refusal. Read from the binding's typed identity: no registry read,
    /// no position.
    ///
    /// Its consumer is `WasmTrampoline`: the no-guest warning and trap abort
    /// in its fallback, and its replacement dependency refusal.
    ///
    /// # Panics
    ///
    /// When the binding is untyped. Every production birth builds a typed
    /// binding, so this is a broken invariant, not an answer (ADR-0063).
    #[must_use]
    pub fn path(&self) -> ActorPath {
        self.binding
            .runtime_identity()
            .expect("NativeCtx::path requires a typed production binding")
            .canonical_name()
            .clone()
    }
}
