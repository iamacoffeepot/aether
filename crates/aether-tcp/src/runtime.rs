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

pub use aether_actor::Manual;
// The manual handlers issue their own replies through `ctx.reply` /
// `ctx.reply_to`, the `OutboundReply` trait methods, so the trait must be in
// scope where those handler bodies expand.
pub use aether_actor::OutboundReply;
pub use aether_substrate::actor::monitor::MonitorHandle;
pub use aether_substrate::actor::native::spawn::Subname;
pub use aether_substrate::actor::native::{
    DeferredReply, NativeActor, NativeCtx, NativeInitCtx, SelfWake, SpawnOutcome, TaskDone,
};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::runtime::trace::SettlementHold;

use aether_actor::{ActorRef, ErasedActorRef, runtime};
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
/// dispatcher thread (so a bind failure replies `Err` synchronously), then
/// stage the bound listener over the already-proven `consumer`. Its task
/// completion registers the monitor, commits the supervisor entry, and
/// replies only after authoritative activation.
fn bind_listener(
    ctx: &mut NativeCtx<'_, TcpCapability, Manual>,
    addr: String,
    name: Option<String>,
    consumer: Option<ErasedActorRef>,
) {
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            ctx.reply(&BindListenerResult::Err { addr, error: format!("bind failed: {e}") });
            return;
        }
    };
    let local_port = match listener.local_addr() {
        Ok(local) => local.port(),
        Err(e) => {
            drop(listener);
            ctx.reply(&BindListenerResult::Err { addr, error: format!("local_addr failed: {e}") });
            return;
        }
    };
    let subname_str = name.unwrap_or_else(|| format!("{local_port}"));
    let owed = ctx.defer_reply_to(ctx.reply_target());

    if let Err((error, owed)) = ctx
        .spawn_child::<TcpListenerActor>(
            Subname::Named(&subname_str),
            TcpListenerConfig { listener: Some(listener), addr: addr.clone(), port: local_port, consumer },
            (),
        )
        .continue_from(owed, ListenerSpawn { addr: addr.clone(), listener_name: subname_str.clone(), local_port })
    {
        owed.reply(ctx, &BindListenerResult::Err { addr, error: format!("spawn failed: {error:?}") });
    }
}

/// The shared body of `on_connect` and `on_connect_self`: park the caller's
/// reply under a fresh connect id, then dial `addr` on a one-shot transport
/// thread that wakes the cap with `ConnectReady`. The session it stages
/// delivers to the already-proven `consumer`.
fn dial<A>(
    state: &mut TcpCapabilityState,
    ctx: &mut NativeCtx<'_, A, Manual>,
    addr: String,
    name: Option<String>,
    consumer: Option<ErasedActorRef>,
) {
    let id = state.next_connect_id;
    state.next_connect_id += 1;
    state.pending_connects.insert(
        id,
        PendingConnect { owed: ctx.defer_reply_to(ctx.reply_target()), addr: addr.clone(), name, consumer },
    );

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
        let PendingConnect { owed, addr, .. } =
            state.pending_connects.remove(&id).expect("connect inserted before thread spawn");
        reply_to_pending_connect(
            ctx,
            owed,
            &ConnectResult::Err { addr, error: format!("connect thread spawn failed: {error}") },
        );
    }
}

