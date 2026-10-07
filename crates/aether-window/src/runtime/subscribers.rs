//! The selector-aware subscription table every window backend keeps. The
//! subscription *mail surface* over it is the manager's own handlers
//! (`runtime::mod`).

use std::collections::{BTreeMap, HashMap, HashSet};

use aether_actor::{Anyone, ErasedActorRef, ProtocolRef, ReplyMode, ResolveError, Subscriber};
use aether_data::{ActorMail, ErasedActorPath, Kind, KindId};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowSize,
};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::NativeCtx;

use super::routing::key_focus::{KeyFocus, Reach, Take};
use super::routing::{Route, Routed};
use crate::{
    KeyFocusHolder, KeyFocusLost, KeyFocusScope, WindowClosed, WindowFocus, WindowMenuActivated, WindowOpened,
    WindowSelector, WindowSubscription,
};

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

    /// The subscribers of events from `window`: the all-window set, then
    /// the members of `window`'s own set the all-window set lacks, so an
    /// actor subscribed through both selectors receives one copy. Borrowed
    /// and copied out one reference at a time, with no allocation: it runs
    /// on every published event.
    fn recipients(&self, window: &ErasedActorPath) -> impl Iterator<Item = ProtocolRef<Subscriber<K>>> {
        let specific = self.specific.get(window).into_iter().flatten();

        self.all
            .values()
            .copied()
            .chain(specific.filter(|(key, _)| !self.all.contains_key(key)).map(|(_, subscriber)| *subscriber))
    }
}

/// A kind the window manager publishes, with its typed set in
/// [`WindowSubscribers`]. Implemented once per published kind from the one
/// list, so a fan-out of any other kind does not compile.
pub trait Published: ActorMail + Sized + 'static {
    /// This kind's set.
    fn set(subscribers: &WindowSubscribers) -> &KindSubscribers<Self>;

    /// This kind's set, mutably.
    fn set_mut(subscribers: &mut WindowSubscribers) -> &mut KindSubscribers<Self>;
}

/// One actor the window watches: its monitor, its canonical path, and the
/// rows it holds in the typed sets. A subscriber has one, and so does an
/// actor that takes key focus and subscribes to nothing. The monitor is taken
/// and the path read when the record is created, so no record is unwatched,
/// and the path, which an actor keeps for life, is never read again.
struct Watched {
    _monitor: MonitorHandle,
    path: ErasedActorPath,
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
        /// `watched` is the reverse index, keyed by each watched actor's
        /// erased reference: its monitor, its path, and the exact rows it
        /// holds, so a departure removes precisely that actor's rows without
        /// scanning anyone else's. `key_focus` is each window's key focus
        /// slot and key records, which [`Self::publish`] routes the key and
        /// text kinds by.
        pub struct WindowSubscribers {
            $($field: KindSubscribers<$kind>,)+
            watched: HashMap<ErasedActorRef, Watched>,
            key_focus: KeyFocus,
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
                Self { $($field: KindSubscribers::default(),)+ watched: HashMap::new(), key_focus: KeyFocus::default() }
            }

            /// Prove an explicit subscription's path live (ADR-0231 §3: its
            /// decode already proved the path handles the kind silently) and
            /// hold the proof under `selector`.
            pub fn subscribe_path<A, S, M: ReplyMode>(
                &mut self,
                ctx: &mut NativeCtx<'_, A, S, M>,
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
            pub fn unsubscribe_path<A, S, M: ReplyMode>(
                &mut self,
                ctx: &NativeCtx<'_, A, S, M>,
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
            /// or the sender's published rows do not handle `kind` with a
            /// silent or unchecked handler, so its events could never be handled.
            pub fn subscribe_self<A, M: ReplyMode>(
                &mut self,
                ctx: &mut NativeCtx<'_, A, Anyone, M>,
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
                            "{} has no silent or unchecked handler for {}, so it cannot subscribe to it",
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

            /// Decode `payload` as the published kind `kind` names and
            /// publish it for `window`: the synthetic runtime's injected
            /// events.
            ///
            /// # Errors
            ///
            /// `kind` is not published here, or `payload` does not decode as
            /// it; nothing is sent.
            #[cfg(feature = "synthetic")]
            pub fn publish_encoded<A, S, M: ReplyMode>(
                &mut self,
                ctx: &mut NativeCtx<'_, A, S, M>,
                window: &ErasedActorPath,
                kind: KindId,
                payload: &[u8],
            ) -> Result<(), String> {
                $(if kind == <$kind as Kind>::ID {
                    let event = <$kind as Kind>::decode_from_bytes(payload)
                        .ok_or_else(|| format!("the payload does not decode as {}", <$kind as Kind>::NAME))?;
                    self.publish(ctx, window, &event);
                    return Ok(());
                })+
                Err(format!("aether.window does not publish {kind:?}"))
            }
        }
    };
}

published_window_kinds!(window_subscribers);

impl WindowSubscribers {
    /// The record of `actor`, created on its first subscription or its first
    /// take of key focus: the one place an actor comes to be monitored, so
    /// each has one monitor and its departure posts one notice.
    fn watch<A, S, M: ReplyMode>(&mut self, ctx: &NativeCtx<'_, A, S, M>, actor: ErasedActorRef) -> &mut Watched {
        self.watched.entry(actor).or_insert_with(|| Watched {
            _monitor: ctx.monitor(actor),
            path: ctx.actor_path(actor),
            rows: HashSet::new(),
        })
    }

