//! The selector-aware subscription table every window manager keeps. The
//! subscription *mail surface* over it lives in the sibling `manager` module,
//! alongside the rest of the shared manager surface (ADR-0169).

use std::collections::{BTreeMap, HashMap, HashSet};

use aether_actor::{ErasedActorRef, ProtocolRef, ReplyMode, ResolveError, Subscriber};
use aether_data::{ActorMail, ErasedActorPath, Kind, KindId};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowSize,
};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::NativeCtx;

use crate::{WindowClosed, WindowMenuActivated, WindowOpened, WindowSelector, WindowSubscription};

/// The subscribers of one published kind `K`, each held as the
/// `ProtocolRef<Subscriber<K>>` its events are sent through (ADR-0231 §8) and
/// keyed by its erased twin, which is what a removal and a departure compare.
///
/// `all` and `specific` are stored separately so an all-window subscription
/// naturally includes windows created later.
pub struct KindSubscribers<K> {
    all: BTreeMap<ErasedActorRef, ProtocolRef<Subscriber<K>>>,
    specific: HashMap<ErasedActorPath, BTreeMap<ErasedActorRef, ProtocolRef<Subscriber<K>>>>,
}

impl<K> Default for KindSubscribers<K> {
    fn default() -> Self {
        Self { all: BTreeMap::new(), specific: HashMap::new() }
    }
}

impl<K> KindSubscribers<K> {
    fn insert(&mut self, window: Option<&ErasedActorPath>, subscriber: ProtocolRef<Subscriber<K>>) {
        let set = match window {
            None => &mut self.all,
            Some(window) => self.specific.entry(window.clone()).or_default(),
        };
        set.insert(subscriber.erase(), subscriber);
    }

    /// Remove `key` from the set `window` selects, dropping a window's entry
    /// once it is empty.
    fn remove(&mut self, window: Option<&ErasedActorPath>, key: ErasedActorRef) {
        match window {
            None => {
                self.all.remove(&key);
            }
            Some(window) => {
                if self.specific.get_mut(window).is_some_and(|set| {
                    set.remove(&key);
                    set.is_empty()
                }) {
                    self.specific.remove(window);
                }
            }
        }
    }

    /// The subscribers of events from `window`: the all-window set united
    /// with `window`'s own, one reference per key, so an actor subscribed
    /// through both selectors receives one copy.
    fn recipients(&self, window: &ErasedActorPath) -> Vec<ProtocolRef<Subscriber<K>>> {
        let mut recipients = self.all.clone();
        if let Some(specific) = self.specific.get(window) {
            recipients.extend(specific);
        }
        recipients.into_values().collect()
    }
}

/// A kind the window manager publishes, with its typed set in
/// [`WindowSubscribers`]. Implemented once per published kind from the one
/// list, so a fan-out of any other kind does not compile.
pub trait Published: ActorMail + Sized {
    /// This kind's set.
    fn set(subscribers: &WindowSubscribers) -> &KindSubscribers<Self>;

    /// This kind's set, mutably.
    fn set_mut(subscribers: &mut WindowSubscribers) -> &mut KindSubscribers<Self>;
}

/// One subscriber's monitor and the rows it holds in the typed sets.
#[derive(Default)]
struct Holder {
    monitor: Option<MonitorHandle>,
    rows: HashSet<Row>,
}

/// One row of a subscriber's: the kind whose set holds it, and the window
/// it selects, or every window.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Row {
    kind: KindId,
    window: Option<ErasedActorPath>,
}

impl Row {
    fn new(selector: WindowSelector, kind: KindId) -> Self {
        match selector {
            WindowSelector::All => Self { kind, window: None },
            WindowSelector::One(window) => Self { kind, window: Some(window) },
        }
    }
}

