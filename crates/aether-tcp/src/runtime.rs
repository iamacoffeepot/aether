//! The `aether.tcp` cap runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! [`TcpCapability`] identity never names these types
//! nor pulls `aether_substrate`. The substrate / `std::net`-typed imports are
//! gated once by this module rather than line-by-line; the `#[actor] impl`
//! reaches the state, ctx types, and supervisor structs through the single
//! `use runtime::*` glob in the parent.

pub use std::collections::HashMap;
pub use std::net::{TcpListener, TcpStream};
pub use std::sync::mpsc;

pub use aether_substrate::actor::monitor::MonitorHandle;
pub use aether_substrate::actor::native::spawn::Subname;
pub use aether_substrate::actor::native::{
    Held, NativeActor, NativeCtx, NativeInitCtx, Pending, SelfWake, SpawnOutcome, TaskDone,
};
pub use aether_substrate::chassis::error::BootError;

use aether_actor::{ActorRef, ErasedActorRef, PathRefused, ProtocolRef, runtime};
use aether_substrate::Erased;
// `MonitorNotice` is named by `on_monitor_notice`'s signature; the parent's
// import of it is private, so re-import it directly where the body expands.
use aether_kinds::MonitorNotice;
// The moved handler bodies name the cap kinds (`BindListener`, `Close`,
// `ListenerInfo`, …) and the listener child actor + its config; bring them in
// from the parent module where they live always-on.
#[allow(clippy::wildcard_imports)]
use super::kinds::*;
use super::{TcpCapability, TcpListenerActor, TcpListenerConfig, TcpSessionActor, TcpSessionConfig};

/// The shared body of `on_bind` and `on_bind_self`: bind the socket on the
/// dispatcher thread (so a bind failure answers `Err` at once), then stage
/// the bound listener over the already-proven `consumer`. Its task
/// completion registers the monitors, commits the supervisor entry, and
/// answers `held` only after authoritative activation. It takes a ctx of any
/// sender `S`, since `on_bind_self` states one and `on_bind` does not.
fn bind_listener<S>(
    state: &mut TcpCapabilityState,
    ctx: &mut NativeCtx<'_, TcpCapability, S>,
    held: Held<BindListenerResult>,
    addr: String,
    name: Option<String>,
    consumer: ProtocolRef<TcpConsumer>,
) {
    let consumer_erased = consumer.erase();
    let standing = state.listeners.values().find(|entry| {
        let same_address = entry.addr == addr;
        let same_consumer = entry.bound_to(consumer_erased);
        let settled = matches!(entry.pending_unbind, UnbindState::Idle);
        same_address && same_consumer && settled
    });

    if let Some(entry) = standing {
        held.answer(ctx, &BindListenerResult::Ok { listener_name: entry.name.clone(), local_port: entry.port });
        return;
    }

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            held.answer(ctx, &BindListenerResult::failed(addr, format!("bind failed: {e}")));
            return;
        }
    };
    let local_port = match listener.local_addr() {
        Ok(local) => local.port(),
        Err(e) => {
            drop(listener);
            held.answer(ctx, &BindListenerResult::failed(addr, format!("local_addr failed: {e}")));
            return;
        }
    };
    let listener_name = name.unwrap_or_else(|| format!("{local_port}"));

    // The child reservation refuses a duplicate subname while a birth is in
    // flight, so the name keys at most one starting listener.
    let staged = ctx
        .spawn_child::<TcpListenerActor>(
            Subname::Named(&listener_name),
            TcpListenerConfig { listener, addr: addr.clone(), port: local_port, consumer },
            (),
        )
        .stage_with(ListenerSpawnKey { listener_name: listener_name.clone() });
    match staged {
        Ok(_) => {
            state.starting_listeners.insert(listener_name, StartingListener { held, addr, local_port, consumer });
        }
        Err((error, _)) => {
            held.answer(ctx, &BindListenerResult::failed(addr, format!("spawn failed: {error:?}")));
        }
    }
}

