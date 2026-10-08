//! [`close`] — the one close sequence every exit of a native actor runs
//! (ADR-0247 rule 5: what wired, unwires).
//!
//! An actor leaves the engine by one of four exits: it asked to close
//! (`ctx.shutdown()`), the engine is tearing down, its birth was cancelled
//! after `wire`, or its boot rolled back. Each exit hands [`close`] the actor
//! and the source of its residual mail, and nothing that selects behaviour.
//! The sequence has no variant:
//!
//! 1. drain the residual inbox through [`dispatch_envelope`];
//! 2. run `A::unwire` under the actor's stamped slots;
//! 3. drop the mailbox's cost rows;
//! 4. discard outbound mail still held for an activation that never landed;
//! 5. settle the held-reply ledger;
//! 6. end the name in the registries;
//! 7. drop the actor.
//!
//! The two places exits differ are facts the engine already records, read
//! here and chosen by no caller:
//!
//! - **Is the engine tearing down?** The binding's teardown bit
//!   ([`NativeBinding::is_engine_teardown`]). It selects silent settlement of
//!   held replies over an `unanswered` answer (ADR-0243 §1) and nothing else.
//! - **Was the name ever published?** The route registry's record for the id.
//!   A `Starting` reservation belongs to the birth that holds its token, which
//!   rolls both registries back itself, so the close writes nothing for it
//!   and the name stays free. Any other record is a published name: the close
//!   tombstones it, retires its route, releases the parent key, and notifies
//!   its watchers.
//!
//! The actor is taken by value, so it cannot be closed twice and cannot be
//! dropped around the sequence by a caller that holds it.

use std::sync::Arc;

use aether_actor::local::ActorSlots;

use super::dispatcher::dispatch_envelope;
use crate::actor::monitor::{notify_alias_departures, notify_departure};
use crate::actor::native::NativeActor;
use crate::actor::native::binding::NativeBinding;
use crate::actor::native::ctx::NativeCtx;
use crate::actor::native::envelope::Envelope;
use crate::actor::native::local;
use crate::actor::registry::ActorRegistry;
use crate::mail::registry::effect::{EffectBatch, RegistryEffect};
use crate::mail::{KindId, Source};
use crate::runtime::effect_chain::{EffectChain, Uncaused};

/// Close `actor`: run the whole sequence in the [module docs](self) on the
/// calling thread, which is the actor's execution home (a pool worker for a
/// pooled slot, the pump thread for a pumped one, the boot thread for a root
/// that never reached its dispatcher).
///
/// `residual` yields the mail still queued for the actor, in arrival order,
/// until it answers `None`. Each envelope is dispatched exactly as a live
/// drain would dispatch it, so its chain settles through the handler it was
/// sent to.
///
/// `unwire` runs with a mail-allowed ctx. What it sends leaves at the ctx's
/// drop unless the actor never went `Live`, in which case the activation
/// hold still stands and step 4 discards it with what `wire` sent.
pub fn close<A>(
    mut actor: Box<A::State>,
    binding: &Arc<NativeBinding>,
    slots: &ActorSlots,
    actor_registry: &ActorRegistry,
    mut residual: impl FnMut() -> Option<Envelope>,
) where
    A: NativeActor,
{
    while let Some(env) = residual() {
        dispatch_envelope::<A>(&mut actor, binding, slots, env);
    }

    local::with_stamped(slots, || {
        let mut close_ctx = NativeCtx::new_for_actor(binding, Source::NONE, None, None);
        A::unwire(actor.as_mut(), &mut close_ctx);
    });

    // iamacoffeepot/aether#3051: `unwire` is the last phase allowed to
    // observe this actor's handler costs, so its global rows go now and
    // native instance churn cannot retain stale cells.
    binding.mailer().cost_table().drop_mailbox(binding.self_mailbox());
    binding.discard_outbound_after_activation();
    finalize_close_and_fan_out(actor_registry, binding, EffectChain::Uncaused(Uncaused::CloseTail));

    drop(actor);
}

