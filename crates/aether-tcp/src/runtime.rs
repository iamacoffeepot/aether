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
/// completion registers the monitor, commits the supervisor entry, and
/// answers `held` only after authoritative activation.
fn bind_listener(
    state: &mut TcpCapabilityState,
    ctx: &mut NativeCtx<'_, TcpCapability>,
    held: Held<BindListenerResult>,
    addr: String,
    name: Option<String>,
    consumer: Option<ProtocolRef<TcpConsumer>>,
) {
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
            TcpListenerConfig { listener: Some(listener), addr: addr.clone(), port: local_port, consumer },
            (),
        )
        .stage_with(ListenerSpawnKey { listener_name: listener_name.clone() });
    match staged {
        Ok(_) => {
            state.starting_listeners.insert(listener_name, StartingListener { held, addr, local_port });
        }
        Err((error, _)) => {
            held.answer(ctx, &BindListenerResult::failed(addr, format!("spawn failed: {error:?}")));
        }
    }
}

/// The shared body of `on_connect` and `on_connect_self`: park the caller's
/// held reply under a fresh connect id, then dial `addr` on a one-shot
/// transport thread that wakes the cap with `ConnectReady`. The session it
/// stages delivers to the already-proven `consumer`.
fn dial<A>(
    state: &mut TcpCapabilityState,
    ctx: &mut NativeCtx<'_, A>,
    held: Held<ConnectResult>,
    addr: String,
    name: Option<String>,
    consumer: Option<ProtocolRef<TcpConsumer>>,
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
/// the entry on listener close.
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
    /// The unbind reply held until this listener's close notice arrives.
    /// One unbind at a time: a second request while this is `Some` is
    /// refused.
    pub pending_unbind: Option<PendingUnbind>,
    // Held to keep the cap's monitor registered against the
    // listener for its lifetime. Drops when the entry is removed
    // (in `on_monitor_notice`).
    _monitor_handle: MonitorHandle,
}

/// An unbind whose reply waits for the listener's close notice.
pub struct PendingUnbind {
    pub held: Held<UnbindListenerResult>,
    pub listener_name: String,
}

/// A connect whose reply waits for its dial sidecar.
pub struct PendingConnect {
    pub held: Held<ConnectResult>,
    pub addr: String,
    pub name: Option<String>,
    /// The consumer proven at `Connect` or `ConnectSelf` receipt (ADR-0230,
    /// ADR-0231 §3/§4).
    pub consumer: Option<ProtocolRef<TcpConsumer>>,
}

/// A dialed session whose staged birth is answering a connect, plus the
/// request vocabulary its `ConnectResult` quotes.
pub struct StartingSession {
    pub held: Held<ConnectResult>,
    pub addr: String,
    pub session_name: String,
    pub peer: String,
}

/// A bound listener whose staged birth is answering a bind, plus the request
/// vocabulary its `BindListenerResult` quotes.
pub struct StartingListener {
    pub held: Held<BindListenerResult>,
    pub addr: String,
    pub local_port: u16,
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

/// Type a `_self` request's sender as the session consumer (ADR-0231 §4's
/// guard cast), or name why it cannot be one: the mail has no actor sender,
/// or the sender's published rows do not cover [`TcpConsumer`].
fn cast_consumer<A>(ctx: &NativeCtx<'_, A>, request: &str) -> Result<ProtocolRef<TcpConsumer>, String> {
    let sender = ctx.sender().ok_or_else(|| format!("{request} needs an actor sender to deliver frames to"))?;
    ctx.cast::<TcpConsumer>(sender).ok_or_else(|| {
        format!("{request} sender does not handle `SessionData` and `SessionClosed` silently (TcpConsumer)")
    })
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
        match mail.consumer.as_ref().map(|path| ctx.resolve(path)).transpose() {
            Ok(consumer) => dial(state, ctx, held, mail.addr, mail.name, consumer),
            Err(error) => held.answer(ctx, &ConnectResult::from(PathRefused::from(error))),
        }
        pending
    }

