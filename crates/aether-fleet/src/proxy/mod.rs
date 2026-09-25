//! `aether.fleet.proxy:<id>` — per-engine proxy actor (issue 763 P3).
//!
//! An instanced `NativeActor` that wraps one *outbound* RPC client
//! connection to a substrate. The forward-model architecture (issue
//! 763) makes every substrate an RPC server; the hub is the client.
//! Each substrate the hub talks to gets one proxy, addressed
//! `aether.fleet.proxy:<engine-id>`.
//!
//! ## What it does
//!
//! - **`init`** dials the substrate's `RpcServerCapability` via
//!   `RpcClient::connect_fail_fast`, which spawns the reader as the
//!   proxy's own sidecar, so a reader panic stops the chassis (ADR-0063).
//!   The handshake's `HelloAck` identity is kept on `conn.server`.
//! - **`wire`** registers the proxy with the hub's RPC server as its
//!   engine's route (`RegisterEngineRoute`), holding the spawn chain open
//!   until **`on_route_registered`** takes the answer.
//! - **`on_forward`** ([`ForwardEnvelope`](aether_rpc::ForwardEnvelope)) wraps the `recipient`
//!   path, `kind`, and `payload` into an RPC `Call` and writes it down the
//!   connection; the substrate resolves the path on arrival. The reply owed
//!   to the sender is parked under the wire `cid` as a `DeferredReply`,
//!   which holds the forward's chain open until the call ends.
//! - **`on_inbound_ready`** ([`RpcInboundReady`](aether_rpc::RpcInboundReady)) is the reader
//!   sidecar's wake: it drains `conn.inbound`, relaying each `ReplyEvent`
//!   through the parked debt's `reply_envelope` (correlation preserved),
//!   answering the debt with a `CallSettled` on `ReplyEnd`, and
//!   self-shutting-down on `Bye`.
//! - **`unwire`** abandons every debt still parked when the proxy closes,
//!   and the hub's RPC server closes the wire calls behind them.
//!
//! ## Scope (issue 763 P3 vs P4)
//!
//! P3 is the bridge core: connect, forward, route replies, lifecycle.
//! The engine-management surface — `describe_kinds` / `list` / `spawn`
//! / `terminate` — lands in P4 with the engines cap. The hub RPC server
//! drives `ForwardEnvelope` at the proxy for `engine = Some(_)` calls
//! once the proxy has registered its engine's route. The cached
//! `HelloAck` manifest the describe handler will read is already in hand
//! on `conn.server`.
//!
//! Native-only: the state owns a `TcpStream` (via `RpcConnection`)
//! and an OS thread, so the substrate-typed runtime half lives in the
//! `runtime` module. The `#[actor]` macro divides the
//! identity from that runtime (ADR-0122): the [`FleetProxy`] ZST and its
//! addressing markers stay in the identity file, while the state, handlers,
//! and `Drop` live behind
//! `runtime`.

// The proxy's implementation, split along its seams (ADR-0121):
// `config` (the init config + heartbeat tuning), `connect` (the
// startup-dial bring-up), `heartbeat` (the liveness-timer sidecar), and
// `sinks` (the test-only capture actors). All are native-only — the
// proxy owns a `TcpStream` and OS threads — so they elide on wasm
// alongside the runtime half.
#[cfg(not(target_family = "wasm"))]
mod config;
#[cfg(not(target_family = "wasm"))]
mod connect;
#[cfg(not(target_family = "wasm"))]
mod heartbeat;
#[cfg(not(target_family = "wasm"))]
mod reap;
#[cfg(test)]
mod sinks;

// `FleetProxyConfig` / `HeartbeatParams` carry only wasm-safe types,
// but the proxy that consumes them is native-only, so the re-export is
// gated like `TcpListenerConfig`. `FleetProxyConfig` rides `not(wasm)`
// because it re-exports on up to the crate root for chassis builders;
// `HeartbeatParams` has no consumer outside the `runtime` half (the
// `connect` / `heartbeat` modules and `server::runtime`), so it rides the
// `runtime` gate to stay off a marker-only host build.
#[cfg(not(target_family = "wasm"))]
pub use config::HeartbeatParams;
#[cfg(not(target_family = "wasm"))]
pub use config::{FleetProxyConfig, ProxyTarget};

