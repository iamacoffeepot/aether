//! Publisher surface + subscriber table + fan-out for the lifecycle cap.
//! Holds the `Publishes` / `Publisher` impls the flat
//! `ctx.subscribe::<LifecycleCapability, K>()` verb resolves through
//! (always-on, both transports), and the native [`StageSubscribers`] table
//! whose [`broadcast_to_subscribers`] the receive side calls once per advance.
//! The table holds each subscriber as a `ProtocolRef<Subscriber<K>>` for its
//! stage `K` (ADR-0231 §8), so each broadcast is a typed send of that stage.

use aether_actor::{Publisher, Publishes};
use aether_data::Kind;
use aether_kinds::{InitCaps, InitComponents, Present, Render, Shutdown, Tick};

use super::LifecycleCapability;
use crate::kinds::{LifecycleSubscribeSelf, LifecycleUnsubscribeSelf};

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use self::table::{StageSubscribers, broadcast_to_subscribers};

/// Writes one `Publishes` impl per published stage — the compile-time gate on
/// the flat `ctx.subscribe::<LifecycleCapability, K>()` verb. The marker
/// states what the cap can ever emit; the runtime still fail-fasts on a stage
/// *this* chassis's graph omits (ADR-0082 §7), so the reply states what it
/// does emit here.
macro_rules! publishes {
    ($($stage:ident $field:ident),+ $(,)?) => {
        $(impl Publishes<$stage> for LifecycleCapability {})+
    };
}

published_stages!(publishes);

/// The flat subscribe verbs send these self-addressed stage requests; the cap
/// resolves the subscriber from the inbound's host-stamped `Source`
/// (ADR-0083).
impl Publisher for LifecycleCapability {
    type Subscribe = LifecycleSubscribeSelf;
    type Unsubscribe = LifecycleUnsubscribeSelf;

    fn subscribe_request<K: Kind>() -> LifecycleSubscribeSelf
    where
        Self: Publishes<K>,
    {
        LifecycleSubscribeSelf { stage: K::ID.0 }
    }