    /// Hold `subscriber` for `K` under `selector`, and monitor it on its
    /// first row.
    pub fn subscribe<K: Published, A, S, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        selector: WindowSelector,
        subscriber: ProtocolRef<Subscriber<K>>,
    ) {
        let row = Row::new(selector, K::ID);

        K::set_mut(self).insert(row.window.as_ref(), subscriber);
        self.watch(ctx, subscriber.erase()).rows.insert(row);
    }

    /// Give `window`'s key focus slot to `holder` with `scope`, monitoring it
    /// when this is the first the window has seen of it.
    pub(super) fn take_key_focus<A, S, M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, A, S, M>,
        window: &ErasedActorPath,
        holder: ProtocolRef<KeyFocusHolder>,
        scope: KeyFocusScope,
    ) -> Take {
        let path = self.watch(ctx, holder.erase()).path.clone();

        self.key_focus.take(window, holder, path, scope)
    }

    /// Empty `window`'s key focus slot when `sender` holds it, answering
    /// whether it did.
    pub(super) fn release_key_focus(&mut self, window: &ErasedActorPath, sender: ErasedActorRef) -> bool {
        self.key_focus.release(window, sender)
    }

    /// Publish `event` for `window`: the one fan-out of the window manager,
    /// which the desktop backend, the synthetic backend and the injected
    /// path all call. The event's [`Route`] says which subscribers of its
    /// kind are sent it, and what routing state it changes once sent.
    pub(super) fn publish<K: Routed, A, S, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        window: &ErasedActorPath,
        event: &K,
    ) {
        match event.route() {
            Route::Everyone => self.fan_out(ctx, window, event, &Reach::Everyone),
            Route::KeyDown { code } => {
                self.key_focus.press(window, code);
                self.fan_out(ctx, window, event, self.key_focus.pressed_under(window, code));
            }
            Route::KeyUp { code } => {
                let reach = self.key_focus.lift(window, code);
                self.fan_out(ctx, window, event, &reach);
            }
            Route::Text => self.fan_out(ctx, window, event, self.key_focus.reach(window)),
            Route::Unfocused => {
                self.fan_out(ctx, window, event, &Reach::Everyone);
                self.key_focus.forget_keys(window);
            }
            Route::Closed => {
                self.fan_out(ctx, window, event, &Reach::Everyone);
                if let Some(holder) = self.key_focus.close(window) {
                    ctx.send_to(holder, &KeyFocusLost { window: window.clone() });
                }
            }
        }
    }

    /// Send `event` to `window`'s subscribers of `K` that `reach` admits.
    /// [`Reach::Everyone`] admits them all: the kind is not narrowed by key
    /// focus, or the key was pressed, or the text arrived, while nobody held
    /// the slot.
    fn fan_out<K: Published, A, S, M: ReplyMode>(
        &self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        window: &ErasedActorPath,
        event: &K,
        reach: &Reach,
    ) {
        let admitted = self.recipients::<K>(window).filter(|subscriber| self.admitted(reach, subscriber.erase()));

        ctx.fanout(admitted, event);
    }

    /// Whether `reach` admits the watched actor `subscriber`, by the path
    /// its record keeps. [`Reach::Everyone`] is answered without the lookup,
    /// which keeps it off the path of every kind key focus does not narrow.
    fn admitted(&self, reach: &Reach, subscriber: ErasedActorRef) -> bool {
        match reach {
            Reach::Everyone => true,
            Reach::Held { .. } => self.watched.get(&subscriber).is_some_and(|watched| reach.admits(&watched.path)),
        }
    }

    /// Remove `key`'s row for `K` under `selector`.
    pub fn unsubscribe<K: Published>(&mut self, selector: WindowSelector, key: ErasedActorRef) {
        self.remove(Row::new(selector, K::ID), key);
    }

    pub fn unsubscribe_self<A, M: ReplyMode>(
        &mut self,
        ctx: &NativeCtx<'_, A, Anyone, M>,
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

    /// Drop every subscription `subscriber` holds, empty every key focus
    /// slot it holds, and release its monitor.
    ///
    /// The caller is the `MonitorNotice` handlers, whose host-stamped sender
    /// is the departed actor (ADR-0230). The watched index names exactly
    /// the rows `subscriber` holds, so this removes those and touches
    /// nothing else.
    pub fn unsubscribe_all(&mut self, subscriber: ErasedActorRef) {
        let Some(watched) = self.watched.remove(&subscriber) else {
            return;
        };
        for row in watched.rows {
            self.remove_row(row, subscriber);
        }
        self.key_focus.forget(subscriber);
    }

    /// The subscribers of `K` events from `window`, one per actor.
    pub fn recipients<K: Published>(
        &self,
        window: &ErasedActorPath,
    ) -> impl Iterator<Item = ProtocolRef<Subscriber<K>>> {
        K::set(self).recipients(window)
    }

    fn remove(&mut self, row: Row, key: ErasedActorRef) {
        if let Some(watched) = self.watched.get_mut(&key) {
            watched.rows.remove(&row);
        }
        self.remove_row(row, key);
    }
}

