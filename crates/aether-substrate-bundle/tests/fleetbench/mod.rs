//! `FleetBench` — a real-process E2E test-support harness over the
//! hub/RPC path (issue 1451). Where [`TestBench`](aether_substrate_bundle::test_bench)
//! drives the substrate in-process over a loopback channel, `FleetBench`
//! drives the *actual* hub → RPC → forked-headless-substrate stack: it
//! boots a hub-shaped passive chassis (`RpcServerCapability` +
//! `EngineServer` + `TraceDispatchCapability`), connects a raw-frame
//! `TcpStream` client, and forks real `aether-substrate-headless`
//! processes through the engines cap. That exercises ADR-0099 lineage
//! addressing, schema-encode, fork+exec + env injection, and component
//! load via the wasm custom section — the layers that sit below the
//! MCP-JSON front and carry the regressions an agent hits when driving
//! a live engine.
//!
//! It is a `tests/` support module (pulled into each scenario file via
//! `mod fleetbench;`), not a crate and not a lib module — the bundle
//! keeps tokio out of its production build, so `FleetBench` is sync and
//! raw-frame: the same wire protocol `aether-mcp` speaks, with the
//! async/JSON front stripped.
//!
//! Each scenario binary uses a subset of the API, so the module carries
//! a crate-wide `dead_code` allow — a method unused by one binary is
//! exercised by another.

#![allow(dead_code)]
// The manifest-presence guard emits its skip diagnostic via stderr so
// `cargo test` surfaces "skipping: ..." alongside `test ... ok` (issue
// 891), matching `headless_autoload.rs`.
#![allow(clippy::print_stderr)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::mem;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aether_capabilities::rpc::{
    Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, RpcServerCapability, RpcServerConfig,
    RpcServerHandle, WIRE_VERSION, WireFrame,
};
use aether_capabilities::trace::TraceDispatchCapability;
use aether_capabilities::{EngineConfig, EngineServer};
use aether_codec::frame::{FrameError, read_frame, write_frame};
use aether_data::{DagId, EngineId, Kind, KindId, MailId, MailboxId, Uuid, mailbox_id_from_path};
use aether_kinds::MailEnvelope as TracedEnvelope;
use aether_kinds::descriptors;
use aether_kinds::trace::{DispatchTraced, DispatchTracedAck, TRACE_MAILBOX_NAME};
use aether_kinds::{
    Cancel, CancelResult, ComponentCapabilities, DagDescriptor, DeadEngineDescriptor,
    EngineDescriptor, HandleDescribe, HandleDescribeResult, ListEngines, ListEnginesResult,
    LoadComponent, LoadResult, LogTail, LogTailResult, ReplaceComponent, ReplaceResult,
    SpawnEngine, SpawnEngineResult, Status, StatusResult, Submit, SubmitResult, TerminateEngine,
    TerminateEngineResult,
};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis, NeverDriver, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use serde::Serialize;

/// Forking a real substrate (cold debug-build start) and waiting for it
/// to bind its RPC port dominates the per-call deadline; matches the
/// seed (`rpc_engine_routing`).
const CALL_DEADLINE: Duration = Duration::from_secs(30);

/// Minimal `Chassis` so `Builder::new` can stand a passive cap set up
/// in-process. Never built through `Chassis::build` — `Builder::new`
/// drives the cap set directly, mirroring the seed.
struct TestChassis;
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is driven by Builder::new directly in FleetBench")
    }
}

/// One driven wire `Call` and the kinds that came back, recorded in
/// order. This is the first-class object the deferred agent-benchmark
/// slots into: an agent emits a sequence of calls, `FleetBench` records
/// them here, and the benchmark scores the recorded trace against the
/// expected one.
#[derive(Debug, Clone)]
pub struct CallRecord {
    /// Monotonic wire correlation id assigned to this call.
    pub cid: u64,
    /// `None` for a hub-local call (`aether.engine`), `Some` for a call
    /// routed to a forked substrate.
    pub engine: Option<EngineId>,
    /// The mailbox path the call addressed.
    pub mailbox: String,
    /// The request kind sent.
    pub request_kind: KindId,
    /// The kinds streamed back as `ReplyEvent`s before `ReplyEnd`.
    pub reply_kinds: Vec<KindId>,
}