// A failed spawn's detail names the startup exit it saw, and a fork whose
// stderr capture cannot start is torn down the way a failed proxy init
// tears down its child. Only the engines cap reads them; `proxy` is a
// private module, so they reach no further than this crate.
#[cfg(not(target_family = "wasm"))]
pub use connect::{describe_exit, read_reported_port, startup_exit_status};
#[cfg(not(target_family = "wasm"))]
pub use reap::terminate_child_group;

/// `aether.fleet.proxy:<id>` cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable` (`NAMESPACE`,
/// `Resolver`), the per-handler `HandlesKind` markers, and the instanced
/// name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`runtime::FleetProxyState`, which
/// holds the `aether_substrate`-typed RPC connection + the forked child +
/// heartbeat handle) lives in `runtime.rs`, so the identity file never names
/// `FleetProxyState`.
///
/// It depends on the hub's
/// [`RpcServerCapability`](aether_rpc::RpcServerCapability): the proxy
/// registers its engine's route there from `wire` and holds its spawn open
/// until the route is answered, so a chassis with no RPC server refuses the
/// proxy before `init` rather than leaving that spawn unsettled. It also
/// depends on the [`FleetServer`](crate::FleetServer), because it reports its engine's liveness
/// there.
#[actor(instanced, child_of(FleetServer), depends(RpcServerCapability, FleetServer))]
pub struct FleetProxy;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state, the connect/heartbeat helpers,
// `Drop` — lives in the `runtime` module below; the struct-hosted `#[actor]`
// reads that module's
// `impl NativeActor` off disk to emit the identity. The handler-signature
// kinds (`ForwardEnvelope` / `RpcInboundReady` / …) stay always-on at file
// root — the always-on `HandlesKind<K>` markers name them.
use aether_actor::actor;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `FleetProxyState`, its `Drop` + helper methods) plus
// the `#[runtime] impl NativeActor` — lives in `runtime.rs`, gated once here.
// The struct-hosted `#[actor]` above reads it off disk to emit the identity.
mod runtime;

#[cfg(test)]
use aether_kinds::DeathReason;
#[cfg(test)]
use sinks::{FleetCapCells, FleetCapSink, ProxyReplySink, RecordedReply, ReplyLog};

#[cfg(test)]
mod tests {
    // The bridge test deliberately builds a bare test chassis with
    // `Builder::new` rather than the based boot path.
    #![allow(clippy::disallowed_methods)]
    use super::{
        DeathReason, FleetCapCells, FleetCapSink, FleetProxy, FleetProxyConfig, HeartbeatParams, ProxyReplySink,
        ProxyTarget, RecordedReply, ReplyLog,
    };
    use aether_actor::{ActorRef, Addressable};
    use aether_codec::frame::{max_frame_size, read_frame, write_frame};
    use aether_data::{ActorPath, EngineId, Kind, Uuid};
    use aether_kinds::TerminateEngine;
    use aether_rpc::server::test_echo::{TestEchoActor, TestEchoReply, TestEchoRequest};
    use aether_rpc::server::{RpcBind, RpcServerCapability, RpcServerConfig, RpcServerHandle, RpcServerParams};
    use aether_rpc::{
        ForwardEnvelope, HelloAck, MailEnvelope, PeerKind, Recipient, ReplyEnvelope, RpcClient, RpcError, WIRE_VERSION,
        WireFrame,
    };
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::testing::{TestChassis, fresh_substrate};
    use aether_substrate::{ReplyTarget, Subname};
    use aether_trace::TraceDispatchCapability;
    use std::collections::VecDeque;
    use std::io::BufReader;
    use std::net::TcpListener;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn substrate_peer_kind() -> PeerKind {
        PeerKind::Substrate { engine_name: "test".into(), engine_version: "0.1.0".into(), kinds: vec![] }
    }

    /// Params for the unbound RPC server a test chassis composes only
    /// because the proxy declares it as a dependency.
    fn unbound_rpc_params() -> RpcServerParams {
        RpcServerParams { peer_kind: substrate_peer_kind(), bind: RpcBind::Boot }
    }

