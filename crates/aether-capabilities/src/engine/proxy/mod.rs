//! `aether.engine.proxy:<id>` — per-engine proxy actor (issue 763 P3).
//!
//! An instanced `NativeActor` that wraps one *outbound* RPC client
//! connection to a substrate. The forward-model architecture (issue
//! 763) makes every substrate an RPC server; the hub is the client.
//! Each substrate the hub talks to gets one proxy, addressed
//! `aether.engine.proxy:<engine-id>`.
//!
//! ## What it does
//!
//! - **`init`** dials the substrate's `RpcServerCapability` via
//!   `RpcClient::connect` and spawns the reader sidecar. The
//!   handshake's `HelloAck` identity is kept on `conn.server`.
//! - **`on_forward`** ([`ForwardEnvelope`]) wraps the `mailbox`,
//!   `kind`, and `payload` into an RPC `Call` and writes it down the
//!   connection. The inbound mail's `Source` is parked under the
//!   wire `cid` so the eventual reply can route back to the sender.
//! - **`on_inbound_ready`** ([`RpcInboundReady`]) is the reader
//!   sidecar's wake: it drains `conn.inbound`, lifting `ReplyEvent`
//!   frames back to the parked `Source` (correlation preserved,
//!   mirroring `Mailer::send_reply`), dropping the `in_flight` entry on
//!   `ReplyEnd`, and self-shutting-down on `Bye`.
//!
//! ## Scope (issue 763 P3 vs P4)
//!
//! P3 is the bridge core: connect, forward, route replies, lifecycle.
//! The engine-management surface — `describe_kinds` / `list` / `spawn`
//! / `terminate` and the hub RPC server's `engine = Some(_)` routing
//! that drives `ForwardEnvelope` at the proxy — lands in P4 with the
//! engines cap. The cached `HelloAck` manifest the describe handler
//! will read is already in hand on `conn.server`.
//!
//! Native-only: the state owns a `TcpStream` (via `RpcConnection`)
//! and an OS thread, so the substrate-typed runtime half lives behind
//! `feature = "runtime"` in the `runtime` module. The `#[actor]` macro divides the
//! identity from that runtime (ADR-0122): the [`EngineProxy`] ZST and its
//! addressing markers stay always-on so `aether-capabilities` still compiles
//! for `wasm32`, while the state, handlers, and `Drop` compile only under
//! `runtime`.

// Handler-signature kinds must be importable at file root — the
// `#[actor]` macro emits `impl HandlesKind<K>` markers always-on against
// the identity, outside the `feature = "runtime"` gate, so they reference
// these kinds from here.
use crate::engine::kinds::{EngineHeartbeatTick, ForwardEnvelope};
use aether_kinds::TerminateEngine;
// `RpcInboundReady` is owned by the RPC server cap (ADR-0121); the proxy
// shares the wake-mail kind. Imported at file root for the always-on
// `HandlesKind<RpcInboundReady>` marker.
use crate::rpc::RpcInboundReady;

// The proxy's implementation, split along its seams (ADR-0121):
// `config` (the init config + heartbeat tuning), `connect` (the
// startup-dial bring-up), `heartbeat` (the liveness-timer sidecar), and
// `sinks` (the test-only capture actors). All are native-only — the
// proxy owns a `TcpStream` and OS threads — so they elide on wasm
// alongside the runtime half.
#[cfg(not(target_arch = "wasm32"))]
mod config;
#[cfg(not(target_arch = "wasm32"))]
mod connect;
#[cfg(not(target_arch = "wasm32"))]
mod heartbeat;
#[cfg(test)]
mod sinks;

// `EngineProxyConfig` / `HeartbeatParams` carry only wasm-safe types,
// but the proxy that consumes them is native-only, so the re-export is
// gated like `TcpListenerConfig`.
#[cfg(not(target_arch = "wasm32"))]
pub use config::{EngineProxyConfig, HeartbeatParams};