/// The shared body of `on_connect` and `on_connect_self`: park the caller's
/// held reply under a fresh connect id, then dial `addr` on a one-shot
/// transport thread that wakes the cap with `ConnectReady`. The session it
/// stages delivers to the already-proven `consumer`. It takes a ctx of any
/// sender `S`, since `on_connect_self` states one and `on_connect` does not.
fn dial<A, S>(
    state: &mut TcpCapabilityState,
    ctx: &mut NativeCtx<'_, A, S>,
    held: Held<ConnectResult>,
    addr: String,
    name: Option<String>,
    consumer: ProtocolRef<TcpConsumer>,
) {
    let id = state.next_connect_id;
    state.next_connect_id += 1;
    state.pending_connects.insert(id, PendingConnect { held, addr: addr.clone(), name, consumer });

    let connect_tx = state.connect_tx.clone();
    let wake = state.connect_wake.clone();

    // Transport thread below the mail layer — it carries a dial result
    // in; no inbound chain to inherit, so no settlement umbrella applies.
    let spawn_result = state.connect_wake.spawn_sidecar(format!("aether-tcp-connect-{id}"), move || {
        if connect_tx.send((id, TcpStream::connect(&addr).map_err(|error| format!("connect failed: {error}")))).is_ok()
        {
            wake.wake(&ConnectReady {});
        }
    });

    if let Err(error) = spawn_result {
        let PendingConnect { held, addr, .. } =
            state.pending_connects.remove(&id).expect("connect inserted before thread spawn");
        held.answer(ctx, &ConnectResult::failed(addr, format!("connect thread spawn failed: {error}")));
    }
}

/// `aether.tcp` runtime state (issue 607 Phase 6a, ADR-0079). The singleton
/// control-plane cap owns its listener fleet directly — it is the supervisor,
/// not a thin shim over the chassis registry. Each listener birth registers a
/// monitor on the new listener and inserts a [`ListenerEntry`] into
/// `listeners` under the listener's reference; `on_monitor_notice` removes
/// the entry on listener close. A dialed session has a [`SessionEntry`] in
/// `sessions` the same way, and `consumers` holds the cap's monitor on each
/// consumer something here is bound to: that consumer's notice closes every
/// listener and dialed session bound to it.
/// The addressing identity is the distinct ZST
/// [`TcpCapability`]. Living in this private module keeps
/// it `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
///
/// Issue 629 / Phase B: plain collection fields. The dispatcher thread is the
/// sole writer / reader; pre-Phase-A's `Mutex<HashMap<...>>` was a
/// worker-pool-era tax, not a contention point.
///
/// Every reply a request still owes waits here as a [`Held`] (ADR-0243 §1),
/// keyed by the work that answers it. Before this state drops, an actor close
/// while the engine keeps running answers the ledger's entries with each
/// reply kind's `unanswered()`, and an engine teardown settles them silently
/// (ADR-0243 §1), so no drain runs at close.
pub struct TcpCapabilityState {
    /// Live listeners spawned by this cap. Each entry holds the proof the
    /// listener's spawn returned, the bind metadata surfaced via
    /// `ListListeners`, any held unbind reply, and the monitor handle that
    /// pins the cap's monitor on the listener until close. Keyed by the
    /// listener's reference, which is the sender of its close notice, so
    /// the notice finds its entry by keyed lookup (ADR-0230).
    pub listeners: HashMap<ErasedActorRef, ListenerEntry>,
    /// Live sessions this cap dialed, in the shape `listeners` has: keyed by
    /// the session's reference, which is the sender of its close notice.
    /// Sessions a listener accepted are that listener's and are not here.
    pub sessions: HashMap<ErasedActorRef, SessionEntry>,
    /// The cap's monitor on each consumer a listener or dialed session here
    /// is bound to, keyed by the consumer's reference, which is the sender
    /// of its close notice.
    ///
    /// One handle per consumer, however many entries name it. The registry
    /// keeps a monitor per `(watcher, target)` call and deregisters every
    /// one of the pair when any handle drops, so a handle on each entry
    /// would end the watch for the others when the first entry left, and a
    /// consumer's close would post one notice per entry. An entry is here
    /// exactly while some `listeners` or `sessions` entry names its
    /// consumer ([`TcpCapabilityState::watch_consumer`],
    /// [`TcpCapabilityState::release_consumer`]).
    pub consumers: HashMap<ErasedActorRef, MonitorHandle>,
    /// Staged listener births awaiting their task completion, keyed by the
    /// listener name their [`ListenerSpawnKey`] carries.
    pub starting_listeners: HashMap<String, StartingListener>,
    /// Monotonic id assigned to the next outbound connect attempt.
    pub next_connect_id: u64,
    /// Outstanding connect replies held until the dial sidecar reports
    /// completion. Keyed by `next_connect_id` values.
    pub pending_connects: HashMap<u64, PendingConnect>,
    /// Dialed sessions whose staged birth awaits its task completion, keyed
    /// by the connect id their [`SessionSpawnKey`] carries.
    pub starting_sessions: HashMap<u64, StartingSession>,
    /// Dial-sidecar result channel. The dispatcher drains it on each
    /// `ConnectReady` wake and correlates results by connect id.
    pub connect_rx: mpsc::Receiver<(u64, Result<TcpStream, String>)>,
    /// Retained sender cloned into each one-shot dial sidecar.
    pub connect_tx: mpsc::Sender<(u64, Result<TcpStream, String>)>,
    /// Wakes this cap with `ConnectReady`; each one-shot dial sidecar holds
    /// a clone.
    pub connect_wake: SelfWake<ConnectReady>,
}