/// Writes [`WindowSubscribers`], its [`Published`] impls, and each kind-id
/// dispatch from the published-kind list: one typed set per kind, and one
/// arm per kind.
macro_rules! window_subscribers {
    ($($kind:ident $field:ident),+ $(,)?) => {
        /// Selector-aware subscriptions for events originating at windows:
        /// one [`KindSubscribers`] per published kind.
        ///
        /// `holders` is the reverse index, keyed by each subscriber's erased
        /// reference: its monitor and the exact rows it holds, so a departure
        /// removes precisely that subscriber's rows without scanning anyone
        /// else's.
        pub struct WindowSubscribers {
            $($field: KindSubscribers<$kind>,)+
            holders: HashMap<ErasedActorRef, Holder>,
        }

        $(impl Published for $kind {
            fn set(subscribers: &WindowSubscribers) -> &KindSubscribers<Self> {
                &subscribers.$field
            }

            fn set_mut(subscribers: &mut WindowSubscribers) -> &mut KindSubscribers<Self> {
                &mut subscribers.$field
            }
        })+

        impl WindowSubscribers {
            pub fn new() -> Self {
                Self { $($field: KindSubscribers::default(),)+ holders: HashMap::new() }
            }

            /// Prove an explicit subscription's path live (ADR-0231 §3: its
            /// decode already proved the path handles the kind silently) and
            /// hold the proof under `selector`.
            pub fn subscribe_path<A, M: ReplyMode>(
                &mut self,
                ctx: &mut NativeCtx<'_, A, M>,
                selector: WindowSelector,
                subscription: &WindowSubscription,
            ) -> Result<(), ResolveError> {
                match subscription {
                    $(WindowSubscription::$kind(path) => {
                        let subscriber = ctx.resolve(path)?;
                        self.subscribe(ctx, selector, subscriber);
                    })+
                }
                Ok(())
            }

            /// Prove an explicit subscription's path live and remove its key
            /// from the set `selector` names.
            pub fn unsubscribe_path<A, M: ReplyMode>(
                &mut self,
                ctx: &NativeCtx<'_, A, M>,
                selector: WindowSelector,
                subscription: &WindowSubscription,
            ) -> Result<(), ResolveError> {
                match subscription {
                    $(WindowSubscription::$kind(path) => {
                        self.unsubscribe::<$kind>(selector, ctx.resolve(path)?.erase());
                    })+
                }
                Ok(())
            }

            /// Subscribe the sending actor, typed as a subscriber to `kind`
            /// by the guard cast (ADR-0231 §4).
            ///
            /// # Errors
            ///
            /// The sender is not a local actor; `kind` is not published here;
            /// or the sender's published rows do not handle `kind` silently
            /// or manually, so its events could never be handled.
            pub fn subscribe_self<A, M: ReplyMode>(
                &mut self,
                ctx: &mut NativeCtx<'_, A, M>,
                selector: WindowSelector,
                kind: KindId,
            ) -> Result<(), String> {
                let sender = ctx.sender().ok_or_else(|| {
                    "aether.window.subscribe_self requires a local component sender; an external session or \
                     remote engine must use aether.window.subscribe with an explicit subscriber path"
                        .to_owned()
                })?;
                $(if kind == <$kind as Kind>::ID {
                    let subscriber = ctx.cast::<Subscriber<$kind>>(sender).ok_or_else(|| {
                        format!(
                            "{} has no silent or manual handler for {}, so it cannot subscribe to it",
                            ctx.actor_path(sender),
                            <$kind as Kind>::NAME,
                        )
                    })?;
                    self.subscribe(ctx, selector, subscriber);
                    return Ok(());
                })+
                Err(format!("aether.window does not publish {kind:?}"))
            }

            /// Remove `key` from the set `row` names.
            fn remove_row(&mut self, row: Row, key: ErasedActorRef) {
                $(if row.kind == <$kind as Kind>::ID {
                    self.$field.remove(row.window.as_ref(), key);
                })+
            }

            /// Decode `payload` as the published kind `kind` names and fan it
            /// out to `window`'s subscribers of that kind: the synthetic
            /// runtime's injected events.
            ///
            /// # Errors
            ///
            /// `kind` is not published here, or `payload` does not decode as
            /// it; nothing is sent.
            #[cfg(feature = "synthetic")]
            pub fn publish_encoded<A, M: ReplyMode>(
                &self,
                ctx: &mut NativeCtx<'_, A, M>,
                window: &ErasedActorPath,
                kind: KindId,
                payload: &[u8],
            ) -> Result<(), String> {
                $(if kind == <$kind as Kind>::ID {
                    let event = <$kind as Kind>::decode_from_bytes(payload)
                        .ok_or_else(|| format!("the payload does not decode as {}", <$kind as Kind>::NAME))?;
                    ctx.fanout(self.recipients::<$kind>(window), &event);
                    return Ok(());
                })+
                Err(format!("aether.window does not publish {kind:?}"))
            }
        }
    };
}