/// `aether.engine.proxy:<id>` cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable` (`NAMESPACE`,
/// `Resolver`), the per-handler `HandlesKind` markers, and the instanced
/// name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`runtime::EngineProxyState`, which
/// holds the `aether_substrate`-typed RPC connection + the forked child +
/// heartbeat handle) lives behind the one `feature = "runtime"` gate, so a
/// transport-only build never names `EngineProxyState` nor pulls
/// `aether_substrate` through this cap.
pub struct EngineProxy;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, the connect/heartbeat helpers,
// `Drop` — lives in the `runtime` module below, gated once by
// `feature = "runtime"`; the `#[actor] impl` reaches all of it through the
// single `use runtime::*` glob. The handler-signature kinds
// (`ForwardEnvelope` / `RpcInboundReady` / …) stay always-on at file root —
// the always-on `HandlesKind<K>` markers name them.
use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the impl
// reaches all of it (state, ctx types, connect/heartbeat helpers) through this
// single seam, so the glob is intentional rather than a dozen one-line
// imports.
#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

#[actor(instanced)]
impl NativeActor for EngineProxy {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// per-engine outbound RPC connection plus the in-flight
    /// reply-correlation table.
    type State = EngineProxyState;
    type Config = EngineProxyConfig;
    const NAMESPACE: &'static str = "aether.engine.proxy";