    /// Full bridge round-trip: boot an RPC server + the echo actor + a
    /// reply sink, spawn an `FleetProxy` pointed at the server's port,
    /// forge a `ForwardEnvelope` at the proxy with the sink as
    /// reply-to, and observe the echoed value land on the sink — proof
    /// the proxy forwards as a `Call` and routes the `ReplyEvent` back
    /// to the original sender.
    #[test]
    fn forward_round_trips_reply_back_to_sender() {
        let (registry, mailer) = fresh_substrate();
        let log = ReplyLog::default();

        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            // TraceObserver produces the `Settled` mail RpcServer's
            // settlement subscription waits on; without it the `Call`
            // never closes with a `ReplyEnd`.
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<TestEchoActor>(())
            .with_actor::<ProxyReplySink>(Arc::clone(&log))
            .with_actor::<FleetCapSink>(FleetCapCells::default())
            .with_actor_configured::<RpcServerCapability>(
                RpcServerParams { peer_kind: substrate_peer_kind(), bind: RpcBind::Boot },
                RpcServerConfig { port: Some(0), port_file: None },
            )
            .build_passive()
            .expect("caps boot");

        let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;

        // Spawn the proxy, dialing this chassis's own RPC server over
        // loopback. A successful `finish()` means `init` connected +
        // handshook. Production places the proxy under `aether.fleet`
        // (`spawn_child::<FleetServer, FleetProxy>`); this test drives the
        // bridge with no engines cap in the picture, so it borrows the
        // test-support parentless placement rather than widening the proxy's
        // shipped ADR-0166 permissions to `root`.
        let proxy = chassis
            .spawn_actor_for_test::<FleetProxy>(
                Subname::Named("e1"),
                FleetProxyConfig {
                    engine_id: EngineId(Uuid::from_u128(1)),
                    target: ProxyTarget::Adopted { rpc_addr: format!("127.0.0.1:{port}") },
                    heartbeat: None,
                    // An adopted substrate is dialed once, so the
                    // connect budget is inert here.
                    connect_budget: None,
                },
                (),
            )
            .finish()
            .expect("proxy spawns + connects");

        // Forge a `ForwardEnvelope` at the proxy, reply-to the sink.
        // Pushed from the embedder (rather than through an actor send) so
        // the test controls the reply target the proxy parks.
        let fwd = ForwardEnvelope {
            recipient: ActorPath::new(<TestEchoActor as Addressable>::NAMESPACE).expect("the echo namespace is a path"),
            kind: <TestEchoRequest as Kind>::ID,
            payload: TestEchoRequest { value: 42 }.encode_into_bytes(),
        };
        chassis.send_for_reply(
            proxy.erase(),
            <ForwardEnvelope as Kind>::ID,
            fwd.encode_into_bytes(),
            ReplyTarget::Actor { to: chassis.actor_ref::<ProxyReplySink>().erase(), correlation: 777 },
        );

        // Poll for the sink to record the echoed value. The round trip
        // is proxy → server (TCP) → echo → server → proxy (TCP) → sink,
        // all across dispatcher threads — give it a generous deadline.
        let first = await_first(&log, "reply did not route back through the proxy");
        assert_eq!(first, RecordedReply::Echo(42), "echoed value routed back through the proxy");
    }

    /// Spawning a proxy at an address with no RPC server fails at
    /// `init` (the dial errors), surfacing as a spawn `finish()` error
    /// rather than a live-but-dead proxy.
    #[test]
    fn proxy_spawn_fails_when_substrate_unreachable() {
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<FleetCapSink>(FleetCapCells::default())
            .with_actor_configured::<RpcServerCapability>(
                unbound_rpc_params(),
                RpcServerConfig { port: None, port_file: None },
            )
            .build_passive()
            .expect("chassis carrying the proxy's dependencies boots");

        // Bind then drop a listener to get a definitely-closed port.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let result = chassis
            .spawn_actor_for_test::<FleetProxy>(
                Subname::Named("dead"),
                FleetProxyConfig {
                    engine_id: EngineId(Uuid::from_u128(2)),
                    target: ProxyTarget::Adopted { rpc_addr: format!("127.0.0.1:{port}") },
                    heartbeat: None,
                    connect_budget: None,
                },
                (),
            )
            .finish();
        assert!(result.is_err(), "spawning a proxy at a closed port should fail at init");
    }

