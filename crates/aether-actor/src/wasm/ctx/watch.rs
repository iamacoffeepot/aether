//! Watching another actor (ADR-0079 §8) — [`WasmCtx::watch`] and
//! [`WasmCtx::unwatch`], the [`Departed`] event a departure handler takes,
//! and the two hidden calls the `#[actor]` departure arm makes.
//!
//! A watch is a registration the host keeps for this actor's mailbox and a
//! context this SDK stores under the watch's id, as a pending request is a
//! correlation and a context. The handler is handed the context, so it never
//! looks the departed actor up in a table keyed by a reference.

use core::fmt;

use aether_data::{Kind, WatchId};

use super::WasmCtx;
use crate::model::ctx::reply_mode::ReplyMode;
use crate::model::{WatchTarget, Watchable, WatchedRef, Watches};
use crate::wasm::bridge::watch;
use crate::wasm::inline::TakenWatchContext;

/// A watched actor departed (ADR-0079 §8): the event a component's
/// departure handler takes for the watched type `W`.
///
/// ```ignore
/// #[handler::event]
/// fn on_provider_gone(&mut self, _ctx: &mut WasmCtx<'_>, event: Departed<Provider>, note: ProviderNote) {
///     self.rows.remove(&event.watch);
/// }
/// ```
///
/// One handler serves each watched type, and its fourth parameter is the
/// context kind every `ctx.watch` of that type passes; a handler that leaves
/// it out is watched for with [`NoContext`](crate::NoContext). The handler
/// runs once per watch, when the target closes by any exit, and the watch
/// ends there.
///
/// The dispatch arm builds it from the engine's departure notice, which has
/// no fields and which no author names. It is never sent as mail.
pub struct Departed<W: Watchable> {
    /// The departed actor, as the typed reference it was watched through.
    /// It proves the actor reached `Live` as that type, never that it is
    /// live now (ADR-0230 §1): it has closed. Use it as an identity, an
    /// in-memory table key, or a name in a log line. A send through it
    /// compiles and is the ordinary send to a closed actor.
    pub actor: W::Ref,
    /// The watch that ended: the id `ctx.watch` returned for this target.
    /// It names a row in this actor's own watch table and means something
    /// only to this actor. It has actor reach, so it is never mailed; it may
    /// sit in this actor's saved state, where [`Self::actor`] may not.
    pub watch: WatchId,
}

impl<W: Watchable> Clone for Departed<W> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<W: Watchable> Copy for Departed<W> {}

impl<W: Watchable> fmt::Debug for Departed<W>
where
    W::Ref: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Departed").field("actor", &self.actor).field("watch", &self.watch).finish()
    }
}