    fn init(
        mut config: EngineProxyConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<EngineProxyState, BootError> {
        let self_mailbox = ctx.self_id();
        let mailer = ctx.mailer();
        let wake_kind = KindId(<RpcInboundReady as Kind>::ID.0);

        // A freshly-forked substrate (`spawned.is_some()`) may not
        // have bound its RPC port yet, so the startup dial retries
        // briefly. An adopted / externally-running substrate
        // (`spawned.is_none()`) is dialed once — a refused
        // connection there is a real error, not a startup race.
        let retry = config.spawned.is_some();
        let conn = match connect_proxy(
            &config.rpc_addr,
            &mailer,
            self_mailbox,
            wake_kind,
            retry,
            config.connect_budget,
        ) {
            Ok(conn) => conn,
            Err(e) => {
                // The proxy owns the child it was handed — a
                // failed boot must not orphan the substrate.
                if let Some(mut child) = config.spawned.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(BootError::Other(Box::new(e)));
            }
        };

        tracing::info!(
            target: "aether_substrate::engine_proxy",
            engine_id = ?config.engine_id,
            addr = %config.rpc_addr,
            spawned = config.spawned.is_some(),
            "engine proxy connected",
        );

        // Arm the liveness heartbeat, if configured. The sidecar
        // thread fires an `EngineHeartbeatTick` at this proxy's own
        // mailbox every `interval`; `on_heartbeat_tick` does the
        // ping + miss accounting on the dispatcher thread (so the
        // RPC write and all proxy state stay single-threaded).
        let (heartbeat, miss_limit) = match config.heartbeat {
            Some(params) if !params.interval.is_zero() && params.miss_limit > 0 => {
                let handle = spawn_heartbeat(Arc::clone(&mailer), self_mailbox, params.interval);
                (Some(handle), params.miss_limit)
            }
            _ => (None, 0),
        };

        Ok(EngineProxyState {
            engine_id: config.engine_id,
            mailer,
            conn,
            in_flight: HashMap::new(),
            spawned: config.spawned,
            missed_heartbeats: 0,
            miss_limit,
            heartbeat_seq: 0,
            _heartbeat: heartbeat,
        })
    }

    /// Relay one mail to the substrate as an RPC `Call`.
    ///
    /// # Agent
    /// Hand the proxy a `ForwardEnvelope { mailbox, kind, payload }`
    /// — the `mailbox` is the *substrate-local* recipient, `kind` +
    /// `payload` the mail to deliver there. Any reply routes back to
    /// the sender of this `ForwardEnvelope`.
    #[handler]
    fn on_forward(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: ForwardEnvelope) {
        let envelope = MailEnvelope {
            to: MailboxAddress::local(mail.mailbox),
            from: None,
            kind: mail.kind,
            correlation_id: None,
            payload: mail.payload,
        };
        match state.conn.client.call(envelope) {
            Ok(cid) => {
                state.in_flight.insert(cid, ctx.reply_target());
            }
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::engine_proxy",
                    engine_id = ?state.engine_id,
                    error = %e,
                    "engine proxy: Call write failed; dropping forward",
                );
            }
        }
    }

    /// Reader-sidecar wake. Drain every inbound frame.
    ///
    /// # Agent
    /// Internal wake mail — not part of the proxy's external
    /// surface. The reader thread fires this after pushing a frame;
    /// the handler drains `conn.inbound` and routes each frame.
    #[handler]
    fn on_inbound_ready(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: RpcInboundReady) {
        while let Ok(frame) = state.conn.inbound.try_recv() {
            match frame {
                WireFrame::ReplyEvent { cid, envelope } => state.route_reply(cid, envelope),
                WireFrame::ReplyEnd { cid, result } => state.route_settled(cid, result),
                // A `Pong` answers this proxy's heartbeat `Ping`
                // (issue 1339): the substrate is alive. Clear the
                // miss counter and report the liveness up to the
                // engines cap so `list_engines` can show a fresh
                // heartbeat age. The nonce is for log correlation
                // only — any `Pong` is a liveness signal.
                WireFrame::Pong(_nonce) => {
                    state.missed_heartbeats = 0;
                    state.report_alive(ctx);
                }
                WireFrame::Bye { reason } => {
                    tracing::info!(
                        target: "aether_substrate::engine_proxy",
                        engine_id = ?state.engine_id,
                        reason = %reason,
                        "engine proxy: substrate closed the connection; shutting down",
                    );
                    // Tell the engines cap the engine is gone so it
                    // drops the registry entry — without this the
                    // proxy dies but `list_engines` keeps reporting
                    // a corpse (issue 1339). The substrate closed the
                    // connection on its own — a crash, not a
                    // deliberate terminate; carry the `Bye` reason so
                    // `list_engines` can show why.
                    state.report_died(ctx, DeathReason::Crashed { detail: reason });
                    ctx.shutdown();
                    return;
                }
                // Hello / HelloAck / Call / Ping: a client-side proxy
                // never expects these inbound. Drop with a debug
                // line rather than warn-storming.
                other => {
                    tracing::debug!(
                        target: "aether_substrate::engine_proxy",
                        engine_id = ?state.engine_id,
                        frame = ?other,
                        "engine proxy: unexpected inbound frame; ignoring",
                    );
                }
            }
        }
    }

    /// Shut this proxy's substrate down.
    ///
    /// # Agent
    /// Sent by the engines cap (`aether.engine`) on a terminate
    /// request. The proxy self-shuts-down; its `Drop` SIGKILLs and
    /// reaps the child substrate it forked (if any), and the
    /// outbound RPC connection closes as the actor drops. The
    /// `engine_id` field is ignored — a proxy only ever terminates
    /// its own engine.
    #[handler]
    fn on_terminate(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: TerminateEngine) {
        tracing::info!(
            target: "aether_substrate::engine_proxy",
            engine_id = ?state.engine_id,
            "engine proxy: terminate requested; shutting down",
        );
        // No `report_died` here: the engines cap initiated this
        // terminate and already dropped the registry entry, so the
        // proxy reporting back would be a redundant (idempotent)
        // no-op. The self-death paths (`Bye`, heartbeat timeout) are
        // the ones the cap doesn't already know about.
        ctx.shutdown();
    }

    /// Liveness-heartbeat timer wake (issue 1339).
    ///
    /// # Agent
    /// Internal wake mail — not part of the proxy's external
    /// surface. The heartbeat sidecar thread fires this every
    /// interval. The handler counts the tick as an outstanding miss
    /// (a `Pong` since the last tick would have cleared the
    /// counter), and once `miss_limit` consecutive ticks go
    /// unanswered it declares the engine dead: reports `EngineDied`
    /// to the engines cap and self-shuts-down (its `Drop` SIGKILLs
    /// the wedged child). Otherwise it sends a fresh `Ping`.
    #[handler]
    fn on_heartbeat_tick(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _mail: EngineHeartbeatTick,
    ) {
        state.heartbeat_seq += 1;
        // A write failure means the socket is already broken — the
        // reader sidecar will surface a `Bye` and `on_inbound_ready`
        // handles the eviction. Count it as a miss and carry on so
        // the miss-limit path also covers it (whichever fires first
        // evicts; the cap side is idempotent).
        if let Err(e) = state.conn.client.ping(state.heartbeat_seq) {
            tracing::debug!(
                target: "aether_substrate::engine_proxy",
                engine_id = ?state.engine_id,
                error = %e,
                "engine proxy: heartbeat ping write failed",
            );
        }
        state.missed_heartbeats += 1;
        if state.missed_heartbeats >= state.miss_limit {
            tracing::warn!(
                target: "aether_substrate::engine_proxy",
                engine_id = ?state.engine_id,
                missed = state.missed_heartbeats,
                miss_limit = state.miss_limit,
                "engine proxy: heartbeat miss limit crossed; evicting engine",
            );
            state.report_died(
                ctx,
                DeathReason::Evicted {
                    detail: format!(
                        "heartbeat miss limit {} of {}",
                        state.missed_heartbeats, state.miss_limit
                    ),
                },
            );
            ctx.shutdown();
        }
    }
}

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `EngineProxyState`, its `Drop` + helper methods, `engine_cap_mailbox`) —
// lives in `runtime.rs`, gated once here. The `#[actor] impl` above reaches it
// through the `use runtime::*` glob.
#[cfg(feature = "runtime")]
mod runtime;