published_window_kinds!(window_subscribers);

impl WindowSubscribers {
    /// Hold `subscriber` for `K` under `selector`, and monitor it on its
    /// first row.
    pub fn subscribe<K: Published, A, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, M>,
        selector: WindowSelector,
        subscriber: ProtocolRef<Subscriber<K>>,
    ) {
        let key = subscriber.erase();
        let row = Row::new(selector, K::ID);

        K::set_mut(self).insert(row.window.as_ref(), subscriber);
        self.holders.entry(key).or_default().rows.insert(row);
        self.watch(ctx, key);
    }

    /// Remove `key`'s row for `K` under `selector`.
    pub fn unsubscribe<K: Published>(&mut self, selector: WindowSelector, key: ErasedActorRef) {
        self.remove(Row::new(selector, K::ID), key);
    }

    pub fn unsubscribe_self<A, M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, A, M>,
        selector: WindowSelector,
        kind: KindId,
    ) -> Result<(), String> {
        let subscriber = ctx.sender().ok_or_else(|| {
            "aether.window.unsubscribe_self requires a local component sender; an external session or remote engine \
             must use aether.window.unsubscribe with an explicit subscriber path"
                .to_owned()
        })?;
        self.remove(Row::new(selector, kind), subscriber);
        Ok(())
    }

    /// Drop every subscription `subscriber` holds and release its monitor.
    ///
    /// The caller is the `MonitorNotice` handlers, whose host-stamped sender
    /// is the departed subscriber (ADR-0230). The holder index names exactly
    /// the rows `subscriber` holds, so this removes those and touches
    /// nothing else.
    pub fn unsubscribe_all(&mut self, subscriber: ErasedActorRef) {
        let Some(holder) = self.holders.remove(&subscriber) else {
            return;
        };
        for row in holder.rows {
            self.remove_row(row, subscriber);
        }
    }

    /// The subscribers of `K` events from `window`, one per actor.
    pub fn recipients<K: Published>(&self, window: &ErasedActorPath) -> Vec<ProtocolRef<Subscriber<K>>> {
        K::set(self).recipients(window)
    }

    fn remove(&mut self, row: Row, key: ErasedActorRef) {
        if let Some(holder) = self.holders.get_mut(&key) {
            holder.rows.remove(&row);
        }
        self.remove_row(row, key);
    }

    fn watch<A, M: ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, A, M>, subscriber: ErasedActorRef) {
        let holder = self.holders.entry(subscriber).or_default();
        if holder.monitor.is_none() {
            holder.monitor = ctx.monitor(subscriber).ok();
        }
    }
}

/// The window runtimes' test subscriber: a keyed actor type whose paths
/// narrow to a `Subscriber<K>` of each kind the tests publish, standing as a
/// registered route the tests choose.
#[cfg(test)]
pub(crate) mod fixture {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use aether_actor::{ActorPath, CoveredBy, ErasedActorRef, ProtocolRef, Subscriber};
    use aether_data::{ErasedActorPath, LoadName};
    use aether_kinds::{Key, MouseButton, MouseMove, MouseWheel, WindowSize};
    use aether_substrate::Registry;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::registry::InboxHandler;
    use aether_substrate::testing::registered_ref;