    fn unsubscribe_request<K: Kind>() -> LifecycleUnsubscribeSelf
    where
        Self: Publishes<K>,
    {
        LifecycleUnsubscribeSelf { stage: K::ID.0 }
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
mod table {
    use std::collections::BTreeMap;

    use aether_actor::{ErasedActorRef, ProtocolRef, ReplyMode, ResolveError, Subscriber};
    use aether_data::{Kind, KindId};
    use aether_kinds::{InitCaps, InitComponents, Present, Render, Shutdown, Tick};
    use aether_substrate::actor::native::NativeCtx;

    use crate::kinds::LifecycleSubscription;

    /// The broadcast value of `$stage`: `Tick` is the tick the capability
    /// built for this advance, carrying its elapsed time and the running
    /// total (issue 4470, issue 7507), and every other stage is its empty
    /// signal.
    macro_rules! stage_signal {
        (Tick, $tick:ident) => {
            $tick
        };
        ($stage:ident, $tick:ident) => {
            <$stage as Default>::default()
        };
    }

    /// Writes [`StageSubscribers`] and [`broadcast_to_subscribers`] from the
    /// stage list: one typed set per stage, and each stage-id dispatch as one
    /// arm per stage.
    macro_rules! stage_subscribers {
        ($($stage:ident $field:ident),+ $(,)?) => {
            /// The subscriber table (ADR-0082 §7): one set per published
            /// stage, holding each subscriber as the
            /// `ProtocolRef<Subscriber<K>>` its broadcast is sent through
            /// (ADR-0231 §8), keyed by its erased twin. The key is what a
            /// removal, a `MonitorNotice` purge, and a log line compare and
            /// name; the value is only ever sent through.
            #[derive(Default)]
            pub struct StageSubscribers {
                $($field: BTreeMap<ErasedActorRef, ProtocolRef<Subscriber<$stage>>>,)+
            }

            impl StageSubscribers {
                /// The stage an explicit subscription names.
                pub fn stage_of(subscription: &LifecycleSubscription) -> KindId {
                    match subscription {
                        $(LifecycleSubscription::$stage(_) => <$stage as Kind>::ID,)+
                    }
                }

                /// Prove an explicit subscription's path live (ADR-0231 §3:
                /// its decode already proved the path covers the stage) and
                /// hold the proof, answering its key for the caller to watch.
                pub fn subscribe<A, S, M: ReplyMode>(
                    &mut self,
                    ctx: &NativeCtx<'_, A, S, M>,
                    subscription: &LifecycleSubscription,
                ) -> Result<ErasedActorRef, ResolveError> {
                    match subscription {
                        $(LifecycleSubscription::$stage(path) => {
                            let subscriber = ctx.resolve(path)?;
                            self.$field.insert(subscriber.erase(), subscriber);
                            Ok(subscriber.erase())
                        })+
                    }
                }

                /// Remove an explicit subscription's subscriber by key. A path
                /// with no live actor holds nothing to remove: a closed
                /// subscriber already left every stage through its
                /// `MonitorNotice`.
                pub fn unsubscribe<A, S, M: ReplyMode>(
                    &mut self,
                    ctx: &NativeCtx<'_, A, S, M>,
                    subscription: &LifecycleSubscription,
                ) {
                    match subscription {
                        $(LifecycleSubscription::$stage(path) => {
                            if let Ok(subscriber) = ctx.resolve(path) {
                                self.$field.remove(&subscriber.erase());
                            }
                        })+
                    }
                }

                /// Type `sender` as a subscriber to `stage` with the guard cast
                /// and hold it. `false` when `stage` is not a published stage
                /// or the sender's published rows do not handle it silently or
                /// manually, and nothing is held.
                pub fn subscribe_sender<A, S, M: ReplyMode>(
                    &mut self,
                    ctx: &NativeCtx<'_, A, S, M>,
                    stage: KindId,
                    sender: ErasedActorRef,
                ) -> bool {
                    $(if stage == <$stage as Kind>::ID {
                        let Some(subscriber) = ctx.cast::<Subscriber<$stage>>(sender) else {
                            return false;
                        };
                        self.$field.insert(sender, subscriber);
                        return true;
                    })+
                    false
                }

                /// Remove `sender` from `stage`'s set by key.
                pub fn unsubscribe_sender(&mut self, stage: KindId, sender: ErasedActorRef) {
                    $(if stage == <$stage as Kind>::ID {
                        self.$field.remove(&sender);
                    })+
                }

                /// Remove `departed` from every stage's set.
                pub fn purge(&mut self, departed: ErasedActorRef) {
                    $(self.$field.remove(&departed);)+
                }

                /// The keys of `stage`'s set, for naming its subscribers.
                pub fn subscribers_of(&self, stage: KindId) -> Vec<ErasedActorRef> {
                    $(if stage == <$stage as Kind>::ID {
                        return self.$field.keys().copied().collect();
                    })+
                    Vec::new()
                }
            }

            /// Send `stage`'s signal to each of its subscribers, with a typed
            /// fan-out through the references its set holds. The fan-out
            /// preserves the inbound `(parent, root)` lineage, so settlement
            /// counts each child against the advance's root (ADR-0080 §6).
            /// `tick` is what the `Tick` stage broadcasts; every other stage
            /// ignores it.
            pub fn broadcast_to_subscribers<A, S, M: ReplyMode>(
                ctx: &mut NativeCtx<'_, A, S, M>,
                subscribers: &StageSubscribers,
                stage: KindId,
                tick: Tick,
            ) {
                $(if stage == <$stage as Kind>::ID {
                    ctx.fanout(subscribers.$field.values(), &stage_signal!($stage, tick));
                })+
            }
        };
    }

    published_stages!(stage_subscribers);
}