#[cfg(test)]
use aether_kinds::DeathReason;
#[cfg(test)]
use sinks::{EngineCapCells, EngineCapSink, ProxyReplySink};

#[cfg(test)]
mod tests {
    // Test harness resolves echo/sink actor mailboxes by their NAMESPACE for
    // fixture wiring — reference id derivation, not sibling-cap addressing.
    #![allow(clippy::disallowed_methods)]
    use super::{
        DeathReason, EngineCapCells, EngineCapSink, EngineProxy, EngineProxyConfig,
        HeartbeatParams, ProxyReplySink,
    };
    use crate::engine::kinds::ForwardEnvelope;
    use crate::rpc::server::{RpcServerCapability, RpcServerConfig, RpcServerHandle};
    use crate::rpc::test_echo::{TestEchoActor, TestEchoRequest};
    use crate::rpc::{HelloAck, PeerKind, WIRE_VERSION, WireFrame};
    use crate::test_chassis::{TestChassis, fresh_substrate};
    use crate::trace::TraceDispatchCapability;
    use aether_actor::Addressable;
    use aether_codec::frame::{read_frame, write_frame};
    use aether_data::{EngineId, Kind, Uuid, mailbox_id_from_name};
    use aether_substrate::Subname;
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::mail::{Mail, Source, SourceAddr};
    use std::io::BufReader;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn substrate_peer_kind() -> PeerKind {
        PeerKind::Substrate {
            engine_name: "test".into(),
            engine_version: "0.1.0".into(),
            kinds: vec![],
        }
    }

    /// Full bridge round-trip: boot an RPC server + the echo actor + a
    /// reply sink, spawn an `EngineProxy` pointed at the server's port,
    /// forge a `ForwardEnvelope` at the proxy with the sink as
    /// reply-to, and observe the echoed value land on the sink — proof
    /// the proxy forwards as a `Call` and routes the `ReplyEvent` back
    /// to the original sender.
    #[test]
    fn forward_round_trips_reply_back_to_sender() {
        let (registry, mailer) = fresh_substrate();
        let recorded: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            // TraceObserver produces the `Settled` mail RpcServer's
            // settlement subscription waits on; without it the `Call`
            // never closes with a `ReplyEnd`.
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<TestEchoActor>(())
            .with_actor::<ProxyReplySink>(Arc::clone(&recorded))
            .with_actor::<RpcServerCapability>(RpcServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                peer_kind: substrate_peer_kind(),
            })
            .build_passive()
            .expect("caps boot");

        let port = chassis
            .handle::<RpcServerHandle>()
            .expect("RpcServerHandle published")
            .local_port;

        // Spawn the proxy, dialing this chassis's own RPC server over
        // loopback. A successful `finish()` means `init` connected +
        // handshook.
        chassis
            .spawn_actor::<EngineProxy>(
                Subname::Named("e1"),
                EngineProxyConfig {
                    engine_id: EngineId(Uuid::from_u128(1)),
                    rpc_addr: format!("127.0.0.1:{port}"),
                    spawned: None,
                    heartbeat: None,
                    // Adopted substrate (`spawned: None`) is dialed once,
                    // so the connect budget is inert here.
                    connect_budget: None,
                },
            )
            .finish()
            .expect("proxy spawns + connects");