    use super::{Published, WindowSubscribers};
    use crate::WindowSelector;

    /// Silent handlers for every kind a window test subscribes.
    pub(crate) struct Watcher;

    #[aether_actor::actor(instanced, root)]
    impl NativeActor for Watcher {
        const NAMESPACE: &'static str = "test.window.watcher";
        type Config = ();

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[handler::single]
        fn on_key(&mut self, _ctx: &mut NativeCtx<'_>, _key: Key) {
            let _ = self;
        }

        #[handler::single]
        fn on_mouse_move(&mut self, _ctx: &mut NativeCtx<'_>, _mail: MouseMove) {
            let _ = self;
        }

        #[handler::single]
        fn on_mouse_button(&mut self, _ctx: &mut NativeCtx<'_>, _mail: MouseButton) {
            let _ = self;
        }

        #[handler::single]
        fn on_mouse_wheel(&mut self, _ctx: &mut NativeCtx<'_>, _mail: MouseWheel) {
            let _ = self;
        }

        #[handler::single]
        fn on_window_size(&mut self, _ctx: &mut NativeCtx<'_>, _mail: WindowSize) {
            let _ = self;
        }
    }

    /// The watcher keyed `key`.
    pub(crate) fn watcher(key: &str) -> ActorPath<Watcher> {
        ActorPath::instance(&LoadName::new(key).expect("a valid key"))
    }

    /// Stand a route with `handler` at `key`'s watcher path, answering its
    /// key.
    pub(crate) fn stand(registry: &Registry, key: &str, handler: Arc<dyn InboxHandler>) -> ErasedActorRef {
        registered_ref(registry, watcher(key).as_erased().as_str(), handler)
    }

    /// Prove `key`'s watcher live as a subscriber to `K`, the way a subscribe
    /// receipt does.
    pub(crate) fn subscriber<K: Published>(ctx: &NativeCtx<'_>, key: &str) -> ProtocolRef<Subscriber<K>>
    where
        Subscriber<K>: CoveredBy<Watcher>,
    {
        ctx.resolve(&watcher(key).narrow()).expect("the watcher stands live")
    }

    /// Hold `key`'s watcher as a subscriber to `K` under `selector`.
    pub(crate) fn hold<K: Published>(
        subscribers: &mut WindowSubscribers,
        ctx: &mut NativeCtx<'_>,
        selector: WindowSelector,
        key: &str,
    ) where
        Subscriber<K>: CoveredBy<Watcher>,
    {
        let subscriber = subscriber::<K>(ctx, key);
        subscribers.subscribe(ctx, selector, subscriber);
    }

    /// The keys of `K`'s recipients from `window`.
    pub(crate) fn recipients<K: Published>(
        subscribers: &WindowSubscribers,
        window: &ErasedActorPath,
    ) -> BTreeSet<ErasedActorRef> {
        subscribers.recipients::<K>(window).into_iter().map(ProtocolRef::erase).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use aether_data::{SessionToken, Uuid};
    use aether_kinds::{Key, MouseMove};
    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::noop_handler;
    use aether_substrate::mail::{Source, SourceAddr};
    use aether_substrate::testing::unrouted_binding;

    use super::fixture::{hold, recipients, stand};
    use super::*;

    fn window(name: &str) -> ErasedActorPath {
        crate::window_path(&aether_data::LoadName::new(name).expect("fixture window name"))
    }

    fn fixture() -> (WindowSubscribers, Arc<NativeBinding>, Arc<Registry>) {
        let registry = Arc::new(Registry::new());
        let binding = unrouted_binding(&Arc::new(Mailer::new(Arc::clone(&registry))));

        (WindowSubscribers::new(), binding, registry)
    }

    /// The reflexive forms read their subscriber off the host-stamped
    /// envelope through `ctx.sender()`, so they are only meaningful for an
    /// in-process actor. A `Session` source (an external MCP session or a
    /// remote engine) must use the explicit `subscribe` / `unsubscribe`
    /// instead, and gets an `Err` plus an untouched route table rather than a
    /// silently mis-attributed subscription.
    #[test]
    fn reflexive_subscribe_rejects_a_non_component_source_without_touching_routes() {
        let mut subscribers = WindowSubscribers::new();
        let transport = unrouted_binding(&Arc::new(Mailer::new(Arc::new(Registry::new()))));
        let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
        let mut ctx = NativeCtx::new(&transport, source, None, None);

        assert!(subscribers.subscribe_self(&mut ctx, WindowSelector::All, Key::ID).is_err());
        assert!(subscribers.unsubscribe_self(&ctx, WindowSelector::All, Key::ID).is_err());

        assert!(recipients::<Key>(&subscribers, &window("a")).is_empty(), "a rejected subscribe inserts no route");
    }

    #[test]
    fn one_selector_routes_only_the_selected_window() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let key = stand(&registry, "one", noop_handler());

        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::One(window("a")), "one");

        assert_eq!(recipients::<Key>(&subscribers, &window("a")), BTreeSet::from([key]));
        assert!(recipients::<Key>(&subscribers, &window("b")).is_empty());
    }