/// The `dist/manifest.json` slice `FleetBench` reads: the wasm component
/// map (`stem → components/<stem>.wasm`, relative to `dist/`). A
/// `Deserialize` view rather than a mirror of xtask's `Serialize`
/// `Manifest` — serde ignores the manifest's other fields (`target`,
/// `profile`, `chassis`), so this stays robust to manifest growth.
#[derive(serde::Deserialize)]
struct ManifestView {
    components: BTreeMap<String, String>,
}

/// The three `LoadResult::Ok` fields a loaded component exposes:
/// the assigned trampoline `mailbox_id` (the [`replace`](FleetBench::replace)
/// target), the rendered ADR-0099 lineage `addr`, and the advertised
/// receive-side `capabilities`. Returned by
/// [`load_full`](FleetBench::load_full) for the lifecycle rows that need
/// the mailbox id the thin [`load`](FleetBench::load) delegate discards.
pub struct Loaded {
    pub mailbox_id: MailboxId,
    pub addr: String,
    pub capabilities: ComponentCapabilities,
}

/// A booted hub chassis plus a connected, handshaken raw-frame client.
/// Forked engines are tracked so [`Drop`] terminates each one — a
/// scenario leaves no orphaned substrate behind.
pub struct FleetBench {
    /// Kept alive for the lifetime of the bench; dropping it tears the
    /// hub caps down.
    _chassis: PassiveChassis<TestChassis>,
    stream: TcpStream,
    next_cid: u64,
    spawned: Vec<EngineId>,
    calls: Vec<CallRecord>,
    /// Per-bench handle-store root the forked substrates write under, so
    /// their `engines/<id>/v1/lock.pid` locks can't collide with another
    /// concurrent fork+exec test on the shared default root. Removed on
    /// [`Drop`].
    store_root: PathBuf,
}

impl FleetBench {
    /// Boot the hub-shaped passive chassis, connect a client
    /// `TcpStream`, and complete the `Hello`/`HelloAck` handshake.
    pub fn start() -> Self {
        let store_root = isolate_store_root();
        let (chassis, port) = boot_hub();
        let stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .expect("test setup: connecting to the hub's bound RPC port succeeds");
        stream
            .set_read_timeout(Some(CALL_DEADLINE))
            .expect("test setup: setting a read timeout on a connected stream succeeds");

        let mut bench = Self {
            _chassis: chassis,
            stream,
            next_cid: 1,
            spawned: Vec::new(),
            calls: Vec::new(),
            store_root,
        };
        bench.handshake();
        bench
    }

    /// The recorded call sequence, in order. Used by scenarios to assert
    /// on round-trip shape (the benchmark-ready trace).
    pub fn calls(&self) -> &[CallRecord] {
        &self.calls
    }

    /// Fork a real `aether-substrate-headless` through the hub's engines
    /// cap and return its `EngineId`. Records the engine for teardown.
    pub fn spawn_headless(&mut self) -> EngineId {
        let headless = env!("CARGO_BIN_EXE_aether-substrate-headless");
        let replies = self.call(
            None,
            "aether.engine",
            &SpawnEngine {
                binary_path: headless.to_owned(),
                args: vec![],
                boot_manifest: None,
            },
        );
        let payload = single_reply(&replies, "SpawnEngine");
        let engine_id = match SpawnEngineResult::decode_from_bytes(&payload) {
            Some(SpawnEngineResult::Ok { engine_id, .. }) => engine_id,
            Some(SpawnEngineResult::Err { error }) => panic!("spawn_headless failed: {error}"),
            None => panic!("undecodable SpawnEngineResult"),
        };
        let engine = EngineId(Uuid::parse_str(&engine_id).expect("engine_id parses as a UUID"));
        self.spawned.push(engine);
        engine
    }

    /// Enumerate the engines the hub currently supervises (the engines
    /// cap's `ListEngines`). Hub-local — addressed at `aether.engine`
    /// with no engine route.
    pub fn list_engines(&mut self) -> Vec<EngineDescriptor> {
        let replies = self.call(None, "aether.engine", &ListEngines {});
        let payload = single_reply(&replies, "ListEngines");
        match ListEnginesResult::decode_from_bytes(&payload) {
            Some(result) => result.engines,
            None => panic!("undecodable ListEnginesResult"),
        }
    }