        let proxy_mailbox = chassis
            .resolve_actor::<EngineProxy>("e1")
            .expect("proxy resolves Live");
        let echo_mailbox = mailbox_id_from_name(<TestEchoActor as Addressable>::NAMESPACE);
        let sink_mailbox = mailbox_id_from_name(<ProxyReplySink as Addressable>::NAMESPACE);

        // Forge a `ForwardEnvelope` at the proxy, reply-to the sink.
        // `mailer.push` directly (rather than through an actor send) so
        // the test controls the `Source` the proxy parks.
        let fwd = ForwardEnvelope {
            mailbox: echo_mailbox,
            kind: <TestEchoRequest as Kind>::ID,
            payload: TestEchoRequest { value: 42 }.encode_into_bytes(),
        };
        mailer.push(
            Mail::new(
                proxy_mailbox,
                <ForwardEnvelope as Kind>::ID,
                fwd.encode_into_bytes(),
                1,
            )
            .with_reply_to(Source::with_correlation(
                SourceAddr::Component(sink_mailbox),
                777,
            )),
        );

        // Poll for the sink to record the echoed value. The round trip
        // is proxy → server (TCP) → echo → server → proxy (TCP) → sink,
        // all across dispatcher threads — give it a generous deadline.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = *recorded
                .lock()
                .expect("test setup: recorded mutex poisoned");
            if let Some(value) = snapshot {
                assert_eq!(value, 42, "echoed value routed back through the proxy");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "reply did not route back through the proxy within 5s",
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Spawning a proxy at an address with no RPC server fails at
    /// `init` (the dial errors), surfacing as a spawn `finish()` error
    /// rather than a live-but-dead proxy.
    #[test]
    fn proxy_spawn_fails_when_substrate_unreachable() {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .build_passive()
            .expect("empty chassis boots");

        // Bind then drop a listener to get a definitely-closed port.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let result = chassis
            .spawn_actor::<EngineProxy>(
                Subname::Named("dead"),
                EngineProxyConfig {
                    engine_id: EngineId(Uuid::from_u128(2)),
                    rpc_addr: format!("127.0.0.1:{port}"),
                    spawned: None,
                    heartbeat: None,
                    connect_budget: None,
                },
            )
            .finish();
        assert!(
            result.is_err(),
            "spawning a proxy at a closed port should fail at init",
        );
    }

    /// How a [`fake_server`] treats the proxy's heartbeat pings after
    /// the handshake.
    #[derive(Clone, Copy)]
    enum Behavior {
        /// Mirror every `Ping(n)` back as `Pong(n)` — a healthy engine.
        Pong,
        /// Read and drop pings without answering — a wedged engine.
        Ignore,
        /// Drop the connection right after the handshake — the
        /// connection-close (`Bye`) eviction path.
        Close,
    }

    /// Spin a one-shot fake substrate RPC server on an OS-picked port:
    /// accept one connection, run the `Hello` / `HelloAck` handshake,
    /// then behave per `behavior`. Returns the port and the server
    /// thread handle (detached — it exits when the proxy disconnects on
    /// test teardown).
    fn fake_server(behavior: Behavior) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let port = listener.local_addr().expect("local_addr").port();
        // Test-only fake substrate server thread (infra, no mail layer).
        #[allow(clippy::disallowed_methods)]
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("fake server accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut writer = stream;
            let _hello: WireFrame = read_frame(&mut reader).expect("read Hello");
            write_frame(
                &mut writer,
                &WireFrame::HelloAck(HelloAck {
                    wire_version: WIRE_VERSION,
                    server: substrate_peer_kind(),
                }),
            )
            .expect("write HelloAck");
            if matches!(behavior, Behavior::Close) {
                return; // drop the stream → the proxy reads eof → Bye
            }
            // Service pings until the proxy hangs up (read error ends
            // the `while let`).
            while let Ok::<WireFrame, _>(frame) = read_frame(&mut reader) {
                if let (WireFrame::Ping(n), Behavior::Pong) = (&frame, behavior)
                    && write_frame(&mut writer, &WireFrame::Pong(*n)).is_err()
                {
                    break;
                }
            }
        });
        (port, handle)
    }

    /// Boot a chassis hosting the engine-cap sink, point an
    /// `EngineProxy` (with the given heartbeat) at `port`, and return
    /// the chassis (kept alive for its dispatcher threads) + the sink
    /// cells. `engine_id` is `Uuid::from_u128(seed)`.
    fn spawn_proxy_with_heartbeat(
        seed: u128,
        port: u16,
        heartbeat: Option<HeartbeatParams>,
    ) -> (PassiveChassis<TestChassis>, EngineCapCells, String) {
        let (registry, mailer) = fresh_substrate();
        let cells = EngineCapCells::default();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<EngineCapSink>(cells.clone())
            .build_passive()
            .expect("caps boot");
        let engine_id = EngineId(Uuid::from_u128(seed));
        chassis
            .spawn_actor::<EngineProxy>(
                Subname::Named("e"),
                EngineProxyConfig {
                    engine_id,
                    rpc_addr: format!("127.0.0.1:{port}"),
                    spawned: None,
                    heartbeat,
                    connect_budget: None,
                },
            )
            .finish()
            .expect("proxy connects");
        (chassis, cells, engine_id.0.to_string())
    }

    /// Block until `cell` holds at least one entry (returning a clone of
    /// the first), or the deadline passes (panicking with `what`).
    fn await_first<T: Clone>(cell: &Arc<Mutex<Vec<T>>>, what: &str) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Clone out under the guard, then drop it before the branch
            // (clippy `significant_drop_in_scrutinee`).
            let first = cell
                .lock()
                .expect("test setup: cell mutex poisoned")
                .first()
                .cloned();
            if let Some(first) = first {
                return first;
            }
            assert!(Instant::now() < deadline, "{what} within 5s");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A wedged engine (handshakes, then never answers a heartbeat
    /// `Ping`) is evicted: after `miss_limit` missed pongs the proxy
    /// reports `EngineDied` to the engines cap. This is the wedge case
    /// the lazy connection-drop path misses.
    #[test]
    fn heartbeat_evicts_engine_after_missed_pongs() {
        let (port, _server) = fake_server(Behavior::Ignore);
        let (_chassis, cells, engine_id) = spawn_proxy_with_heartbeat(
            42,
            port,
            Some(HeartbeatParams {
                interval: Duration::from_millis(40),
                miss_limit: 3,
            }),
        );
        let died = await_first(&cells.died, "wedged engine not evicted");
        assert_eq!(
            died.engine_id, engine_id,
            "the wedged engine's id is reported dead"
        );
        assert!(
            matches!(died.reason, DeathReason::Evicted { .. }),
            "a heartbeat-evicted engine is reported Evicted, got {:?}",
            died.reason,
        );
    }

    /// A healthy engine (pongs every heartbeat) is reported alive and
    /// never evicted.
    #[test]
    fn heartbeat_reports_alive_on_pong() {
        let (port, _server) = fake_server(Behavior::Pong);
        let (_chassis, cells, engine_id) = spawn_proxy_with_heartbeat(
            7,
            port,
            Some(HeartbeatParams {
                interval: Duration::from_millis(40),
                miss_limit: 3,
            }),
        );
        let alive = await_first(&cells.alive, "healthy engine never reported alive");
        assert_eq!(
            alive, engine_id,
            "the healthy engine's id is reported alive"
        );
        // Give the miss-limit window a chance to (wrongly) fire, then
        // confirm a ponging engine is never declared dead.
        thread::sleep(Duration::from_millis(200));
        assert!(
            cells
                .died
                .lock()
                .expect("test setup: died cell mutex poisoned")
                .is_empty(),
            "a ponging engine must not be evicted",
        );
    }

    /// A proxy whose substrate closes the connection reports
    /// `EngineDied` so the cap drops the registry entry — the reactive
    /// path that, before issue 1339, left `list_engines` reporting a
    /// corpse. No heartbeat needed; the `Bye` drives it.
    #[test]
    fn proxy_reports_died_when_connection_closes() {
        let (port, _server) = fake_server(Behavior::Close);
        let (_chassis, cells, engine_id) = spawn_proxy_with_heartbeat(99, port, None);
        let died = await_first(&cells.died, "closed engine not reported dead");
        assert_eq!(
            died.engine_id, engine_id,
            "the closed engine's id is reported dead"
        );
        assert!(
            matches!(died.reason, DeathReason::Crashed { .. }),
            "a connection-close eviction is reported Crashed, got {:?}",
            died.reason,
        );
    }
}