/// Cap-local supervisor state for one live listener. Drops with
/// the entry; `MonitorHandle::Drop` is idempotent with the close
/// path's index drain.
pub struct ListenerEntry {
    pub addr: String,
    pub port: u16,
    pub name: String,
    /// The reference the listener's spawn outcome proved; `on_unbind` mails
    /// `Close` through it. Its erased form keys this entry in `listeners`.
    pub listener: ActorRef<TcpListenerActor>,
    /// The consumer this listener was bound for. Its close closes the
    /// listener.
    pub consumer: ProtocolRef<TcpConsumer>,
    /// Whether an unbind reply is parked on the entry.
    pub pending_unbind: UnbindState,
    // Held to keep the cap's monitor registered against the
    // listener for its lifetime. Drops when the entry is removed
    // (in `on_monitor_notice`).
    _monitor_handle: MonitorHandle,
}

impl ListenerEntry {
    /// Whether this listener was bound for `consumer`.
    fn bound_to(&self, consumer: ErasedActorRef) -> bool {
        self.consumer.erase() == consumer
    }
}

/// Cap-local supervisor state for one live dialed session, in the shape
/// [`ListenerEntry`] has.
pub struct SessionEntry {
    /// The reference the session's spawn outcome proved; the cap mails
    /// `SessionClose` through it when the consumer closes. Its erased form
    /// keys this entry in `sessions`.
    pub session: ActorRef<TcpSessionActor>,
    /// The consumer this session was dialed for. Its close closes the
    /// session.
    pub consumer: ProtocolRef<TcpConsumer>,
    // Held to keep the cap's monitor registered against the session for its
    // lifetime. Drops when the entry is removed (in `on_monitor_notice`).
    _monitor_handle: MonitorHandle,
}

impl SessionEntry {
    /// Whether this session was dialed for `consumer`.
    fn bound_to(&self, consumer: ErasedActorRef) -> bool {
        self.consumer.erase() == consumer
    }
}

impl TcpCapabilityState {
    /// Monitor `consumer` unless the cap already does. Called as an entry
    /// naming it is committed, so a consumer that closed before then is
    /// noticed all the same: the registration posts its notice, which
    /// arrives after the calling handler returns and finds the entry.
    fn watch_consumer(&mut self, ctx: &NativeCtx<'_, TcpCapability>, consumer: ProtocolRef<TcpConsumer>) {
        self.consumers.entry(consumer.erase()).or_insert_with(|| ctx.monitor(consumer));
    }

    /// Stop monitoring `consumer` once no listener or dialed session is
    /// bound to it. Called as an entry naming it is removed.
    fn release_consumer(&mut self, consumer: ProtocolRef<TcpConsumer>) {
        let consumer = consumer.erase();
        let binds = self.listeners.values().any(|entry| entry.bound_to(consumer));
        let dials = self.sessions.values().any(|entry| entry.bound_to(consumer));
        let bound = binds || dials;
        if !bound {
            self.consumers.remove(&consumer);
        }
    }
}