    /// The engines cap's recently-died ring — the last few engines that
    /// left the supervised table, each with why it left (issue 1906).
    /// Same `ListEngines` round-trip as [`list_engines`](Self::list_engines),
    /// reading the reply's `recently_died` sidecar instead of the live set.
    pub fn recently_died(&mut self) -> Vec<DeadEngineDescriptor> {
        let replies = self.call(None, "aether.engine", &ListEngines {});
        let payload = single_reply(&replies, "ListEngines");
        match ListEnginesResult::decode_from_bytes(&payload) {
            Some(result) => result.recently_died,
            None => panic!("undecodable ListEnginesResult"),
        }
    }

    /// Load the `<stem>` component wasm (located through
    /// `dist/manifest.json`) into `engine` and return its registered
    /// ADR-0099 lineage address
    /// (`aether.component/aether.embedded:<NAMESPACE>`). Loads with no
    /// init-config — the `LoadComponent.config` carrier is empty, which a
    /// `Config = ()` component decodes uniformly. A thin delegate over
    /// [`load_full`](Self::load_full) for callers that need only the
    /// address.
    pub fn load(&mut self, engine: EngineId, stem: &str) -> String {
        self.load_full(engine, stem).addr
    }

    /// Load the `<stem>` component and surface all three `LoadResult::Ok`
    /// fields as a [`Loaded`]: the assigned trampoline `mailbox_id` (the
    /// [`replace`](Self::replace) target), the rendered lineage `addr`,
    /// and the advertised `capabilities`. The lifecycle rows that drive a
    /// replace or re-address the loaded mailbox need the mailbox id the
    /// thin [`load`](Self::load) delegate drops.
    pub fn load_full(&mut self, engine: EngineId, stem: &str) -> Loaded {
        let wasm = read_component_wasm(stem);
        let replies = self.call(
            Some(engine),
            "aether.component",
            &LoadComponent {
                wasm,
                name: None,
                config: Vec::new(),
                export: None,
            },
        );
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok {
                mailbox_id,
                name,
                capabilities,
            }) => Loaded {
                mailbox_id,
                addr: name,
                capabilities,
            },
            Some(LoadResult::Err { error }) => panic!("load of {stem:?} failed: {error}"),
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Terminate `engine` through the asserting [`call`](Self::call)
    /// path — the agent-facing `terminate_substrate`, distinct from the
    /// `Drop`-only best-effort
    /// [`terminate_quietly`](Self::terminate_quietly). The engines cap
    /// removes the fleet entry synchronously before it replies, so a
    /// follow-up [`list_engines`](Self::list_engines) reflects the
    /// eviction with no heartbeat-eviction wait. Drops `engine` from the
    /// teardown set so `Drop` doesn't double-terminate it.
    pub fn terminate(&mut self, engine: EngineId) {
        let replies = self.call(
            None,
            "aether.engine",
            &TerminateEngine {
                engine_id: engine.0.to_string(),
            },
        );
        let payload = single_reply(&replies, "TerminateEngine");
        match TerminateEngineResult::decode_from_bytes(&payload) {
            Some(TerminateEngineResult::Ok) => {}
            Some(TerminateEngineResult::Err { error }) => {
                panic!("terminate of {engine:?} failed: {error}")
            }
            None => panic!("undecodable TerminateEngineResult"),
        }
        self.spawned.retain(|e| *e != engine);
    }

    /// Replace the component bound to `mailbox_id` on `engine` with the
    /// `<stem>` wasm (ADR-0022 in-place swap) and return the swapped
    /// binary's advertised capabilities. The trampoline keeps its
    /// load-time name across replace, so targeting the captured
    /// `mailbox_id` rebinds the same lineage address to the new instance.
    pub fn replace(
        &mut self,
        engine: EngineId,
        mailbox_id: MailboxId,
        stem: &str,
    ) -> ComponentCapabilities {
        let wasm = read_component_wasm(stem);
        let replies = self.call(
            Some(engine),
            "aether.component",
            &ReplaceComponent {
                mailbox_id,
                wasm,
                drain_timeout_ms: None,
                config: Vec::new(),
            },
        );
        let payload = single_reply(&replies, "ReplaceComponent");
        match ReplaceResult::decode_from_bytes(&payload) {
            Some(ReplaceResult::Ok { capabilities }) => capabilities,
            Some(ReplaceResult::Err { error }) => panic!("replace with {stem:?} failed: {error}"),
            None => panic!("undecodable ReplaceResult"),
        }
    }