impl<A, S, M: ReplyMode> WasmCtx<'_, A, S, M> {
    /// Watch `target` (ADR-0079 §8): when it closes, this actor's departure
    /// handler for the type it was watched through runs once and is handed
    /// the departed actor's reference, the returned id, and `context`.
    ///
    /// `target` is a typed reference this actor holds: an
    /// [`ActorRef<R>`](crate::ActorRef), or a
    /// [`ProtocolRef<P>`](crate::ProtocolRef) such as a sender cast at
    /// receipt with [`Self::cast`]. An
    /// [`ErasedActorRef`](crate::ErasedActorRef) is cast first. The handler
    /// is the actor's `#[handler::event]` over
    /// [`Departed<T::Watched>`](Departed), and `context` is the kind that
    /// handler takes as its fourth parameter, or
    /// [`NoContext`](crate::NoContext) when it takes none. A watch through a
    /// type no handler names does not compile, and neither does one whose
    /// context is another kind.
    ///
    /// - **It never fails.** Every typed reference returns an id.
    /// - **The notice is mail.** It arrives after the handler that is
    ///   running returns, so state this handler keys on the returned id is
    ///   in place when it does.
    /// - **A target that has already closed** is noticed the same way. A
    ///   reference proves its actor reached `Live`, never that it is live
    ///   now (ADR-0230 §1).
    /// - **A watch is unique per target and watched type.** A second call
    ///   for a target already watched through the same type makes no second
    ///   watch: it returns the same id and replaces the stored context with
    ///   the one passed. So the id is this actor's own stable name for the
    ///   target, and a component that watches a sender on every mail from it
    ///   keys its own table by the id. One actor watched through two types, an
    ///   `ActorRef<R>` and a `ProtocolRef<P>` or two protocols, is two
    ///   watches with two ids, and its departure runs each handler once.
    /// - **Every watch of one type shares one context kind**, the one its
    ///   handler takes. Different notes about actors of one type are one
    ///   enum kind.
    /// - **A watch ends at its notice**, as a request ends at its reply, or
    ///   at [`Self::unwatch`].
    /// - **A watch stands across a republish of this actor.** The host moves
    ///   the registration with the mailbox and the context rides the saved
    ///   state, whether or not this actor overrides `on_dehydrate`, so the
    ///   successor's handler runs with it. A `wire` that watches again after
    ///   an aborted republish gets the standing id.
    /// - **The host releases every watch** when this actor closes, traps, or
    ///   is dropped as a republish candidate, with no code of its own run.
    ///
    /// The id names a row in this actor's own watch table and means
    /// something only to this actor: the same number held by another actor
    /// names one of that actor's watches. It has actor reach (ADR-0242), so
    /// a kind holding one is never mail, and no component builds one from a
    /// number. It may be kept in this actor's saved state and in a context
    /// this actor stores with a request. The reference in the event may not:
    /// a reference has no codec.
    pub fn watch<T: WatchTarget>(&mut self, target: T, context: <A as Watches<T::Watched>>::Context) -> WatchId
    where
        A: Watches<T::Watched>,
    {
        let watch = watch::watch(target.position().0, self.mailbox, T::watched_tag());
        self.inline.store_watch_context(watch, context);
        watch
    }

    /// End the watch `watch` before its target departs: the target's close
    /// then sends this actor nothing for it, and its stored context is
    /// dropped. An id that is no live watch of this actor does nothing. The
    /// id is one this actor's own `watch` returned or its own departure event
    /// carried: an id has actor reach, so none arrives in mail.
    ///
    /// A notice already posted when this runs still arrives, finds no watch,
    /// and runs no handler. Watching the same target again afterwards is a
    /// new watch with a new id, since ids are never reused; when the target
    /// has already closed, that watch is noticed at once, and whichever
    /// notice arrives first ends it, so the handler runs once.
    pub fn unwatch(&mut self, watch: WatchId) {
        if watch::unwatch(watch) {
            self.inline.discard_watch_context(watch);
        }
    }

    /// Ask the host which watch through `W` the departure notice being
    /// dispatched ends, and build its event. `None` when the dispatch is not
    /// a host dispatch with a sender, or no such watch stands: it was
    /// released, or an earlier notice already ended it.
    ///
    /// Not part of the public API; the `#[actor]` departure arm calls it once
    /// per watched type the actor has a handler for.
    #[doc(hidden)]
    pub fn __ended_watch<W: Watchable>(&mut self) -> Option<Departed<W>> {
        if !self.host_dispatch {
            return None;
        }
        let departed = self.source?.id();
        let watch = watch::watch_ended(departed.0, self.mailbox, <W::Ref as WatchedRef>::watched_tag())?;

        Some(Departed { actor: <W::Ref as WatchedRef>::departed_at(departed), watch })
    }

    /// Take the context stored for the ended watch `watch` as the kind `C`
    /// its handler takes. `None`, after an error in this actor's own log
    /// ring, when the context was stored as another kind (a predecessor
    /// module's handler for this watched type took that kind) or none is
    /// stored; the stored context is discarded either way, since the watch
    /// has ended.
    ///
    /// Not part of the public API; the `#[actor]` departure arm calls it
    /// before the handler named `handler`.
    #[doc(hidden)]
    pub fn __take_watch_context<C: Kind>(&mut self, watch: WatchId, handler: &'static str) -> Option<C> {
        match self.inline.take_watch_context::<C>(watch) {
            TakenWatchContext::Stored(context) => Some(context),
            TakenWatchContext::Missing => {
                tracing::error!(
                    handler,
                    %watch,
                    context = C::NAME,
                    "departure handler did not run: its watch has no stored context",
                );
                None
            }
            TakenWatchContext::OtherKind(stored) => {
                tracing::error!(
                    handler,
                    %watch,
                    context = C::NAME,
                    stored = stored.0,
                    "departure handler did not run: its watch's context was stored as another kind and is discarded",
                );
                None
            }
        }
    }
}