/// `aether.tcp` runtime state (issue 607 Phase 6a, ADR-0079). The singleton
/// control-plane cap owns its listener fleet directly — it is the supervisor,
/// not a thin shim over the chassis registry. Each `on_bind` registers a
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
pub struct TcpCapabilityState {
    /// Live listeners spawned by this cap. Each entry holds the proof the
    /// listener's spawn returned, the bind metadata surfaced via
    /// `ListListeners`, any parked unbind reply, and the monitor handle that
    /// pins the cap's monitor on the listener until close. Keyed by the
    /// listener's reference, which is the sender of its close notice, so
    /// the notice finds its entry by keyed lookup (ADR-0230).
    pub listeners: HashMap<ErasedActorRef, ListenerEntry>,
    /// Monotonic id assigned to the next outbound connect attempt.
    pub next_connect_id: u64,
    /// Outstanding connect replies parked until the dial sidecar
    /// reports completion. Keyed by `next_connect_id` values.
    pub pending_connects: HashMap<u64, PendingConnect>,
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
    /// The unbind reply parked until this listener's close notice arrives.
    /// One unbind at a time: a second request while this is `Some` is
    /// refused.
    pub pending_unbind: Option<PendingUnbind>,
    // Held to keep the cap's monitor registered against the
    // listener for its lifetime. Drops when the entry is removed
    // (in `on_monitor_notice`).
    _monitor_handle: MonitorHandle,
}

pub struct PendingUnbind {
    pub sender: aether_data::Source,
    /// The unbind caller's chain, kept open until the listener's close
    /// notice drives the reply. Absent when the request arrived on no
    /// chain, in which case nothing gates the reply (ADR-0168 §2).
    pub hold: Option<SettlementHold>,
    pub listener_name: String,
}

pub struct PendingConnect {
    pub owed: DeferredReply,
    pub addr: String,
    pub name: Option<String>,
    /// The consumer proven at `Connect` receipt (ADR-0230).
    pub consumer: Option<ErasedActorRef>,
}

/// The dial a staged session birth is answering, plus the request vocabulary
/// its `ConnectResult` quotes. The child's identity rides its `SpawnOutcome`;
/// the outcome's child type selects the reply kind, so a dial and a bind
/// complete into separate handlers.
#[derive(Clone)]
pub struct OutboundSessionSpawn {
    pub addr: String,
    pub session_name: String,
    pub peer: String,
}

/// The bind a staged listener birth is answering, plus the request vocabulary
/// its `BindListenerResult` quotes.
#[derive(Clone)]
pub struct ListenerSpawn {
    pub addr: String,
    pub listener_name: String,
    pub local_port: u16,
}