    /// Tail `recipient`'s per-actor `ActorLogRing` (ADR-0081) on
    /// `engine`. `since: None` reads from the oldest retained entry;
    /// `Some(n)` returns only entries with `sequence > n` (the per-actor
    /// cursor). `max: 0` resolves to the substrate-default cap. The
    /// framework dispatch loop answers `LogTail` for every native actor
    /// and wasm trampoline, so `recipient` is any live mailbox path.
    pub fn log_tail(
        &mut self,
        engine: EngineId,
        recipient: &str,
        since: Option<u64>,
    ) -> LogTailResult {
        let replies = self.call(
            Some(engine),
            recipient,
            &LogTail {
                max: 0,
                min_level: None,
                since,
            },
        );
        let payload = single_reply(&replies, "LogTail");
        LogTailResult::decode_from_bytes(&payload).expect("undecodable LogTailResult")
    }

    /// Route a mail to a recipient on a forked substrate and return the
    /// reply envelopes (one per `ReplyEvent`). `recipient` is a mailbox
    /// path — a chassis cap (`aether.fs`) or a loaded component's
    /// lineage address (`aether.component/aether.embedded:<name>`).
    pub fn send<K>(&mut self, engine: EngineId, recipient: &str, mail: &K) -> Vec<MailEnvelope>
    where
        K: Kind + Serialize,
    {
        self.call(Some(engine), recipient, mail)
    }

    /// Write one `Call` frame and read until its `ReplyEnd`, returning
    /// the `ReplyEvent` envelopes seen in between and recording the call
    /// into [`calls`](Self::calls). Panics on a `ReplyEnd::Err` or a
    /// mismatched cid — the seed's `call_round_trip`, generalised to N
    /// reply events and a recorded trace.
    fn call<K>(&mut self, engine: Option<EngineId>, mailbox: &str, request: &K) -> Vec<MailEnvelope>
    where
        K: Kind + Serialize,
    {
        let cid = self.next_cid;
        self.next_cid += 1;

        self.write_call(cid, engine, mailbox, K::ID, request.encode_into_bytes())
            .expect("test setup: writing a Call frame to the hub succeeds");

        let mut events: Vec<MailEnvelope> = Vec::new();
        loop {
            match read_frame(&mut self.stream).expect("test setup: reading a reply frame succeeds")
            {
                WireFrame::ReplyEvent {
                    cid: got_cid,
                    envelope,
                } => {
                    assert_eq!(got_cid, cid, "ReplyEvent cid mismatch");
                    events.push(envelope);
                }
                WireFrame::ReplyEnd {
                    cid: got_cid,
                    result,
                } => {
                    assert_eq!(got_cid, cid, "ReplyEnd cid mismatch");
                    result.unwrap_or_else(|e| panic!("call {cid} ended with error: {e:?}"));
                    self.calls.push(CallRecord {
                        cid,
                        engine,
                        mailbox: mailbox.to_owned(),
                        request_kind: K::ID,
                        reply_kinds: events.iter().map(|e| e.kind).collect(),
                    });
                    return events;
                }
                other => panic!("unexpected frame for call {cid}: {other:?}"),
            }
        }
    }