    /// Dial `mail.addr` with the sender as the session's consumer, as
    /// [`Self::on_connect`] does with an explicit consumer.
    ///
    /// The sender is cast to [`TcpConsumer`] once, at receipt (ADR-0231
    /// §4), so nothing is dialed for a sender that would warn-drop the
    /// session's frames or its close notice.
    ///
    /// # Agent
    /// Reply: `ConnectResult`. `Err` when the mail carries no actor sender
    /// (a session has no inbox to deliver frames to), when the sender's
    /// published rows do not handle `SessionData` and `SessionClosed`
    /// silently, or on the errors `Connect` reports.
    #[handler::request]
    fn on_connect_self(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: ConnectSelf) -> Pending<ConnectResult> {
        let (pending, held) = ctx.hold::<ConnectResult>();
        match cast_consumer(ctx, "connect_self") {
            Ok(consumer) => dial(state, ctx, held, mail.addr, mail.name, Some(consumer)),
            Err(error) => held.answer(ctx, &ConnectResult::failed(mail.addr, error)),
        }
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
                    TcpSessionConfig {
                        stream: Some(stream),
                        peer: peer.clone(),
                        session_name: session_name.clone(),
                        consumer,
                    },
                    (),
                )
                .stage_with(SessionSpawnKey { connect_id: id });
            match staged {
                Ok(_) => {
                    state.starting_sessions.insert(id, StartingSession { held, addr, session_name, peer });
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
        match mail.consumer.as_ref().map(|path| ctx.resolve(path)).transpose() {
            Ok(consumer) => bind_listener(state, ctx, held, mail.addr, mail.name, consumer),
            Err(error) => held.answer(ctx, &BindListenerResult::from(PathRefused::from(error))),
        }
        pending
    }

    /// Spawn a fresh `TcpListenerActor` bound to `mail.addr` whose consumer
    /// is the sender, as [`Self::on_bind`] does with an explicit consumer.
    ///
    /// The sender is cast to [`TcpConsumer`] once, at receipt (ADR-0231
    /// §4), so nothing is bound for a sender that would warn-drop its
    /// sessions' frames or close notices.
    ///
    /// # Agent
    /// Reply: `BindListenerResult`. `Err` when the mail carries no actor
    /// sender (a session has no inbox to deliver frames to), when the
    /// sender's published rows do not handle `SessionData` and
    /// `SessionClosed` silently, or on the errors `BindListener` reports.
    #[handler::request]
    fn on_bind_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: BindListenerSelf,
    ) -> Pending<BindListenerResult> {
        let (pending, held) = ctx.hold::<BindListenerResult>();
        match cast_consumer(ctx, "bind_listener_self") {
            Ok(consumer) => bind_listener(state, ctx, held, mail.addr, mail.name, Some(consumer)),
            Err(error) => held.answer(ctx, &BindListenerResult::failed(mail.addr, error)),
        }
        pending
    }

    /// Settle one staged session birth: answer the connect it serves with
    /// the activated session, or with the owner's rejection.
    #[handler(task)]
    fn on_session_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<TcpSessionActor>>,
    ) {
        let Some(SessionSpawnKey { connect_id }) = ctx.take_context() else {
            return;
        };
        let Some(StartingSession { held, addr, session_name, peer }) = state.starting_sessions.remove(&connect_id)
        else {
            return;
        };
        let reply = match done.into_output().result {
            Ok(_) => ConnectResult::Ok { session_name, peer },
            Err(error) => ConnectResult::failed(addr, format!("spawn failed: {error:?}")),
        };
        held.answer(ctx, &reply);
    }

    /// Settle one staged listener birth: monitor the activated listener,
    /// commit its supervisor entry, and only then answer the bind it serves.
    /// A listener that closed before this ran is entered and answered all
    /// the same, and its notice, which arrives after this handler returns,
    /// removes the entry.
    #[handler(task)]
    fn on_listener_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<TcpListenerActor>>,
    ) {
        let Some(ListenerSpawnKey { listener_name }) = ctx.take_context() else {
            return;
        };
        let Some(StartingListener { held, addr, local_port }) = state.starting_listeners.remove(&listener_name) else {
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
                pending_unbind: None,
                _monitor_handle: ctx.monitor(listener.erase()),
            },
        );
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
        if entry.pending_unbind.is_some() {
            held.answer(
                ctx,
                &UnbindListenerResult::Err {
                    listener_name: mail.listener_name,
                    error: "unbind already in progress".into(),
                },
            );
            return pending;
        }
        entry.pending_unbind = Some(PendingUnbind { held, listener_name: mail.listener_name });
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

    /// Listener tombstoned — remove from the supervisor map and
    /// answer the held unbind reply if one is waiting.
    ///
    /// The host stamps the closed listener as the notice's sender, so its
    /// entry is the one keyed by `ctx.sender()`. The cap's monitor on every
    /// spawned listener (registered by its birth's completion) fires this
    /// notice; if the close came from an unbind request, the entry's
    /// `pending_unbind` holds the originator's reply.
    #[handler::event]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased>, _notice: MonitorNotice) {
        // Drop the supervisor entry. The held MonitorHandle drops
        // here; deregister is idempotent with the close path's
        // forward-index drain.
        let Some(entry) = ctx.sender().and_then(|departed| state.listeners.remove(&departed)) else {
            return;
        };
        // Answer the held unbind reply if one was waiting.
        if let Some(PendingUnbind { held, listener_name }) = entry.pending_unbind {
            held.answer(ctx, &UnbindListenerResult::Ok { listener_name });
        }
        // Else: notice came from a non-unbind close (chassis
        // shutdown, future trap). Nothing to reply to; the
        // supervisor entry is gone, that's the cleanup.
    }
}