/// The window runtimes' test rig: a window manager booted pumped on a bare
/// test chassis, and [`Watcher`] subscribers spawned beside it that report
/// every published event they receive.
#[cfg(test)]
pub mod fixture {
    use std::collections::BTreeSet;
    use std::sync::mpsc::{self, Receiver, Sender};

    use aether_actor::{ActorPath, ActorRef, ErasedActorRef, HandlesKind, ProtocolRef, ReplyMode, Root};
    use aether_data::{ErasedActorPath, Kind, KindId, LoadName, SessionToken, Uuid};
    use aether_kinds::{Key, KeyRelease, MouseButton, MouseMove, MouseWheel, TextInput, WindowSize};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};
    use aether_substrate::chassis::builder::PassiveChassis;
    use aether_substrate::chassis::error::BootError;
    #[cfg(feature = "desktop")]
    use aether_substrate::config::SettlementConfig;
    use aether_substrate::mail::MailId;
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::testing::{PumpedDriver, TestChassis, boot_bare_test_chassis, fresh_substrate_and_rx};
    use aether_substrate::{ReplyTarget, Subname};

    use super::{Published, WindowSubscribers};
    #[cfg(feature = "desktop")]
    use crate::runtime::WindowBackend;
    #[cfg(feature = "desktop")]
    use crate::runtime::desktop::DesktopWindows;
    use crate::{
        KeyFocusGained, KeyFocusLost, KeyFocusScope, ReleaseKeyFocus, SubscribeWindow, SubscribeWindowResult,
        TakeKeyFocus, WindowCapability, WindowFocus, WindowSelector, WindowSubscription,
    };

    /// One published event a [`Watcher`] received: the watcher's key, the
    /// event, and the envelope's causal root and stamped sender.
    pub struct Receipt {
        pub watcher: String,
        kind: KindId,
        payload: Vec<u8>,
        pub root: Option<MailId>,
        pub sender: Option<ErasedActorRef>,
    }

    impl Receipt {
        /// The event, when it is a `K`.
        pub fn event<K: Kind>(&self) -> Option<K> {
            if self.kind == K::ID {
                K::decode_from_bytes(&self.payload)
            } else {
                None
            }
        }
    }

    /// Asks a [`Watcher`] to shut down, so the runtime posts its departure
    /// to every actor monitoring it.
    #[aether_data::kind(name = "test.window.watcher.leave", copy, eq)]
    pub struct Leave;

    /// Tells a [`Watcher`] to take key focus in `window` with `scope`: it
    /// mails the window the take itself, as any holder does.
    #[aether_data::kind(name = "test.window.watcher.take_key_focus", eq)]
    pub struct TakeKeyFocusIn {
        pub window: ErasedActorPath,
        pub scope: KeyFocusScope,
    }

    /// Tells a [`Watcher`] to release its key focus in `window`.
    #[aether_data::kind(name = "test.window.watcher.release_key_focus", eq)]
    pub struct ReleaseKeyFocusIn {
        pub window: ErasedActorPath,
    }

    /// Tells a [`Watcher`] to spawn one [`WatcherChild`] beneath itself,
    /// keyed `key`.
    #[aether_data::kind(name = "test.window.watcher.spawn_child", eq)]
    pub struct SpawnChild {
        pub key: String,
    }

    /// A subscriber with a silent handler for every kind a window test
    /// subscribes, and for the two key focus notices. Each handler reports a
    /// [`Receipt`] over the channel its config carries; a report whose
    /// receiver has already dropped is discarded, since a test that wants it
    /// awaits it.
    pub struct Watcher {
        key: String,
        report: Sender<Receipt>,
    }

    #[aether_actor::actor(instanced, root, depends(WindowCapability))]
    impl NativeActor for Watcher {
        const NAMESPACE: &'static str = "test.window.watcher";
        type Config = (String, Sender<Receipt>);

        fn init((key, report): (String, Sender<Receipt>), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { key, report })
        }

        #[handler::event]
        fn on_key(&mut self, ctx: &mut NativeCtx<'_>, mail: Key) {
            self.record(ctx, &mail);
        }

        #[handler::event]
        fn on_key_release(&mut self, ctx: &mut NativeCtx<'_>, mail: KeyRelease) {
            self.record(ctx, &mail);
        }

        #[handler::event]
        fn on_text_input(&mut self, ctx: &mut NativeCtx<'_>, mail: TextInput) {
            self.record(ctx, &mail);
        }

        #[handler::tell]
        fn on_key_focus_gained(&mut self, ctx: &mut NativeCtx<'_>, mail: KeyFocusGained) {
            self.record(ctx, &mail);
        }

        #[handler::tell]
        fn on_key_focus_lost(&mut self, ctx: &mut NativeCtx<'_>, mail: KeyFocusLost) {
            self.record(ctx, &mail);
        }

        #[handler::tell]
        fn on_take_key_focus_in(&mut self, ctx: &mut NativeCtx<'_>, mail: TakeKeyFocusIn) {
            let _ = self;
            ctx.send::<WindowCapability>(&TakeKeyFocus { window: mail.window, scope: mail.scope });
        }

        #[handler::tell]
        fn on_release_key_focus_in(&mut self, ctx: &mut NativeCtx<'_>, mail: ReleaseKeyFocusIn) {
            let _ = self;
            ctx.send::<WindowCapability>(&ReleaseKeyFocus { window: mail.window });
        }

        #[handler::tell]
        fn on_spawn_child(&mut self, ctx: &mut NativeCtx<'_>, mail: SpawnChild) {
            let config = (format!("{}/{}", self.key, mail.key), self.report.clone());
            ctx.spawn_child::<WatcherChild>(Subname::Named(&mail.key), config, ()).stage().expect("the child stages");
        }

        /// The child's birth holds the chain that asked for it, so the test's
        /// send returns once the child is live.
        #[handler(task)]
        fn on_child_born(&mut self, _ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome<WatcherChild>>) {
            let _ = self;
            done.into_output().result.expect("the child is born");
        }

        #[handler::event]
        fn on_mouse_move(&mut self, ctx: &mut NativeCtx<'_>, mail: MouseMove) {
            self.record(ctx, &mail);
        }

        #[handler::event]
        fn on_mouse_button(&mut self, ctx: &mut NativeCtx<'_>, mail: MouseButton) {
            self.record(ctx, &mail);
        }

        #[handler::event]
        fn on_mouse_wheel(&mut self, ctx: &mut NativeCtx<'_>, mail: MouseWheel) {
            self.record(ctx, &mail);
        }

        #[handler::event]
        fn on_window_size(&mut self, ctx: &mut NativeCtx<'_>, mail: WindowSize) {
            self.record(ctx, &mail);
        }

        #[handler::event]
        fn on_window_focus(&mut self, ctx: &mut NativeCtx<'_>, mail: WindowFocus) {
            self.record(ctx, &mail);
        }

        #[handler::tell]
        fn on_leave(&mut self, ctx: &mut NativeCtx<'_>, _mail: Leave) {
            let _ = self;
            ctx.shutdown();
        }
    }

    impl Watcher {
        fn record<K: Kind, A, M: ReplyMode>(&self, ctx: &NativeCtx<'_, A, aether_actor::Anyone, M>, mail: &K) {
            report(&self.report, &self.key, ctx, mail);
        }
    }

    /// Report `mail`, received by the fixture actor keyed `key`.
    fn report<K: Kind, A, M: ReplyMode>(
        report: &Sender<Receipt>,
        key: &str,
        ctx: &NativeCtx<'_, A, aether_actor::Anyone, M>,
        mail: &K,
    ) {
        let _ = report.send(Receipt {
            watcher: key.to_owned(),
            kind: K::ID,
            payload: mail.encode_into_bytes(),
            root: ctx.in_flight_root(),
            sender: ctx.sender(),
        });
    }

    /// A key subscriber beneath a [`Watcher`]: the actor a subtree scope
    /// admits with its parent. It reports as `<parent key>/<its key>`.
    pub struct WatcherChild {
        key: String,
        report: Sender<Receipt>,
    }

    #[aether_actor::actor(instanced, child_of(Watcher))]
    impl NativeActor for WatcherChild {
        const NAMESPACE: &'static str = "test.window.watcher.child";
        type Config = (String, Sender<Receipt>);

        fn init((key, report): (String, Sender<Receipt>), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { key, report })
        }

        #[handler::event]
        fn on_key(&mut self, ctx: &mut NativeCtx<'_>, mail: Key) {
            report(&self.report, &self.key, ctx, &mail);
        }
    }

    /// The watcher keyed `key`, where [`Rig::watcher`] spawns it.
    pub fn watcher(key: &str) -> ActorPath<Watcher> {
        ActorPath::instance(&LoadName::new(key).expect("a valid key"))
    }

    /// The child keyed `key` beneath the watcher keyed `parent`, where
    /// [`SpawnChild`] spawns it.
    pub fn watcher_child(parent: &str, key: &str) -> ActorPath<WatcherChild> {
        ActorPath::child(&watcher(parent), &LoadName::new(key).expect("a valid key")).expect("a valid child path")
    }

    /// The keys of `K`'s recipients from `window`.
    pub fn recipients<K: Published>(
        subscribers: &WindowSubscribers,
        window: &ErasedActorPath,
    ) -> BTreeSet<ErasedActorRef> {
        subscribers.recipients::<K>(window).map(ProtocolRef::erase).collect()
    }

    /// The keys of the watchers that received `receipts`, sorted.
    pub fn receivers(receipts: &[Receipt]) -> Vec<&str> {
        let mut keys = receipts.iter().map(|receipt| receipt.watcher.as_str()).collect::<Vec<_>>();
        keys.sort_unstable();
        keys
    }

    fn session() -> ReplyTarget {
        ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0x7041)), correlation: 1 }
    }

    /// A window manager `M` booted pumped on a bare test chassis and driven
    /// through [`PumpedDriver`]. Mail reaches it only through the chassis and
    /// runs only on the driver's mail-wake drains, so a test decides exactly
    /// which turns have run. Replies go to one hub session; watchers report
    /// into one channel.
    pub struct Rig<M: Root + NativeActor> {
        pub driver: PumpedDriver<M>,
        egress: Receiver<EgressEvent>,
        report: Sender<Receipt>,
        receipts: Receiver<Receipt>,
    }

    impl<M: Root + NativeActor<Config = ()>> Rig<M> {
        pub fn boot(params: M::Params) -> Self {
            let (registry, mailer, egress) = fresh_substrate_and_rx();
            let driver = PumpedDriver::boot(boot_bare_test_chassis(&registry, &mailer), (), params);
            let (report, receipts) = mpsc::channel();

            Self { driver, egress, report, receipts }
        }

        pub fn chassis(&self) -> &PassiveChassis<TestChassis> {
            self.driver.chassis()
        }

        pub fn manager(&self) -> ActorRef<M> {
            self.chassis().actor_ref::<M>()
        }

        /// Spawn the watcher keyed `key`, reporting into this rig.
        pub fn watcher(&self, key: &str) -> ActorRef<Watcher> {
            self.chassis()
                .spawn_actor_for_test::<Watcher>(Subname::Named(key), (key.to_owned(), self.report.clone()), ())
                .finish()
                .expect("the watcher spawns")
        }

        /// Queue `mail` on the manager as a tracked root answered to the rig's
        /// session, without pumping it: for a request whose reply the manager
        /// holds past its turn, or one that must wait behind later mail.
        pub fn push<K: Kind>(&self, mail: &K) -> MailId
        where
            M: HandlesKind<K>,
        {
            self.push_to(self.manager(), mail)
        }

        /// [`Self::push`] to `to`: for a request one of the manager's
        /// children forwards and the manager holds past its turn.
        pub fn push_to<R: HandlesKind<K>, K: Kind>(&self, to: ActorRef<R>, mail: &K) -> MailId {
            self.driver.send_tracked(to, mail, Some(session()))
        }

        /// [`Self::send_to`] the manager.
        pub fn send<K: Kind>(&mut self, mail: &K) -> Vec<Receipt>
        where
            M: HandlesKind<K>,
        {
            self.send_to(self.manager(), mail).1
        }

        /// Push `mail` to `to` as a tracked chassis root answered to the rig's
        /// session, and pump the manager until the whole chain settles:
        /// answer the root beside every receipt the chain delivered.
        pub fn send_to<R: HandlesKind<K>, K: Kind>(&mut self, to: ActorRef<R>, mail: &K) -> (MailId, Vec<Receipt>) {
            let root = self.driver.send_and_settle(to, mail, Some(session()));
            (root, self.receipts.try_iter().collect())
        }

        /// Pump the manager on each mail wake until `done` holds of its
        /// state: the wait for an effect off any chain the test holds.
        pub fn pump_until(&mut self, what: &str, done: impl FnMut(&M::State) -> bool) {
            self.driver.pump_until(what, done);
        }

        /// The next session reply of kind `R`, already sent: a reply precedes
        /// its root's settlement, so it is read after the wait, never waited on.
        pub fn reply<R: Kind>(&self) -> R {
            self.egress
                .try_iter()
                .find_map(|event| match event {
                    EgressEvent::ToSession { kind_name, payload, .. } if kind_name == R::NAME => {
                        R::decode_from_bytes(&payload)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("a {} reply was sent", R::NAME))
        }

        /// Every session reply of kind `R` already sent.
        pub fn replies<R: Kind>(&self) -> Vec<R> {
            self.egress
                .try_iter()
                .filter_map(|event| match event {
                    EgressEvent::ToSession { kind_name, payload, .. } if kind_name == R::NAME => {
                        R::decode_from_bytes(&payload)
                    }
                    _ => None,
                })
                .collect()
        }

        /// The next `count` receipts of events published outside any chain
        /// the rig waits on, each awaited on the watcher channel under the
        /// settlement cap.
        #[cfg(feature = "desktop")]
        pub fn receipts(&self, count: usize) -> Vec<Receipt> {
            let cap = SettlementConfig::from_env().to_cap();
            (0..count)
                .map(|_| self.receipts.recv_timeout(cap).expect("a watcher receipt arrives within the settlement cap"))
                .collect()
        }

        /// Tell `watcher` to take key focus in `window` with `scope`, and
        /// answer the receipts the take delivered once its chain settles.
        pub fn take(
            &mut self,
            watcher: ActorRef<Watcher>,
            window: &ErasedActorPath,
            scope: KeyFocusScope,
        ) -> Vec<Receipt> {
            self.send_to(watcher, &TakeKeyFocusIn { window: window.clone(), scope }).1
        }

        /// Tell `watcher` to release its key focus in `window`, and answer
        /// the receipts the release delivered once its chain settles.
        pub fn release(&mut self, watcher: ActorRef<Watcher>, window: &ErasedActorPath) -> Vec<Receipt> {
            self.send_to(watcher, &ReleaseKeyFocusIn { window: window.clone() }).1
        }

        /// Subscribe through the explicit `aether.window.subscribe`,
        /// answering the manager's reply.
        pub fn subscribe(&mut self, selector: WindowSelector, subscription: WindowSubscription) -> SubscribeWindowResult
        where
            M: HandlesKind<SubscribeWindow>,
        {
            self.send(&SubscribeWindow { selector, subscription });
            self.reply()
        }
    }

    impl Rig<WindowCapability> {
        /// The manager booted with the synthetic backend.
        #[cfg(feature = "synthetic")]
        pub fn synthetic() -> Self {
            Self::boot(crate::WindowParams::Synthetic)
        }

        /// The manager booted with the desktop backend, pumped as the desktop
        /// chassis boots it.
        #[cfg(feature = "desktop")]
        pub fn desktop() -> Self {
            Self::boot(crate::WindowParams::Desktop(crate::DesktopWindowBoot::for_test()))
        }

        /// Run `turn` against the desktop backend on a host turn, as the
        /// desktop application does.
        #[cfg(feature = "desktop")]
        pub fn desktop_turn<T>(
            &mut self,
            turn: impl FnOnce(
                &mut DesktopWindows,
                &mut NativeCtx<'_, WindowCapability, aether_actor::Anyone, aether_actor::Single>,
            ) -> T,
        ) -> Option<T> {
            self.driver
                .host_turn(|state, ctx| match &mut state.backend {
                    WindowBackend::Desktop(windows) => Some(turn(windows, ctx)),
                    #[cfg(feature = "synthetic")]
                    WindowBackend::Synthetic(_) => None,
                })
                .flatten()
        }

        /// Read the desktop backend's state.
        #[cfg(feature = "desktop")]
        pub fn read_desktop<T>(&self, read: impl FnOnce(&DesktopWindows) -> T) -> Option<T> {
            self.driver
                .read_state(|state| match &state.backend {
                    WindowBackend::Desktop(windows) => Some(read(windows)),
                    #[cfg(feature = "synthetic")]
                    WindowBackend::Synthetic(_) => None,
                })
                .flatten()
        }

        /// [`Self::pump_until`] `done` holds of the desktop backend.
        #[cfg(feature = "desktop")]
        pub fn pump_desktop_until(&mut self, what: &str, mut done: impl FnMut(&DesktopWindows) -> bool) {
            self.driver.pump_until(what, |state| match &state.backend {
                WindowBackend::Desktop(windows) => done(windows),
                #[cfg(feature = "synthetic")]
                WindowBackend::Synthetic(_) => false,
            });
        }

        /// Inject `event` as published at `window` and answer the receipts
        /// its fan-out delivered.
        #[cfg(feature = "synthetic")]
        pub fn inject<K: Kind>(&mut self, window: &ErasedActorPath, event: &K) -> Vec<Receipt> {
            self.send(&crate::InjectWindowEvent {
                window: window.clone(),
                kind: K::ID,
                payload: event.encode_into_bytes(),
            })
        }
    }
}

#[cfg(all(test, feature = "synthetic"))]
mod tests {
    use std::collections::BTreeSet;

    use aether_kinds::{Key, MouseMove};

    use super::fixture::{Leave, Rig, receivers, recipients, watcher};
    use super::*;
    use crate::WindowSelector::All;
    use crate::{
        SubscribeWindowResult, SubscribeWindowSelf, UnsubscribeWindow, UnsubscribeWindowSelf, WindowCapability,
    };

    fn window(name: &str) -> ErasedActorPath {
        crate::window_path(&aether_data::LoadName::new(name).expect("fixture window name"))
    }

    fn rig() -> Rig<WindowCapability> {
        Rig::synthetic()
    }

    fn keys(name: &str) -> WindowSubscription {
        WindowSubscription::Key(watcher(name).narrow())
    }

    fn key_at(window: &ErasedActorPath) -> Key {
        Key { window: window.clone(), code: 41 }
    }

    fn one(name: &str) -> WindowSelector {
        WindowSelector::One(window(name))
    }

    fn subscribed(result: &SubscribeWindowResult) -> bool {
        matches!(result, SubscribeWindowResult::Ok)
    }

    /// The reflexive forms read their subscriber off the host-stamped
    /// envelope, so they are only meaningful for an in-process actor. A
    /// `Session` source (an external MCP session or a remote engine) must use
    /// the explicit `subscribe` / `unsubscribe` instead, and is answered `Err`
    /// rather than a subscription attributed to some other actor. This fails
    /// if the no-sender arm stops refusing.
    #[test]
    fn reflexive_subscribe_rejects_a_non_component_source() {
        let mut rig = rig();

        rig.send(&SubscribeWindowSelf { selector: All, kind: Key::ID });
        assert!(matches!(rig.reply(), SubscribeWindowResult::Err(_)), "a session cannot subscribe itself");

        rig.send(&UnsubscribeWindowSelf { selector: All, kind: Key::ID });
        assert!(matches!(rig.reply(), SubscribeWindowResult::Err(_)), "a session cannot unsubscribe itself");
    }

    /// Fails if a `One` selector stores its row under every window, or under
    /// the wrong one.
    #[test]
    fn one_selector_routes_only_the_selected_window() {
        let mut rig = rig();
        rig.watcher("one");
        assert!(subscribed(&rig.subscribe(one("a"), keys("one"))));

        assert!(rig.inject(&window("b"), &key_at(&window("b"))).is_empty(), "another window's key reaches nobody");
        assert_eq!(receivers(&rig.inject(&window("a"), &key_at(&window("a")))), ["one"]);
    }

    /// Fails if an `All` subscription snapshots the windows that existed
    /// when it was taken instead of covering every later one.
    #[test]
    fn all_selector_is_prospective() {
        let mut rig = rig();
        rig.watcher("all");
        let mouse = WindowSubscription::MouseMove(watcher("all").narrow());
        assert!(subscribed(&rig.subscribe(All, mouse)));

        let late = window("late");
        let receipts = rig.inject(&late, &MouseMove { window: late.clone(), x: 1.0, y: 2.0 });

        assert_eq!(receivers(&receipts), ["all"], "a window born after the subscription still reaches it");
    }

    /// Fails if an actor subscribed through both `All` and `One` receives
    /// the window's event twice, or if the union drops a `One`-only
    /// subscriber.
    #[test]
    fn all_and_one_union_deduplicates_the_same_subscriber() {
        let mut rig = rig();
        rig.watcher("union");
        rig.watcher("union-other");
        assert!(subscribed(&rig.subscribe(All, keys("union"))));
        assert!(subscribed(&rig.subscribe(one("g"), keys("union"))));
        assert!(subscribed(&rig.subscribe(one("g"), keys("union-other"))));

        let receipts = rig.inject(&window("g"), &key_at(&window("g")));

        assert_eq!(receivers(&receipts), ["union", "union-other"], "one copy per subscriber");
    }

    /// Fails if an unsubscribe removes a row other than the one it names:
    /// the subscriber's own `One` row, or another subscriber's.
    #[test]
    fn unsubscribe_removes_only_the_named_row() {
        let mut rig = rig();
        rig.watcher("cleanup");
        rig.watcher("cleanup-other");
        assert!(subscribed(&rig.subscribe(All, keys("cleanup"))));
        assert!(subscribed(&rig.subscribe(one("c"), keys("cleanup"))));
        assert!(subscribed(&rig.subscribe(one("c"), keys("cleanup-other"))));

        rig.send(&UnsubscribeWindow { selector: All, subscription: keys("cleanup") });
        assert!(subscribed(&rig.reply()));

        assert_eq!(receivers(&rig.inject(&window("c"), &key_at(&window("c")))), ["cleanup", "cleanup-other"]);
        assert!(rig.inject(&window("d"), &key_at(&window("d"))).is_empty(), "the `All` row is gone");
    }

    /// A subscriber that departs leaves every route it held once its
    /// `MonitorNotice` is processed, and nobody else's. Fails if departure
    /// cleanup misses one of the departed subscriber's rows or takes a
    /// survivor's with it.
    #[test]
    fn departure_purges_only_the_departed_subscriber_from_every_route() {
        let mut rig = rig();
        let departed = rig.watcher("departed");
        let survivor = rig.watcher("survivor");
        assert!(subscribed(&rig.subscribe(All, keys("departed"))));
        assert!(subscribed(&rig.subscribe(All, keys("survivor"))));
        let mouse = WindowSubscription::MouseMove(watcher("departed").narrow());
        assert!(subscribed(&rig.subscribe(one("c"), mouse)));

        rig.send_to(departed, &Leave);
        let c = window("c");
        rig.pump_until("the departure notice", |state| {
            recipients::<Key>(state.subscribers(), &c) == BTreeSet::from([survivor.erase()])
                && recipients::<MouseMove>(state.subscribers(), &c).is_empty()
        });

        assert_eq!(receivers(&rig.inject(&c, &key_at(&c))), ["survivor"]);
    }
}