    fn handshake(&mut self) {
        write_frame(
            &mut self.stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer: PeerKind::Client {
                    client_name: "fleetbench".into(),
                    client_version: "0.0.1".into(),
                },
            }),
        )
        .expect("test setup: writing the client Hello succeeds");
        match read_frame(&mut self.stream).expect("test setup: reading the HelloAck succeeds") {
            WireFrame::HelloAck(HelloAck { wire_version, .. }) => {
                assert_eq!(wire_version, WIRE_VERSION, "wire version mismatch");
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    /// Build and write one `WireFrame::Call` to the hub. Shared by the
    /// asserting [`call`](Self::call) path and the best-effort
    /// [`terminate_quietly`](Self::terminate_quietly) drain; the caller
    /// decides whether a write error panics or is swallowed.
    fn write_call(
        &mut self,
        cid: u64,
        engine: Option<EngineId>,
        mailbox: &str,
        kind: KindId,
        payload: Vec<u8>,
    ) -> Result<(), FrameError> {
        write_frame(
            &mut self.stream,
            &WireFrame::Call {
                cid: Some(cid),
                envelope: MailEnvelope {
                    to: MailboxAddress {
                        engine,
                        mailbox: mailbox_id_from_path(mailbox),
                    },
                    from: None,
                    kind,
                    correlation_id: None,
                    payload,
                },
            },
        )
    }

    /// Best-effort `TerminateEngine` for one engine — used only on the
    /// `Drop` path, so it swallows every wire error rather than
    /// panicking (a panic here would abort under an unwinding test).
    fn terminate_quietly(&mut self, engine: EngineId) {
        let cid = self.next_cid;
        self.next_cid += 1;
        let payload = TerminateEngine {
            engine_id: engine.0.to_string(),
        }
        .encode_into_bytes();
        if self
            .write_call(cid, None, "aether.engine", TerminateEngine::ID, payload)
            .is_err()
        {
            return;
        }
        // Drain to this call's ReplyEnd so the next Drop iteration reads
        // its own frames, not a straggler. Any error ends the drain.
        loop {
            match read_frame(&mut self.stream) {
                Ok(WireFrame::ReplyEnd { cid: got_cid, .. }) if got_cid == cid => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    /// Like [`load`](Self::load) but threads a typed init-config into the
    /// component: `config` is encoded into the `LoadComponent.config`
    /// carrier the guest decodes as its `FfiActor::Config` (ADR-0090).
    /// Returns the registered ADR-0099 lineage address. Used by
    /// components whose typed `Config` cannot decode from the empty
    /// carrier the empty-config [`load`](Self::load) sends.
    pub fn load_with_config<C>(&mut self, engine: EngineId, stem: &str, config: &C) -> String
    where
        C: Kind + Serialize,
    {
        let wasm = read_component_wasm(stem);
        let replies = self.call(
            Some(engine),
            "aether.component",
            &LoadComponent {
                wasm,
                name: None,
                config: config.encode_into_bytes(),
                export: None,
            },
        );
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { name, .. }) => name,
            Some(LoadResult::Err { error }) => panic!("load of {stem:?} failed: {error}"),
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Route a one-entry traced batch (`DispatchTraced`) to a forked
    /// engine's `aether.trace` mailbox and return the chassis-root
    /// [`MailId`] every dispatched envelope inherited plus the reply
    /// envelopes collected across the settlement window. Mirrors
    /// `aether-mcp`'s `send_mail_traced`, minus the round-2 trace-tree
    /// stitch: Tier-A asserts settlement (the [`call`](Self::call) read
    /// already spans it — the server holds the wire `Call` open until
    /// chain settlement) and the collected replies, not the
    /// reconstructed tree.
    ///
    /// The leading `ReplyEvent` is the trace cap's synchronous
    /// `DispatchTracedAck::Ok` — its ordering is well-defined (the trace
    /// handler replies before the dispatched children run), so it is
    /// split off and decoded for the `root`, and the trailing events are
    /// the dispatched mail's correlated replies. Panics on an
    /// `Err`/undecodable ack, mirroring [`single_reply`].
    pub fn send_traced<K>(
        &mut self,
        engine: EngineId,
        recipient: &str,
        mail: &K,
    ) -> (MailId, Vec<MailEnvelope>)
    where
        K: Kind + Serialize,
    {
        let batch = DispatchTraced {
            mails: vec![TracedEnvelope {
                recipient_name: recipient.to_owned(),
                kind_name: K::NAME.to_owned(),
                payload: mail.encode_into_bytes(),
                count: 1,
            }],
        };
        let mut events = self.call(Some(engine), TRACE_MAILBOX_NAME, &batch);
        assert!(
            !events.is_empty(),
            "send_traced expected a DispatchTracedAck reply event, got none"
        );
        let ack = events.remove(0);
        let root = match DispatchTracedAck::decode_from_bytes(&ack.payload) {
            Some(DispatchTracedAck::Ok { root }) => root,
            Some(DispatchTracedAck::Err { error }) => panic!("send_traced batch rejected: {error}"),
            None => panic!("undecodable DispatchTracedAck"),
        };
        (root, events)
    }

    /// Submit a computation DAG (ADR-0047) to `engine`'s `aether.dag`
    /// cap and decode the synchronous [`SubmitResult`]. Validation runs
    /// on the submit call before any source dispatches, so a bad
    /// descriptor still comes back as a wire-`Ok` reply carrying
    /// `SubmitResult::Err` — the asserting [`call`](Self::call) path
    /// handles both arms identically.
    pub fn submit_dag(&mut self, engine: EngineId, descriptor: &DagDescriptor) -> SubmitResult {
        let replies = self.call(
            Some(engine),
            "aether.dag",
            &Submit {
                descriptor: descriptor.clone(),
            },
        );
        let payload = single_reply(&replies, "Submit");
        SubmitResult::decode_from_bytes(&payload)
            .expect("test setup: decoding a SubmitResult reply")
    }

    /// Query a submitted DAG's [`StatusResult`] (ADR-0047 §6) by its
    /// substrate-minted [`DagId`].
    pub fn dag_status(&mut self, engine: EngineId, dag_id: DagId) -> StatusResult {
        let replies = self.call(Some(engine), "aether.dag", &Status { dag_id });
        let payload = single_reply(&replies, "Status");
        StatusResult::decode_from_bytes(&payload)
            .expect("test setup: decoding a StatusResult reply")
    }

    /// Cancel an in-flight DAG (ADR-0047 §1) by its [`DagId`] and decode
    /// the [`CancelResult`]. `Ok.cancelled` is `false` when the DAG had
    /// already settled (nothing to cancel).
    pub fn dag_cancel(&mut self, engine: EngineId, dag_id: DagId) -> CancelResult {
        let replies = self.call(Some(engine), "aether.dag", &Cancel { dag_id });
        let payload = single_reply(&replies, "Cancel");
        CancelResult::decode_from_bytes(&payload)
            .expect("test setup: decoding a CancelResult reply")
    }

    /// Summarize `engine`'s persistent handle store (ADR-0049 §10),
    /// capping each top-N list at `max`. The store's `aether.handle` cap
    /// answers with a [`HandleDescribeResult`].
    pub fn describe_handles(&mut self, engine: EngineId, max: u32) -> HandleDescribeResult {
        let replies = self.call(Some(engine), "aether.handle", &HandleDescribe { max });
        let payload = single_reply(&replies, "HandleDescribe");
        HandleDescribeResult::decode_from_bytes(&payload)
            .expect("test setup: decoding a HandleDescribeResult reply")
    }

    /// Poll [`dag_status`](Self::dag_status) until the DAG reaches a
    /// terminal status (`Complete` / `Failed`), sleeping between polls.
    /// Panics past `deadline` — a DAG that never settles is a test bug,
    /// not a pass. The forked `FsCapability` inbox poll adds ~100ms of
    /// source latency, so callers pass a generous `deadline` (the
    /// `CALL_DEADLINE` mirror).
    pub fn poll_dag(
        &mut self,
        engine: EngineId,
        dag_id: DagId,
        deadline: Duration,
    ) -> StatusResult {
        let start = Instant::now();
        loop {
            let status = self.dag_status(engine, dag_id);
            match status {
                StatusResult::Complete { .. } | StatusResult::Failed { .. } => return status,
                StatusResult::Pending | StatusResult::Running { .. } => {}
            }
            assert!(
                start.elapsed() < deadline,
                "poll_dag: DAG {dag_id:?} did not reach a terminal status within {deadline:?}",
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for FleetBench {
    fn drop(&mut self) {
        let engines = mem::take(&mut self.spawned);
        for engine in engines {
            self.terminate_quietly(engine);
        }
        // Best-effort: reap this bench's per-engine handle-store dirs.
        let _ = fs::remove_dir_all(&self.store_root);
    }
}

/// Point this bench's forked substrates at a unique per-process
/// handle-store root, so their `engines/<id>/v1/lock.pid` locks can't
/// collide with another concurrent fork+exec test (the seed
/// `rpc_engine_routing` / `engines_cap`) on the shared default
/// `dirs::data_dir()/aether/engines` root — the issue-1274 lock
/// collision, here across test processes since the cap mints engine ids
/// from a fixed sequence. Mirrors `engines_cap`'s `two_engines`: the cap
/// reads `AETHER_ENGINE_STORE_ROOT` when it forks (priority over the
/// default), and `AETHER_HANDLE_STORE_DIR` must be unset so the cap's
/// per-engine injection wins.
fn isolate_store_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let root = env::temp_dir().join(format!("aether-fleetbench-{}-{nanos}", process::id()));
    // SAFETY: nextest runs each integration test in its own process, so
    // this env mutation can't race a sibling test; each `FleetBench` in a
    // process gets a fresh `nanos`-tagged root.
    unsafe {
        env::set_var("AETHER_ENGINE_STORE_ROOT", &root);
        env::remove_var("AETHER_HANDLE_STORE_DIR");
    }
    root
}

/// Boot a hub-shaped passive chassis: a forwarding `RpcServerCapability`
/// (engine-addressed Calls route through `aether.engine`), the engines
/// cap, and `TraceDispatchCapability` so the `RpcServer`'s local Calls
/// settle and close. Returns the chassis and the port the RPC server
/// bound. Mirrors the seed's `boot_hub`.
fn boot_hub() -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let store = Arc::new(HandleStore::new(1024 * 1024));
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<EngineServer>(EngineConfig::default())
        .with_actor::<RpcServerCapability>(RpcServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            peer_kind: PeerKind::Substrate {
                engine_name: "fleetbench-hub".into(),
                engine_version: "0.1.0".into(),
                kinds: vec![],
            },
        })
        .build_passive()
        .expect("test setup: hub caps boot");
    let port = chassis
        .handle::<RpcServerHandle>()
        .expect("test setup: RpcServerHandle published")
        .local_port;
    (chassis, port)
}

/// Exactly one `ReplyEvent` payload, panicking if a call that should
/// yield a single reply yielded zero or many.
fn single_reply(replies: &[MailEnvelope], label: &str) -> Vec<u8> {
    match replies {
        [one] => one.payload.clone(),
        other => panic!(
            "{label} expected exactly one reply event, got {}",
            other.len()
        ),
    }
}

/// Workspace `dist/` directory: `CARGO_MANIFEST_DIR`
/// (`crates/aether-substrate-bundle`) up two levels to the workspace
/// root, then `dist/`.
fn dist_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist")
}

/// Manifest-presence guard for the `FleetBench` tests that load component
/// wasm. Returns `true` when `dist/manifest.json` is present (the test
/// proceeds); returns `false` when it's absent so the caller can
/// early-return as a skip — except under `AETHER_REQUIRE_RUNTIME` (which
/// CI sets), where a missing manifest panics so a missing
/// `cargo xtask dist` pre-build can't pass silently. Mirrors
/// `headless_autoload.rs`'s pre-built-wasm skip/require convention (issue
/// 891): a bare `cargo nextest run` in a worktree that hasn't run
/// `cargo xtask dist` skips instead of reporting hard failures that read
/// as regressions.
///
/// [`read_component_wasm`] itself keeps its panic: past this guard the
/// manifest is present, and a manifest that exists but is missing a
/// requested stem (a stale dist, not an unbuilt one) is a real test bug.
#[must_use]
pub fn dist_manifest_present() -> bool {
    let manifest_path = dist_dir().join("manifest.json");
    if manifest_path.exists() {
        return true;
    }
    assert!(
        env::var("AETHER_REQUIRE_RUNTIME").is_err(),
        "AETHER_REQUIRE_RUNTIME set but {} is absent; \
         run `cargo xtask dist` to build the component wasm + manifest",
        manifest_path.display(),
    );
    eprintln!(
        "skipping: {} absent; \
         run `cargo xtask dist` first to build the component wasm + manifest",
        manifest_path.display(),
    );
    false
}

/// Read a component wasm by stem through `dist/manifest.json` (the
/// #1445 dist tree). Panics with a `cargo xtask dist` hint if the
/// manifest is absent — the harness can't locate component wasm without
/// it. Tests guard their load path with [`dist_manifest_present`] so a
/// missing manifest skips rather than reaching this panic.
pub fn read_component_wasm(stem: &str) -> Vec<u8> {
    let dist = dist_dir();
    let manifest_path = dist.join("manifest.json");
    let raw = fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "reading {} ({e}); run `cargo xtask dist` first to build the component wasm + manifest",
            manifest_path.display(),
        )
    });
    let manifest: ManifestView =
        serde_json::from_str(&raw).expect("test setup: dist/manifest.json parses");
    let rel = manifest.components.get(stem).unwrap_or_else(|| {
        panic!("component stem {stem:?} is not in dist/manifest.json; run `cargo xtask dist`")
    });
    let wasm_path = dist.join(rel);
    fs::read(&wasm_path)
        .unwrap_or_else(|e| panic!("reading component wasm {} ({e})", wasm_path.display()))
}