    /// How a [`fake_server`] behaves after the handshake.
    enum Behavior {
        /// Mirror every `Ping(n)` back as `Pong(n)` — a healthy engine.
        Pong,
        /// Read and drop pings without answering — a wedged engine.
        Ignore,
        /// Drop the connection right after the handshake — the
        /// connection-close (`Bye`) eviction path.
        Close,
        /// Report each `Call`'s `cid` on `calls` and answer it with the
        /// next script in `scripts`; a call with no script left is never
        /// answered.
        Scripted { calls: mpsc::Sender<u64>, scripts: VecDeque<CallScript> },
    }

    /// One frame a scripted [`fake_server`] writes back for a call, under
    /// that call's `cid`.
    #[derive(Clone, Copy)]
    enum ScriptFrame {
        /// A `ReplyEvent` carrying a [`TestEchoReply`] of this value.
        Echo(u64),
        /// A successful `ReplyEnd`.
        End,
    }

    /// The frames a scripted [`fake_server`] answers one call with.
    struct CallScript {
        frames: Vec<ScriptFrame>,
        /// When present, the frames wait until this fires.
        release: Option<mpsc::Receiver<()>>,
    }

    impl ScriptFrame {
        fn under(self, cid: u64) -> WireFrame {
            match self {
                Self::Echo(value) => WireFrame::ReplyEvent {
                    cid,
                    envelope: ReplyEnvelope {
                        kind: <TestEchoReply as Kind>::ID,
                        payload: TestEchoReply { value }.encode_into_bytes(),
                    },
                },
                Self::End => WireFrame::ReplyEnd { cid, result: Ok(()) },
            }
        }
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
                &WireFrame::HelloAck(HelloAck { wire_version: WIRE_VERSION, server: substrate_peer_kind() }),
            )
            .expect("write HelloAck");
            let mut behavior = behavior;
            if matches!(behavior, Behavior::Close) {
                return; // drop the stream → the proxy reads eof → Bye
            }
            // Service frames until the proxy hangs up (read error ends
            // the `while let`).
            while let Ok::<WireFrame, _>(frame) = read_frame(&mut reader) {
                match (&frame, &mut behavior) {
                    (WireFrame::Ping(n), Behavior::Pong) => {
                        if write_frame(&mut writer, &WireFrame::Pong(*n)).is_err() {
                            break;
                        }
                    }
                    (WireFrame::Call { cid: Some(cid), .. }, Behavior::Scripted { calls, scripts }) => {
                        let _ = calls.send(*cid);
                        let Some(script) = scripts.pop_front() else {
                            continue;
                        };
                        if let Some(release) = script.release {
                            let _ = release.recv();
                        }
                        for frame in script.frames {
                            if write_frame(&mut writer, &frame.under(*cid)).is_err() {
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
        (port, handle)
    }

    /// Boot a chassis hosting the engine-cap sink, point an
    /// `FleetProxy` (with the given heartbeat) at `port`, and return
    /// the chassis (kept alive for its dispatcher threads) + the sink
    /// cells. `engine_id` is `Uuid::from_u128(seed)`.
    fn spawn_proxy_with_heartbeat(
        seed: u128,
        port: u16,
        heartbeat: Option<HeartbeatParams>,
    ) -> (PassiveChassis<TestChassis>, FleetCapCells, String) {
        let (registry, mailer) = fresh_substrate();
        let cells = FleetCapCells::default();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<FleetCapSink>(cells.clone())
            .with_actor_configured::<RpcServerCapability>(
                unbound_rpc_params(),
                RpcServerConfig { port: None, port_file: None },
            )
            .build_passive()
            .expect("caps boot");
        let engine_id = EngineId(Uuid::from_u128(seed));
        chassis
            .spawn_actor_for_test::<FleetProxy>(
                Subname::Named("e"),
                FleetProxyConfig {
                    engine_id,
                    target: ProxyTarget::Adopted { rpc_addr: format!("127.0.0.1:{port}") },
                    heartbeat,
                    connect_budget: None,
                },
                (),
            )
            .finish()
            .expect("proxy connects");
        (chassis, cells, engine_id.0.to_string())
    }

    /// Boot a chassis hosting trace dispatch, the engine-cap sink, a reply
    /// sink, and the unbound RPC server, then point an [`FleetProxy`] named
    /// `subname` at the fake engine on `port` with no heartbeat. Returns the
    /// chassis (kept alive for its dispatcher threads), the proxy, and the
    /// sink's reply log. `engine_id` is `Uuid::from_u128(seed)`.
    fn spawn_scripted_proxy(
        seed: u128,
        subname: &'static str,
        port: u16,
    ) -> (PassiveChassis<TestChassis>, ActorRef<FleetProxy>, ReplyLog) {
        let (registry, mailer) = fresh_substrate();
        let log = ReplyLog::default();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<FleetCapSink>(FleetCapCells::default())
            .with_actor::<ProxyReplySink>(Arc::clone(&log))
            .with_actor_configured::<RpcServerCapability>(
                unbound_rpc_params(),
                RpcServerConfig { port: None, port_file: None },
            )
            .build_passive()
            .expect("caps boot");
        let proxy = chassis
            .spawn_actor_for_test::<FleetProxy>(
                Subname::Named(subname),
                FleetProxyConfig {
                    engine_id: EngineId(Uuid::from_u128(seed)),
                    target: ProxyTarget::Adopted { rpc_addr: format!("127.0.0.1:{port}") },
                    heartbeat: None,
                    connect_budget: None,
                },
                (),
            )
            .finish()
            .expect("proxy spawns + connects");
        (chassis, proxy, log)
    }

    /// Block until `cell` holds at least one entry (returning a clone of
    /// the first), or the deadline passes (panicking with `what`).
    fn await_first<T: Clone>(cell: &Arc<Mutex<Vec<T>>>, what: &str) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Clone out under the guard, then drop it before the branch
            // (clippy `significant_drop_in_scrutinee`).
            let first = cell.lock().expect("test setup: cell mutex poisoned").first().cloned();
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
            Some(HeartbeatParams { interval: Duration::from_millis(40), miss_limit: 3 }),
        );
        let died = await_first(&cells.died, "wedged engine not evicted");
        assert_eq!(died.engine_id, engine_id, "the wedged engine's id is reported dead");
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
            Some(HeartbeatParams { interval: Duration::from_millis(40), miss_limit: 3 }),
        );
        let alive = await_first(&cells.alive, "healthy engine never reported alive");
        assert_eq!(alive, engine_id, "the healthy engine's id is reported alive");
        // Give the miss-limit window a chance to (wrongly) fire, then
        // confirm a ponging engine is never declared dead.
        thread::sleep(Duration::from_millis(200));
        assert!(
            cells.died.lock().expect("test setup: died cell mutex poisoned").is_empty(),
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
        // Hold `init` until the reader has enqueued its synthetic `Bye`
        // and fired the still-pre-registration wake. The post-registration
        // catch-up wake must recover that deliberately lost first wake.
        super::connect::wait_for_reader_wake_before_connect_returns();
        let (_chassis, cells, engine_id) = spawn_proxy_with_heartbeat(99, port, None);
        let died = await_first(&cells.died, "closed engine not reported dead");
        assert_eq!(died.engine_id, engine_id, "the closed engine's id is reported dead");
        assert!(
            matches!(died.reason, DeathReason::Crashed { .. }),
            "a connection-close eviction is reported Crashed, got {:?}",
            died.reason,
        );
    }

    /// The echo request a test forwards; the scripted fake engine answers
    /// it without reading it.
    fn echo_forward() -> ForwardEnvelope {
        ForwardEnvelope {
            recipient: ActorPath::new(<TestEchoActor as Addressable>::NAMESPACE).expect("the echo namespace is a path"),
            kind: <TestEchoRequest as Kind>::ID,
            payload: TestEchoRequest { value: 1 }.encode_into_bytes(),
        }
    }

    /// Block until `log` holds at least `len` entries, or panic naming
    /// `what` after 5s.
    fn await_len(log: &ReplyLog, len: usize, what: &str) -> Vec<RecordedReply> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = log.lock().expect("test setup: reply log mutex poisoned").clone();
            if snapshot.len() >= len {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "{what} within 5s, log so far: {snapshot:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A forward holds its chain open while the remote engine works, relays
    /// every reply event to the sender in wire order, and ends the exchange
    /// at its `CallSettled`: an event arriving after the terminal is not
    /// relayed.
    #[test]
    fn forward_holds_its_chain_until_the_terminal_and_relays_replies_in_order() {
        let (calls_tx, calls_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let scripts = VecDeque::from([
            CallScript {
                frames: vec![ScriptFrame::Echo(1), ScriptFrame::Echo(2), ScriptFrame::End, ScriptFrame::Echo(3)],
                release: Some(release_rx),
            },
            CallScript { frames: vec![ScriptFrame::End], release: None },
        ]);
        let (port, _server) = fake_server(Behavior::Scripted { calls: calls_tx, scripts });
        let (chassis, proxy, log) = spawn_scripted_proxy(3, "scripted", port);
        let sink = chassis.actor_ref::<ProxyReplySink>().erase();

        let (_, settled) = chassis.send_tracked(
            proxy.erase(),
            <ForwardEnvelope as Kind>::ID,
            echo_forward().encode_into_bytes(),
            Some(ReplyTarget::Actor { to: sink, correlation: 11 }),
        );
        calls_rx.recv_timeout(Duration::from_secs(5)).expect("the fake engine receives the first call");
        assert!(
            settled.recv_timeout(Duration::from_millis(300)).is_err_and(|error| error.is_timeout()),
            "the forward's chain stays open while the remote call is unanswered",
        );

        release_tx.send(()).expect("the fake engine waits on the release");
        let first_exchange = await_len(&log, 3, "the first exchange's replies did not arrive");
        assert_eq!(
            first_exchange,
            vec![RecordedReply::Echo(1), RecordedReply::Echo(2), RecordedReply::Settled(Ok(()))],
            "reply events relay in wire order, ahead of the terminal",
        );
        settled.recv_timeout(Duration::from_secs(5)).expect("the forward's chain settles after its terminal");

        let (_, second) = chassis.send_tracked(
            proxy.erase(),
            <ForwardEnvelope as Kind>::ID,
            echo_forward().encode_into_bytes(),
            Some(ReplyTarget::Actor { to: sink, correlation: 12 }),
        );
        calls_rx.recv_timeout(Duration::from_secs(5)).expect("the fake engine receives the second call");
        second.recv_timeout(Duration::from_secs(5)).expect("the second forward's chain settles");
        assert_eq!(
            await_len(&log, 4, "the second exchange's terminal did not arrive"),
            vec![
                RecordedReply::Echo(1),
                RecordedReply::Echo(2),
                RecordedReply::Settled(Ok(())),
                RecordedReply::Settled(Ok(())),
            ],
            "an event after its call's terminal is not relayed",
        );
    }

    /// A forward whose `Call` cannot be written to the engine answers its
    /// sender at once with a `CallSettled::Err` naming the write failure,
    /// and the proxy keeps serving: a later forward still round-trips.
    #[test]
    fn a_forward_whose_call_write_fails_answers_its_caller_with_the_error() {
        let (calls_tx, calls_rx) = mpsc::channel();
        let scripts = VecDeque::from([CallScript { frames: vec![ScriptFrame::End], release: None }]);
        let (port, _server) = fake_server(Behavior::Scripted { calls: calls_tx, scripts });
        let (chassis, proxy, log) = spawn_scripted_proxy(5, "unwritable", port);
        let sink = chassis.actor_ref::<ProxyReplySink>().erase();

        // One byte past the frame cap: the client refuses to encode the
        // `Call` before any byte reaches the socket, so the connection
        // stays healthy for the follow-up forward.
        let oversized = ForwardEnvelope { payload: vec![0; max_frame_size() + 1], ..echo_forward() };
        let (_, settled) = chassis.send_tracked(
            proxy.erase(),
            <ForwardEnvelope as Kind>::ID,
            oversized.encode_into_bytes(),
            Some(ReplyTarget::Actor { to: sink, correlation: 21 }),
        );
        let replies = await_len(&log, 1, "the unwritable forward's terminal did not arrive");
        let [RecordedReply::Settled(Err(RpcError::Other { reason }))] = replies.as_slice() else {
            panic!("the unwritable forward is answered with one CallSettled::Err, got {replies:?}");
        };
        assert!(reason.contains("encoded frame too large"), "the error names the write failure: {reason}");
        settled.recv_timeout(Duration::from_secs(5)).expect("the unwritable forward's chain settles");

        let (_, second) = chassis.send_tracked(
            proxy.erase(),
            <ForwardEnvelope as Kind>::ID,
            echo_forward().encode_into_bytes(),
            Some(ReplyTarget::Actor { to: sink, correlation: 22 }),
        );
        calls_rx.recv_timeout(Duration::from_secs(5)).expect("the fake engine receives the follow-up call");
        second.recv_timeout(Duration::from_secs(5)).expect("the follow-up forward's chain settles");
        assert_eq!(
            await_len(&log, 2, "the follow-up forward's terminal did not arrive")[1],
            RecordedReply::Settled(Ok(())),
            "the proxy still serves after a failed write",
        );
    }

    /// A proxy that closes with a forward still open abandons its debt
    /// quietly, and the hub's RPC server closes the wire call behind it with
    /// its departure error.
    #[test]
    fn closing_with_a_pending_forward_abandons_it_and_the_hub_closes_the_call() {
        let (calls_tx, calls_rx) = mpsc::channel();
        let (port, _server) = fake_server(Behavior::Scripted { calls: calls_tx, scripts: VecDeque::new() });

        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<TraceDispatchCapability>(())
            .with_actor::<FleetCapSink>(FleetCapCells::default())
            .with_actor_configured::<RpcServerCapability>(
                RpcServerParams { peer_kind: substrate_peer_kind(), bind: RpcBind::Boot },
                RpcServerConfig { port: Some(0), port_file: None },
            )
            .build_passive()
            .expect("caps boot");
        let hub_port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
        let engine_id = EngineId(Uuid::from_u128(4));
        let proxy = chassis
            .spawn_actor_for_test::<FleetProxy>(
                Subname::Named("closing"),
                FleetProxyConfig {
                    engine_id,
                    target: ProxyTarget::Adopted { rpc_addr: format!("127.0.0.1:{port}") },
                    heartbeat: None,
                    connect_budget: None,
                },
                (),
            )
            .finish()
            .expect("proxy spawns + connects");

        let mut conn = RpcClient::connect(
            &format!("127.0.0.1:{hub_port}"),
            PeerKind::Client { client_name: "fleet-proxy-test".into(), client_version: "0.0.1".into() },
            || {},
        )
        .expect("client connects to the hub");
        let call = MailEnvelope {
            to: Recipient { engine: Some(engine_id), path: echo_forward().recipient },
            kind: <TestEchoRequest as Kind>::ID,
            payload: TestEchoRequest { value: 1 }.encode_into_bytes(),
        };
        // The proxy registers its route from `wire`, so retry while the hub
        // still answers that no route exists.
        let deadline = Instant::now() + Duration::from_secs(5);
        let cid = loop {
            let cid = conn.client.call(call.clone()).expect("write Call to the hub");
            match conn.inbound.recv_timeout(Duration::from_millis(200)) {
                Ok(WireFrame::ReplyEnd { result: Err(RpcError::UnknownEngine { .. }), .. }) => {
                    assert!(Instant::now() < deadline, "the proxy's route did not register within 5s");
                    thread::sleep(Duration::from_millis(20));
                }
                Err(RecvTimeoutError::Timeout) => break cid,
                other => panic!("unexpected answer while the route registers: {other:?}"),
            }
        };
        calls_rx.recv_timeout(Duration::from_secs(5)).expect("the fake engine receives the forwarded call");

        let (_, _terminated) = chassis.send_tracked(
            proxy.erase(),
            <TerminateEngine as Kind>::ID,
            TerminateEngine { engine_id: engine_id.0.to_string() }.encode_into_bytes(),
            None,
        );
        let end = conn.inbound.recv_timeout(Duration::from_secs(5)).expect("the hub closes the wire call");
        let WireFrame::ReplyEnd { cid: closed, result: Err(RpcError::Other { reason }) } = end else {
            panic!("the hub closes the call with its departure error, got {end:?}");
        };
        assert_eq!(closed, cid, "the closed call is the one forwarded");
        assert!(reason.contains("left before the call settled"), "the departure error names the departure: {reason}");

        drop(conn);
        let teardown = catch_unwind(AssertUnwindSafe(|| drop(chassis)));
        assert!(teardown.is_ok(), "closing with a pending forward raises no fatal abort");
    }
}