fn reply_to_pending_connect<A>(ctx: &mut NativeCtx<'_, A, Manual>, owed: DeferredReply, result: &ConnectResult) {
    owed.reply(ctx, result);
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
            next_connect_id: 0,
            pending_connects: HashMap::new(),
            connect_rx,
            connect_tx,
            connect_wake: ctx.self_wake(),
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        for (_, pending) in state.pending_connects.drain() {
            pending.owed.abandon_for_actor_close();
        }
    }

    /// Dial an outbound TCP stream on a one-shot transport thread and
    /// park the caller's reply until `ConnectReady` reports completion.
    ///
    /// # Agent
    /// Reply: `ConnectResult`. Asynchronous — the shared cap dispatcher
    /// remains available while the OS resolves and connects `mail.addr`.
    #[handler::manual]
    fn on_connect(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: Connect) {
        // ADR-0230: prove the consumer once, at receipt.
        let consumer = match mail.consumer.map(|position| ctx.resolve_live(position)).transpose() {
            Ok(consumer) => consumer,
            Err(error) => {
                ctx.reply(&ConnectResult::Err { addr: mail.addr, error: format!("consumer not live: {error}") });
                return;
            }
        };
        dial(state, ctx, mail.addr, mail.name, consumer);
    }

    /// Dial `mail.addr` with the sender as the session's consumer, as
    /// [`Self::on_connect`] does with an explicit consumer.
    ///
    /// # Agent
    /// Reply: `ConnectResult`. `Err` when the mail carries no actor sender
    /// (a session has no inbox to deliver frames to), or on the errors
    /// `Connect` reports.
    #[handler::manual]
    fn on_connect_self(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, mail: ConnectSelf) {
        let Some(consumer) = ctx.sender() else {
            ctx.reply(&ConnectResult::Err {
                addr: mail.addr,
                error: "connect_self needs an actor sender to deliver frames to".to_owned(),
            });
            return;
        };
        dial(state, ctx, mail.addr, mail.name, Some(consumer));
    }

    /// Drain completed outbound dials and stage one `TcpSessionActor` per
    /// connected stream. The task completion sends each parked
    /// `ConnectResult` only after authoritative activation.
    #[handler::manual]
    fn on_connect_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, _mail: ConnectReady) {
        while let Ok((id, result)) = state.connect_rx.try_recv() {
            let Some(PendingConnect { owed, addr, name, consumer }) = state.pending_connects.remove(&id) else {
                continue;
            };

            match result {
                Ok(stream) => {
                    let session_name = name.unwrap_or_else(|| format!("conn-{id}"));
                    let peer = match stream.peer_addr() {
                        Ok(peer) => peer.to_string(),
                        Err(error) => {
                            drop(stream);
                            reply_to_pending_connect(
                                ctx,
                                owed,
                                &ConnectResult::Err { addr, error: format!("peer_addr failed: {error}") },
                            );
                            continue;
                        }
                    };
                    match ctx
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
                        .continue_from(
                            owed,
                            OutboundSessionSpawn { addr: addr.clone(), session_name: session_name.clone(), peer },
                        ) {
                        Ok(_) => {}
                        Err((error, owed)) => {
                            reply_to_pending_connect(
                                ctx,
                                owed,
                                &ConnectResult::Err { addr, error: format!("spawn failed: {error:?}") },
                            );
                        }
                    }
                }
                Err(error) => {
                    reply_to_pending_connect(ctx, owed, &ConnectResult::Err { addr, error });
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
    #[handler::manual]
    fn on_bind(_state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, mail: BindListener) {
        // ADR-0230: prove the consumer once, at receipt, before binding.
        let consumer = match mail.consumer.map(|position| ctx.resolve_live(position)).transpose() {
            Ok(consumer) => consumer,
            Err(error) => {
                ctx.reply(&BindListenerResult::Err { addr: mail.addr, error: format!("consumer not live: {error}") });
                return;
            }
        };
        bind_listener(ctx, mail.addr, mail.name, consumer);
    }

    /// Spawn a fresh `TcpListenerActor` bound to `mail.addr` whose consumer
    /// is the sender, as [`Self::on_bind`] does with an explicit consumer.
    ///
    /// # Agent
    /// Reply: `BindListenerResult`. `Err` when the mail carries no actor
    /// sender (a session has no inbox to deliver frames to), or on the
    /// errors `BindListener` reports.
    #[handler::manual]
    fn on_bind_self(_state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, Manual>, mail: BindListenerSelf) {
        let Some(consumer) = ctx.sender() else {
            ctx.reply(&BindListenerResult::Err {
                addr: mail.addr,
                error: "bind_listener_self needs an actor sender to deliver frames to".to_owned(),
            });
            return;
        };
        bind_listener(ctx, mail.addr, mail.name, Some(consumer));
    }

    #[handler(task)]
    fn on_session_spawn_done(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<TcpSessionActor>, OutboundSessionSpawn>,
    ) {
        let OutboundSessionSpawn { addr, session_name, peer } = done.context().clone();
        done.resolve_with(ctx, move |outcome, _| match &outcome.result {
            Ok(_) => ConnectResult::Ok { session_name, session_id: outcome.mailbox_id, peer },
            Err(error) => ConnectResult::Err { addr, error: format!("spawn failed: {error:?}") },
        });
    }

    #[handler(task)]
    fn on_listener_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<TcpListenerActor>, ListenerSpawn>,
    ) {
        let ListenerSpawn { addr, listener_name, local_port } = done.context().clone();
        let listener = match &done.output().result {
            Ok(listener) => *listener,
            Err(spawn_error) => {
                let error = format!("spawn failed: {spawn_error:?}");
                done.resolve_with(ctx, move |_, _| BindListenerResult::Err { addr, error });
                return;
            }
        };
        let monitor_handle = match ctx.monitor(listener.erase()) {
            Ok(handle) => handle,
            Err(monitor_error) => {
                ctx.send_to(listener, &Close::default());
                let error = format!("monitor failed: {monitor_error:?}");
                done.resolve_with(ctx, move |_, _| BindListenerResult::Err { addr, error });
                return;
            }
        };

        let listener_mailbox = done.output().mailbox_id;
        state.listeners.insert(
            listener.erase(),
            ListenerEntry {
                addr,
                port: local_port,
                name: listener_name.clone(),
                listener,
                pending_unbind: None,
                _monitor_handle: monitor_handle,
            },
        );
        done.resolve_with(ctx, move |_, _| BindListenerResult::Ok {
            listener_name,
            listener_id: listener_mailbox,
            local_port,
        });
    }

    /// Mail `Close` to the named listener and park the
    /// originator's reply target. Reply fires from
    /// `on_monitor_notice` once the listener tombstones.
    ///
    /// # Agent
    /// Reply: `UnbindListenerResult`. Asynchronous — the response
    /// fires after the listener's accept thread joins and its
    /// `MonitorNotice` arrives at this cap.
    #[handler::manual]
    fn on_unbind(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: UnbindListener) {
        // Resolve listener_id from the cap-local supervisor map by
        // name. The cap is the source of truth for "what listeners
        // exist"; no registry walk needed.
        let Some(entry) = state.listeners.values_mut().find(|entry| entry.name == mail.listener_name) else {
            ctx.reply(&UnbindListenerResult::Err {
                listener_name: mail.listener_name,
                error: "no such listener (or already closed)".into(),
            });
            return;
        };
        // Park the reply target on the entry. The cap's
        // already-registered monitor (set at spawn time) fires
        // MonitorNotice on close, which drives the reply. Preserve
        // the first caller when a duplicate request arrives while
        // that close is still in flight; replacing it would drop the
        // original settlement hold without ever replying.
        if entry.pending_unbind.is_some() {
            ctx.reply(&UnbindListenerResult::Err {
                listener_name: mail.listener_name,
                error: "unbind already in progress".into(),
            });
            return;
        }
        entry.pending_unbind = Some(PendingUnbind {
            sender: ctx.reply_target(),
            hold: ctx.acquire_settlement_hold(),
            listener_name: mail.listener_name,
        });
        let listener = entry.listener;
        // Mail Close through the reference the listener's spawn outcome
        // proved. ADR-0099 §3: the listener is a spawned child, so its id
        // is the lineage fold, not `hash(NAMESPACE:name)` — re-resolving by
        // name would reach a flat id nothing is registered under. The cap
        // kept the proof on the entry at spawn, so it sends through that.
        ctx.send_to(listener, &Close::default());
    }

    /// Walk the cap-local listener map and report metadata, ordered by
    /// listener name.
    ///
    /// # Agent
    /// Reply: `ListListenersResult`.
    #[handler::single]
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
    /// fire the parked unbind reply if one is waiting.
    ///
    /// The host stamps the closed listener as the notice's sender, so its
    /// entry is the one keyed by `ctx.sender()`. The cap's monitor on every
    /// spawned listener (registered in `on_bind`) fires this notice; if the
    /// close came from an unbind request, the entry's `pending_unbind` holds
    /// the originator to reply to.
    #[handler::manual]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, _notice: MonitorNotice) {
        // Drop the supervisor entry. The held MonitorHandle drops
        // here; deregister is idempotent with the close path's
        // forward-index drain.
        let Some(entry) = ctx.sender().and_then(|departed| state.listeners.remove(&departed)) else {
            return;
        };
        // Fire the parked unbind reply if one was waiting.
        if let Some(PendingUnbind { sender, hold, listener_name }) = entry.pending_unbind {
            let root = hold.as_ref().map_or(aether_data::MailId::NONE, SettlementHold::root);
            ctx.reply_to_target(sender, &UnbindListenerResult::Ok { listener_name }, root, None);
            drop(hold);
        }
        // Else: notice came from a non-unbind close (chassis
        // shutdown, future trap). Nothing to reply to; the
        // supervisor entry is gone, that's the cleanup.
    }
}