    #[test]
    fn all_selector_is_prospective() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let key = stand(&registry, "all", noop_handler());

        hold::<MouseMove>(&mut subscribers, &mut ctx, WindowSelector::All, "all");

        assert_eq!(recipients::<MouseMove>(&subscribers, &window("a")), BTreeSet::from([key]));
        assert_eq!(recipients::<MouseMove>(&subscribers, &window("late")), BTreeSet::from([key]));
    }

    #[test]
    fn all_and_one_union_deduplicates_the_same_subscriber() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let key = stand(&registry, "union", noop_handler());
        let other = stand(&registry, "union-other", noop_handler());

        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::All, "union");
        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::One(window("g")), "union");
        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::One(window("g")), "union-other");

        assert_eq!(subscribers.recipients::<Key>(&window("g")).len(), 2, "one copy per subscriber");
        assert_eq!(recipients::<Key>(&subscribers, &window("g")), BTreeSet::from([key, other]));
    }

    #[test]
    fn unsubscribe_and_departure_cleanup_preserve_other_routes() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let key = stand(&registry, "cleanup", noop_handler());
        let other = stand(&registry, "cleanup-other", noop_handler());

        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::All, "cleanup");
        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::One(window("c")), "cleanup");
        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::One(window("c")), "cleanup-other");

        subscribers.unsubscribe::<Key>(WindowSelector::All, key);
        assert_eq!(recipients::<Key>(&subscribers, &window("c")), BTreeSet::from([key, other]));

        subscribers.unsubscribe_all(key);
        assert_eq!(recipients::<Key>(&subscribers, &window("c")), BTreeSet::from([other]));
    }

    #[test]
    fn monitor_cleanup_purges_only_the_departed_subscriber_from_every_route() {
        let (mut subscribers, binding, registry) = fixture();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, None, None);
        let departed = stand(&registry, "departed", noop_handler());
        let survivor = stand(&registry, "survivor", noop_handler());

        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::All, "departed");
        hold::<Key>(&mut subscribers, &mut ctx, WindowSelector::All, "survivor");
        hold::<MouseMove>(&mut subscribers, &mut ctx, WindowSelector::One(window("c")), "departed");

        subscribers.unsubscribe_all(departed);

        assert_eq!(recipients::<Key>(&subscribers, &window("c")), BTreeSet::from([survivor]));
        assert!(recipients::<MouseMove>(&subscribers, &window("c")).is_empty());
    }
}