/// An unbind whose reply waits for the listener's close notice.
pub struct PendingUnbind {
    pub held: Held<UnbindListenerResult>,
    pub listener_name: String,
}

/// Whether an unbind reply is parked on a listener entry.
pub enum UnbindState {
    /// No unbind is outstanding.
    Idle,
    /// The held reply waits for the listener close notice.
    Unbinding(PendingUnbind),
}

/// A connect whose reply waits for its dial sidecar.
pub struct PendingConnect {
    pub held: Held<ConnectResult>,
    pub addr: String,
    pub name: Option<String>,
    /// The consumer proven at `Connect` or `ConnectSelf` receipt (ADR-0230,
    /// ADR-0231 §3/§4).
    pub consumer: ProtocolRef<TcpConsumer>,
}

/// A dialed session whose staged birth is answering a connect, plus the
/// request vocabulary its `ConnectResult` quotes.
pub struct StartingSession {
    pub held: Held<ConnectResult>,
    pub addr: String,
    pub session_name: String,
    pub peer: String,
    /// The consumer the session was dialed for, entered with the session
    /// when its birth completes.
    pub consumer: ProtocolRef<TcpConsumer>,
}

/// A bound listener whose staged birth is answering a bind, plus the request
/// vocabulary its `BindListenerResult` quotes.
pub struct StartingListener {
    pub held: Held<BindListenerResult>,
    pub addr: String,
    pub local_port: u16,
    /// The consumer the listener was bound for, entered with the listener
    /// when its birth completes.
    pub consumer: ProtocolRef<TcpConsumer>,
}

/// The context a staged session birth carries into its task completion: the
/// connect id that keys its [`StartingSession`] (ADR-0243 §9). The child's
/// identity rides its `SpawnOutcome`.
#[aether_data::kind(name = "aether.tcp.session_spawn_key", copy)]
pub struct SessionSpawnKey {
    pub connect_id: u64,
}

/// The context a staged listener birth carries into its task completion: the
/// listener name that keys its [`StartingListener`] (ADR-0243 §9).
#[aether_data::kind(name = "aether.tcp.listener_spawn_key")]
pub struct ListenerSpawnKey {
    pub listener_name: String,
}