/// The close tail: settle the held-reply ledger, then end the name.
///
/// Each held reply still in the binding's ledger is answered with its
/// `R::unanswered()` before its hold releases, or, when the actor closes as
/// part of engine teardown, settled silently (ADR-0243 §1). Both run before
/// the actor's state drops, so a `Held` parked in that state finds its entry
/// gone and drops silently.
///
/// A name that was never published has no registry tail: its route still
/// reads `Starting`, the reservation of a birth that was cancelled after
/// `wire`, and that birth rolls back the reservation, the actor registry's
/// entry, and the parent's staged key itself.
///
/// For a published name the tail drains `monitors_of[self_id]`, prunes
/// `monitoring[id]` from each target, marks the slot `Dead`, retires the
/// route to `Dropped`, releases this actor's parent-local live child key,
/// and fans one [`MonitorNotice`](aether_kinds::MonitorNotice) out to every
/// watcher through the binding's mailer.
///
/// The actor registry's tombstone is the synchronous authority spawn
/// admission and `register_monitor` read: a monitor registered after it is
/// written is not entered in the index, and its caller posts the notice
/// instead (see [`ActorRegistry`]). The route's `Dropped` record is
/// its published mirror for route readers (`resolve_live` refuses it and the
/// live inventory drops it), staged through the ADR-0165 owner, so it lands
/// at the owner's next apply. The route keeps its proven name, so a held
/// reference still names its path, and the name is never registered again
/// (ADR-0079 §7). An owner that closed first is chassis shutdown, and the
/// logged sink stays quiet about it.
///
/// The closing actor's inline-child aliases (ADR-0114 §2) depart with it, so
/// each of those addresses fans out under its own name too (see
/// `notify_alias_departures`). Each alias closes with it and tombstones
/// (ADR-0241 §8), so a later watch on it is answered with its notice at once
/// and its key is never spawned again; only `self_id` goes `Dead`, because an alias is served by this slot
/// rather than owning one. An alias resolves through its target, so its route
/// reads `Dropped` with it.
///
/// The key release sits between the registry close and the fan-out on
/// purpose. A watcher that re-stages the dead child's subname the moment its
/// notice lands then finds the key already free and the id already
/// tombstoned, so owner-time activation answers `SubnameRetired`, the
/// authoritative reason (ADR-0165), rather than a stale parent-local
/// `SubnameInUse`.
///
/// `chain` is the ADR-0168 §3 declaration. The tail runs past the closing
/// chain's `Finished`, so the only honest answer is [`Uncaused::CloseTail`]:
/// the registry close and the `MonitorNotice` fan-out are outside settlement,
/// and no consumer can wait for either. The notices are pushed straight
/// through the mailer, so each is on its watcher's inbox, or settled as
/// discarded, before this returns.
fn finalize_close_and_fan_out(actor_registry: &ActorRegistry, binding: &NativeBinding, chain: EffectChain) {
    debug_assert!(chain.held_root().is_none(), "the close tail runs past its chain's Finished, so it can hold nothing");
    if binding.is_engine_teardown() {
        binding.settle_held_for_engine_teardown();
    } else {
        binding.answer_held_for_actor_close();
    }

    let self_id = binding.self_mailbox();
    let registry = binding.mailer().registry();
    // `route_lookup` ignores its kind on this path, as `is_live_at` relies
    // on; the zero kind carries that.
    if registry.route_lookup(KindId(0), self_id).is_starting() {
        return;
    }

    let watchers = actor_registry.close_actor(self_id);
    registry.submit_logged(EffectBatch::new(vec![RegistryEffect::DropMailbox(self_id)]));
    binding.release_parent_child_reservation();
    notify_departure(binding, self_id, watchers);
    notify_alias_departures(actor_registry, binding, self_id);
}