#[runtime]
impl NativeActor for TcpCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// cap-local listener-fleet supervisor map.
    type State = TcpCapabilityState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.tcp";

    fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<TcpCapabilityState, BootError> {
        let (connect_tx, connect_rx) = mpsc::channel::<(u64, Result<TcpStream, String>)>();
        Ok(TcpCapabilityState {
            listeners: HashMap::new(),
            sessions: HashMap::new(),
            consumers: HashMap::new(),
            starting_listeners: HashMap::new(),
            next_connect_id: 0,
            pending_connects: HashMap::new(),
            starting_sessions: HashMap::new(),
            connect_rx,
            connect_tx,
            connect_wake: ctx.self_wake(),
        })
    }

    /// Dial an outbound TCP stream on a one-shot transport thread and
    /// hold the caller's reply until the staged session activates.
    ///
    /// # Agent
    /// Reply: `ConnectResult`. Asynchronous — the shared cap dispatcher
    /// remains available while the OS resolves and connects `mail.addr`.
    #[handler::request]
    fn on_connect(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased>, mail: Connect) -> Pending<ConnectResult> {
        let (pending, held) = ctx.hold::<ConnectResult>();
        // ADR-0231 §3: the decode proved the consumer covers `TcpConsumer`
        // (a refusal there is answered `Err(Consumer(..))` by the dispatch);
        // prove it is still live once, at receipt.
        match ctx.resolve(&mail.consumer) {
            Ok(consumer) => dial(state, ctx, held, mail.addr, mail.name, consumer),
            Err(error) => held.answer(ctx, &ConnectResult::from(PathRefused::from(error))),
        }
        pending
    }

    /// Dial `mail.addr` with the sender as the session's consumer, as
    /// [`Self::on_connect`] does with an explicit consumer.
    ///
    /// The ctx's sender is the requirement (ADR-0231 §11): an actor sends
    /// this kind only when it covers [`TcpConsumer`], and the engine casts
    /// the sender before this handler runs, so nothing is dialed for a sender
    /// that would warn-drop the session's frames or its close notice.
    ///
    /// # Agent
    /// Reply: `ConnectResult`. A sender whose published rows do not handle
    /// `SessionData` and `SessionClosed` silently is answered
    /// `Err(Consumer(..))` naming it, without dialing; mail with no actor
    /// sender (a session has no inbox to deliver frames to) is refused with
    /// no reply. Otherwise `Err` on the errors `Connect` reports.
    #[handler::request]
    fn on_connect_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, TcpConsumer>,
        mail: ConnectSelf,
    ) -> Pending<ConnectResult> {
        let (pending, held) = ctx.hold::<ConnectResult>();
        let consumer = ctx.sender();
        dial(state, ctx, held, mail.addr, mail.name, consumer);
        pending
    }

    /// Drain completed outbound dials and stage one `TcpSessionActor` per
    /// connected stream. The task completion answers each held
    /// `ConnectResult` only after authoritative activation.
    #[handler::tell]
    fn on_connect_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: ConnectReady) {
        while let Ok((id, result)) = state.connect_rx.try_recv() {
            let Some(PendingConnect { held, addr, name, consumer }) = state.pending_connects.remove(&id) else {
                continue;
            };

            let stream = match result {
                Ok(stream) => stream,
                Err(error) => {
                    held.answer(ctx, &ConnectResult::failed(addr, error));
                    continue;
                }
            };
            let session_name = name.unwrap_or_else(|| format!("conn-{id}"));
            let peer = match stream.peer_addr() {
                Ok(peer) => peer.to_string(),
                Err(error) => {
                    drop(stream);
                    held.answer(ctx, &ConnectResult::failed(addr, format!("peer_addr failed: {error}")));
                    continue;
                }
            };

            let staged = ctx
                .spawn_child::<TcpSessionActor>(
                    Subname::Named(&session_name),
                    TcpSessionConfig { stream, peer: peer.clone(), session_name: session_name.clone(), consumer },
                    (),
                )
                .stage_with(SessionSpawnKey { connect_id: id });
            match staged {
                Ok(_) => {
                    state.starting_sessions.insert(id, StartingSession { held, addr, session_name, peer, consumer });
                }
                Err((error, _)) => {
                    held.answer(ctx, &ConnectResult::failed(addr, format!("spawn failed: {error:?}")));
                }
            }
        }
    }

    /// Spawn a fresh `TcpListenerActor` bound to `mail.addr`.
    ///
    /// Binds the socket on the dispatcher thread (so a bind failure replies
    /// `Err` synchronously), then stages the bound listener. Its task
    /// completion registers the monitor, commits the supervisor entry, and
    /// replies only after authoritative activation.
    ///
    /// # Agent
    /// Reply: `BindListenerResult`. `Ok` on successful bind +
    /// spawn; `Err` on addr parse / bind / spawn / monitor failure.
    #[handler::request]
    fn on_bind(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: BindListener) -> Pending<BindListenerResult> {
        let (pending, held) = ctx.hold::<BindListenerResult>();
        // ADR-0231 §3: the decode proved the consumer covers `TcpConsumer`
        // (a refusal there is answered `Err(Consumer(..))` by the dispatch);
        // prove it is still live once, at receipt, before binding.
        match ctx.resolve(&mail.consumer) {
            Ok(consumer) => bind_listener(state, ctx, held, mail.addr, mail.name, consumer),
            Err(error) => held.answer(ctx, &BindListenerResult::from(PathRefused::from(error))),
        }
        pending
    }

    /// Spawn a fresh `TcpListenerActor` bound to `mail.addr` whose consumer
    /// is the sender, as [`Self::on_bind`] does with an explicit consumer.
    ///
    /// The ctx's sender is the requirement (ADR-0231 §11): an actor sends
    /// this kind only when it covers [`TcpConsumer`], and the engine casts
    /// the sender before this handler runs, so nothing is bound for a sender
    /// that would warn-drop its sessions' frames or close notices.
    ///
    /// # Agent
    /// Reply: `BindListenerResult`. A sender whose published rows do not
    /// handle `SessionData` and `SessionClosed` silently is answered
    /// `Err(Consumer(..))` naming it, without binding; mail with no actor
    /// sender (a session has no inbox to deliver frames to) is refused with
    /// no reply. Otherwise `Err` on the errors `BindListener` reports.
    #[handler::request]
    fn on_bind_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, TcpConsumer>,
        mail: BindListenerSelf,
    ) -> Pending<BindListenerResult> {
        let (pending, held) = ctx.hold::<BindListenerResult>();
        let consumer = ctx.sender();
        bind_listener(state, ctx, held, mail.addr, mail.name, consumer);
        pending
    }

    /// Settle one staged session birth: monitor the activated session and
    /// its consumer, commit its entry, and answer the connect it serves, or
    /// answer with the owner's rejection. A session or a consumer that
    /// closed before this ran is entered all the same, and its notice, which
    /// arrives after this handler returns, finds the entry.
    #[handler(task)]
    fn on_session_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<TcpSessionActor>>,
    ) {
        let Some(SessionSpawnKey { connect_id }) = ctx.take_context() else {
            return;
        };
        let Some(StartingSession { held, addr, session_name, peer, consumer }) =
            state.starting_sessions.remove(&connect_id)
        else {
            return;
        };
        let session = match done.into_output().result {
            Ok(session) => session,
            Err(error) => {
                held.answer(ctx, &ConnectResult::failed(addr, format!("spawn failed: {error:?}")));
                return;
            }
        };

        state
            .sessions
            .insert(session.erase(), SessionEntry { session, consumer, _monitor_handle: ctx.monitor(session) });
        state.watch_consumer(ctx, consumer);
        held.answer(ctx, &ConnectResult::Ok { session_name, peer });
    }

    /// Settle one staged listener birth: monitor the activated listener and
    /// its consumer, commit its supervisor entry, and only then answer the
    /// bind it serves. A listener or a consumer that closed before this ran
    /// is entered and answered all the same, and its notice, which arrives
    /// after this handler returns, finds the entry.
    #[handler(task)]
    fn on_listener_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<TcpListenerActor>>,
    ) {
        let Some(ListenerSpawnKey { listener_name }) = ctx.take_context() else {
            return;
        };
        let Some(StartingListener { held, addr, local_port, consumer }) =
            state.starting_listeners.remove(&listener_name)
        else {
            return;
        };
        let listener = match done.into_output().result {
            Ok(listener) => listener,
            Err(spawn_error) => {
                held.answer(ctx, &BindListenerResult::failed(addr, format!("spawn failed: {spawn_error:?}")));
                return;
            }
        };

        state.listeners.insert(
            listener.erase(),
            ListenerEntry {
                addr,
                port: local_port,
                name: listener_name.clone(),
                listener,
                consumer,
                pending_unbind: UnbindState::Idle,
                _monitor_handle: ctx.monitor(listener),
            },
        );
        state.watch_consumer(ctx, consumer);
        held.answer(ctx, &BindListenerResult::Ok { listener_name, local_port });
    }

    /// Mail `Close` to the named listener and hold the originator's
    /// reply. The reply fires from `on_monitor_notice` once the
    /// listener tombstones.
    ///
    /// # Agent
    /// Reply: `UnbindListenerResult`. Asynchronous — the response
    /// fires after the listener's accept thread joins and its
    /// `MonitorNotice` arrives at this cap.
    #[handler::request]
    fn on_unbind(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Erased>,
        mail: UnbindListener,
    ) -> Pending<UnbindListenerResult> {
        let (pending, held) = ctx.hold::<UnbindListenerResult>();
        // Find the listener in the cap-local supervisor map by
        // name. The cap is the source of truth for "what listeners
        // exist"; no registry walk needed.
        let Some(entry) = state.listeners.values_mut().find(|entry| entry.name == mail.listener_name) else {
            held.answer(
                ctx,
                &UnbindListenerResult::Err {
                    listener_name: mail.listener_name,
                    error: "no such listener (or already closed)".into(),
                },
            );
            return pending;
        };
        // Park the held reply on the entry. The cap's
        // already-registered monitor (set at spawn time) fires
        // MonitorNotice on close, which answers it. Preserve the
        // first caller when a duplicate request arrives while that
        // close is still in flight; replacing it would lose the
        // original reply.
        if matches!(entry.pending_unbind, UnbindState::Unbinding(_)) {
            held.answer(
                ctx,
                &UnbindListenerResult::Err {
                    listener_name: mail.listener_name,
                    error: "unbind already in progress".into(),
                },
            );
            return pending;
        }
        entry.pending_unbind = UnbindState::Unbinding(PendingUnbind { held, listener_name: mail.listener_name });
        let listener = entry.listener;
        // Mail Close through the reference the listener's spawn outcome
        // proved. ADR-0099 §3: the listener is a spawned child, so its id
        // is the lineage fold, not `hash(NAMESPACE:name)` — re-resolving by
        // name would reach a flat id nothing is registered under. The cap
        // kept the proof on the entry at spawn, so it sends through that.
        ctx.send_to(listener, &Close::default());
        pending
    }

    /// Walk the cap-local listener map and report metadata, ordered by
    /// listener name.
    ///
    /// # Agent
    /// Reply: `ListListenersResult`.
    #[handler::request]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListListeners) -> ListListenersResult {
        let mut listeners: Vec<ListenerInfo> = state
            .listeners
            .values()
            .map(|entry| ListenerInfo { name: entry.name.clone(), addr: entry.addr.clone(), port: entry.port })
            .collect();
        // The table is keyed by reference, so its iteration order is
        // arbitrary; the reply is ordered by name so it is deterministic.
        listeners.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        ListListenersResult { listeners }
    }

    /// An actor the cap monitors tombstoned. The host stamps it as the
    /// notice's sender, and the table that sender keys says which it was:
    ///
    /// - `listeners`: a listener closed. Its entry goes, and an unbind held
    ///   on it is answered. The cap's monitor on every spawned listener
    ///   (registered by its birth's completion) fires this, whatever closed
    ///   the listener.
    /// - `sessions`: a dialed session closed. Its entry goes.
    /// - `consumers`: a consumer closed. Every listener bound to it is
    ///   mailed the `Close` an unbind sends, and every session dialed for it
    ///   `SessionClose`. Their entries stay until their own notices arrive.
    ///
    /// A listener or a session entry that goes releases the cap's monitor on
    /// its consumer when it was the last one bound to it. A sender that keys
    /// no table is a notice posted before its handle dropped, and changes
    /// nothing.
    #[handler::event]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased>, _notice: MonitorNotice) {
        let Some(departed) = ctx.sender() else {
            return;
        };

        // The held MonitorHandle drops with the entry; deregister is
        // idempotent with the close path's forward-index drain.
        if let Some(entry) = state.listeners.remove(&departed) {
            state.release_consumer(entry.consumer);
            // The close came from an unbind request when one is parked here;
            // otherwise from the consumer's close or a teardown, with no one
            // to answer.
            if let UnbindState::Unbinding(PendingUnbind { held, listener_name }) = entry.pending_unbind {
                held.answer(ctx, &UnbindListenerResult::Ok { listener_name });
            }
            return;
        }

        if let Some(entry) = state.sessions.remove(&departed) {
            state.release_consumer(entry.consumer);
            return;
        }

        // Dropping the handle ends a watch the consumer's close already
        // drained. A bind or dial for this consumer that commits later
        // monitors it again and is noticed at once.
        let Some(_consumer_monitor) = state.consumers.remove(&departed) else {
            return;
        };
        for entry in state.listeners.values() {
            if entry.bound_to(departed) {
                ctx.send_to(entry.listener, &Close::default());
            }
        }
        for entry in state.sessions.values() {
            if entry.bound_to(departed) {
                ctx.send_to(entry.session, &SessionClose::default());
            }
        }
    }
}
