//! `FleetHarness` — a real-process E2E test-support harness over the
//! hub/RPC path (issue 1451). Where `SubstrateHarness`
//! (`aether-harness-substrate`'s in-process harness) drives the substrate
//! over a loopback channel, `FleetHarness`
//! drives the *actual* hub → RPC → forked-headless-substrate stack: it
//! boots a hub-shaped passive chassis (`RpcServerCapability` +
//! `FleetServer` + `TraceDispatchCapability`), connects a raw-frame
//! `TcpStream` client, and forks real `aether-headless`
//! processes through the engines cap. That exercises ADR-0099 lineage
//! addressing, schema-encode, fork+exec + env injection, and component
//! load via the wasm custom section — the layers that sit below the
//! MCP-JSON front and carry the regressions an agent hits when driving
//! a live engine.
//!
//! `FleetHarness` is sync and raw-frame — the same wire protocol
//! `aether-mcp` speaks, with the async/JSON front stripped — so the
//! harness pulls no tokio into any consumer's dev graph. Scenario suites
//! take it as a dev-dependency; the forked headless chassis binary and
//! the component wasm both resolve through `dist/manifest.json`, the
//! tree `cargo xtask dist` packages (see [`headless_bin_path`] and
//! [`read_component_wasm`]).

// Matching `aether_substrate::testing`: test-support harness — the returned
// values are always consumed by the calling scenario, and the panics on wire /
// decode failures are themselves the test assertions.
#![allow(clippy::must_use_candidate, clippy::missing_panics_doc)]
// The manifest-presence guard emits its skip diagnostic via stderr so
// `cargo test` surfaces "skipping: ..." alongside `test ... ok` (issue
// 891), matching `headless_autoload.rs`; the slow-gate re-arm loop logs
// its heartbeat the same way.
#![allow(clippy::print_stderr)]
// The harness reads its process-level test knobs (AETHER_REQUIRE_RUNTIME,
// the AETHER_HARNESS_FLEET_* budgets, AETHER_HARNESS_FLEET_HEADLESS_BIN)
// straight from the environment — test-harness tuning, not cap config —
// and hand-hashes wire mailbox paths (`mailbox_id_from_path`), the
// sanctioned wire-`Call`-forwarding use.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::mem;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aether_codec::frame::{FrameError, read_frame, write_frame};
use aether_data::{EngineId, Kind, KindId, MailId, MailboxId, Uuid, mailbox_id_from_path};
use aether_fleet::{FleetConfig, FleetServer};
use aether_kinds::NamedMail;
use aether_kinds::descriptors;
use aether_kinds::trace::{DispatchTraced, DispatchTracedAck, TRACE_MAILBOX_NAME};
use aether_kinds::{
    BinarySelector, ComponentCapabilities, ComponentSelector, DeadEngineDescriptor, EngineDescriptor,
    ListComponentBinaries, ListComponentBinariesResult, ListComponents, ListComponentsResult, ListEngineBinaries,
    ListEngineBinariesResult, ListEngines, ListEnginesResult, LoadComponent, LoadResult, LogTail, LogTailResult,
    ReplaceComponent, ReplaceResult, ResolveComponent, ResolveComponentResult, SpawnEngine, SpawnEngineResult,
    TerminateEngine, TerminateEngineResult, UploadBinary, UploadBinaryResult, UploadComponent, UploadComponentResult,
};
use aether_rpc::{
    Hello, HelloAck, MailEnvelope, MailboxAddress, PeerKind, RpcServerCapability, RpcServerConfig, RpcServerHandle,
    RpcServerParams, WIRE_VERSION, WireFrame,
};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::HubOutbound;
use aether_substrate::mail::registry::Registry;
use aether_substrate::testing::{TestChassis, boot_authority};
use aether_trace::TraceDispatchCapability;
use serde::Serialize;

/// Re-arm interval for the client→hub socket read: how often a blocked
/// `read_frame` wakes to log a slow-gate line and re-check its cumulative
/// backstop (issue 2064). This is *not* a correctness gate — the per-call
/// backstop ([`reply_cap`] / [`spawn_cap`]) is. Short enough that a
/// genuine wedge surfaces promptly, long enough not to busy-spin while a
/// healthy reply chain settles.
const READ_REARM: Duration = Duration::from_secs(2);

/// Default cold-start backstop (seconds): the budget the `spawn_headless`
/// `SpawnEngine` call allows for the hub to fork the substrate, dial it
/// (the hub retries the refused connection up to its own configurable
/// `AETHER_HUB_PROXY_CONNECT_BUDGET_SECS`), and reply once the proxy
/// connects — so the `SpawnEngineResult::Ok` reply *is* the explicit
/// port-readiness signal. Kept comfortably above the hub's connect budget
/// so the hub returns a clean `Err` first rather than the client tripping
/// this backstop. Generous over a debug-build cold start under CPU
/// pressure; overridable via `AETHER_HARNESS_FLEET_SPAWN_CAP_SECS`.
const DEFAULT_SPAWN_CAP_SECS: u64 = 60;

/// Default reply backstop (seconds): the deadlock/livelock cap a settled
/// mail chain never reaches, so a slow-but-healthy reply under a saturated
/// `nextest --workspace` run isn't called dead (issue 2064, mirroring
/// #2062's 300 s settlement default). Overridable via
/// `AETHER_HARNESS_FLEET_REPLY_CAP_SECS`.
const DEFAULT_REPLY_CAP_SECS: u64 = 300;

/// Lower a seconds budget to a `Duration`, mapping `0` to the
/// "wait forever" sentinel [`Duration::MAX`] — the gate's `elapsed >= cap`
/// test never trips, so the read blocks on the signal indefinitely (for
/// attaching a debugger to a suspected wedge; the per-interval re-arm line
/// stays the live heartbeat). Mirrors #2062's `AETHER_SETTLEMENT_CAP_SECS`
/// sentinel.
pub fn cap_from_secs(secs: u64) -> Duration {
    if secs == 0 {
        Duration::MAX
    } else {
        Duration::from_secs(secs)
    }
}

/// Resolve the steady-state reply backstop from
/// `AETHER_HARNESS_FLEET_REPLY_CAP_SECS` (default [`DEFAULT_REPLY_CAP_SECS`];
/// `0` → wait forever).
fn reply_cap() -> Duration {
    cap_from_secs(env_secs("AETHER_HARNESS_FLEET_REPLY_CAP_SECS", DEFAULT_REPLY_CAP_SECS))
}

/// Resolve the cold-start backstop from `AETHER_HARNESS_FLEET_SPAWN_CAP_SECS`
/// (default [`DEFAULT_SPAWN_CAP_SECS`]; `0` → wait forever).
fn spawn_cap() -> Duration {
    cap_from_secs(env_secs("AETHER_HARNESS_FLEET_SPAWN_CAP_SECS", DEFAULT_SPAWN_CAP_SECS))
}

/// Interval between [`poll_until`] attempts — short enough to settle
/// promptly, long enough not to busy-spin while a forked engine boots.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Default in-test poll budget (seconds): how long a registration /
/// liveness wait — a forked engine appearing in `ListEngines`, a
/// boot-manifest component registering, a log entry surfacing — is
/// given before it is called a failure. Generous over a debug-build
/// cold start under a saturated `nextest --workspace` run, the same
/// CPU-pressure regime [`DEFAULT_SPAWN_CAP_SECS`] covers, so a
/// slow-but-healthy fork that registers late is not called dead.
/// Overridable via `AETHER_HARNESS_FLEET_POLL_SECS`.
const DEFAULT_POLL_SECS: u64 = 30;

static STORE_ROOT_NONCE: AtomicU64 = AtomicU64::new(0);

/// Resolve the in-test poll budget from `AETHER_HARNESS_FLEET_POLL_SECS`
/// (default [`DEFAULT_POLL_SECS`]; `0` → wait forever).
fn poll_budget() -> Duration {
    cap_from_secs(env_secs("AETHER_HARNESS_FLEET_POLL_SECS", DEFAULT_POLL_SECS))
}

/// Poll `predicate` every `POLL_INTERVAL` until it returns `true` or
/// the `AETHER_HARNESS_FLEET_POLL_SECS` budget elapses; returns whether it became true. Replaces
/// the per-test `for _ in 0..N { … sleep }` loops whose fixed iteration
/// counts flake under CI contention: the budget is wall-clock and
/// generous, so a starved fork that registers late still passes, while a
/// genuinely dead one still fails within the bound. `predicate` is
/// `FnMut` so a caller can capture the matched value out through it.
pub fn poll_until(mut predicate: impl FnMut() -> bool) -> bool {
    let budget = poll_budget();
    let start = Instant::now();
    loop {
        if predicate() {
            return true;
        }
        if start.elapsed() >= budget {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Read a `u64` seconds knob from the environment, falling back to
/// `default` when unset or unparseable — a test harness tolerates a
/// typo'd override rather than aborting the run.
fn env_secs(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

/// Whether a `read_frame` error is a socket read-timeout — the re-arm
/// signal — rather than a real failure (the substrate closed the
/// connection, a decode error). A timed-out blocking read surfaces as
/// `WouldBlock` or `TimedOut` depending on platform; both mean "no frame
/// yet", so the call re-arms and re-reads until its cumulative backstop.
/// Any other error propagates as a genuine failure.
pub fn is_rearm_timeout(err: &FrameError) -> bool {
    matches!(
        err,
        FrameError::Io(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    )
}

/// One driven wire `Call` and the kinds that came back, recorded in
/// order. This is the first-class object the deferred agent-benchmark
/// slots into: an agent emits a sequence of calls, `FleetHarness` records
/// them here, and the benchmark scores the recorded trace against the
/// expected one.
#[derive(Debug, Clone)]
pub struct CallRecord {
    /// Monotonic wire correlation id assigned to this call.
    pub cid: u64,
    /// `None` for a hub-local call (`aether.fleet`), `Some` for a call
    /// routed to a forked substrate.
    pub engine: Option<EngineId>,
    /// The mailbox path the call addressed.
    pub mailbox: String,
    /// The request kind sent.
    pub request_kind: KindId,
    /// The kinds streamed back as `ReplyEvent`s before `ReplyEnd`.
    pub reply_kinds: Vec<KindId>,
}

/// The `dist/manifest.json` slice `FleetHarness` reads: the wasm component
/// map (`stem → components/<stem>.wasm`) and the chassis bin map
/// (`name → bin/<name>`), both relative to `dist/`. A `Deserialize` view
/// rather than a mirror of xtask's `Serialize` `Manifest` — serde
/// ignores the manifest's other fields (`target`, `profile`), so this
/// stays robust to manifest growth. `chassis` defaults empty so a
/// component-only manifest still classifies its component stems.
#[derive(serde::Deserialize)]
struct ManifestView {
    components: BTreeMap<String, String>,
    #[serde(default)]
    chassis: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DistManifestClassification {
    Available { relative_path: String },
    MissingStem,
    Unparseable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DistComponentGuardOutcome {
    Available,
    Skip(String),
    RequireRuntime(String),
}

enum DistComponentRequirement {
    Available(PathBuf),
    ManifestAbsent,
    ManifestUnreadable(String),
    StemMissing,
}

/// The three `LoadResult::Ok` fields a loaded component exposes:
/// the assigned trampoline `mailbox_id` (the [`replace`](FleetHarness::replace)
/// target), the rendered ADR-0099 lineage `addr`, and the advertised
/// receive-side `capabilities`. Returned by
/// [`load_full`](FleetHarness::load_full) for the lifecycle rows that need
/// the mailbox id the thin [`load`](FleetHarness::load) delegate discards.
pub struct Loaded {
    pub mailbox_id: MailboxId,
    pub addr: String,
    pub capabilities: ComponentCapabilities,
}

/// A booted hub chassis plus a connected, handshaken raw-frame client.
/// Forked engines are tracked so [`Drop`] terminates each one — a
/// scenario leaves no orphaned substrate behind.
pub struct FleetHarness {
    /// Kept alive for the lifetime of the harness; dropping it tears the
    /// hub caps down.
    _chassis: PassiveChassis<TestChassis>,
    stream: TcpStream,
    next_cid: u64,
    spawned: Vec<EngineId>,
    calls: Vec<CallRecord>,
    /// Per-harness scratch root the forked substrates materialize their
    /// per-engine executables under (ADR-0115), so they can't collide with
    /// another concurrent fork+exec test on the shared default root.
    /// Removed on [`Drop`].
    store_root: PathBuf,
}

impl FleetHarness {
    /// Boot the hub-shaped passive chassis, connect a client
    /// `TcpStream`, and complete the `Hello`/`HelloAck` handshake.
    pub fn start() -> Self {
        let store_root = isolate_store_root();
        let (chassis, port) = boot_hub(&store_root.join("binaries"), &store_root);
        let stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .expect("test setup: connecting to the hub's bound RPC port succeeds");
        // The read timeout is the re-arm *interval*, not a deadline: every
        // `read_frame` in `call_with_budget` wakes on it to re-check the
        // call's cumulative backstop, then re-arms (issue 2064). The
        // backstop, not this interval, decides a wedge.
        stream
            .set_read_timeout(Some(READ_REARM))
            .expect("test setup: setting a read timeout on a connected stream succeeds");

        let mut harness =
            Self { _chassis: chassis, stream, next_cid: 1, spawned: Vec::new(), calls: Vec::new(), store_root };
        harness.handshake();
        harness
    }

    /// The recorded call sequence, in order. Used by scenarios to assert
    /// on round-trip shape (the benchmark-ready trace).
    pub fn calls(&self) -> &[CallRecord] {
        &self.calls
    }

    /// Fork a real `aether-headless` through the hub's engines
    /// cap and return its `EngineId`. Records the engine for teardown.
    ///
    /// Pins the binary by content hash (ADR-0115, #1954): upload the
    /// headless bin into the hub's content-addressed store, then spawn
    /// exactly that hash — so every run forks the same binary regardless of
    /// local build state, instead of handing the cap a host path.
    pub fn spawn_headless(&mut self) -> EngineId {
        self.spawn_headless_inner(None)
    }

    /// Fork a real `aether-headless` with a `--boot-manifest`
    /// pointing at `boot_manifest_path` (ADR-0116, issue 1956; the hub
    /// addresses it via argv per ADR-0162), so the
    /// engine comes up with the manifest's components already loading — the
    /// path a `spawn_substrate` carrying a component list drives. Records
    /// the engine for teardown.
    pub fn spawn_headless_with_boot_manifest(&mut self, boot_manifest_path: &Path) -> EngineId {
        self.spawn_headless_inner(Some(boot_manifest_path.to_string_lossy().into_owned()))
    }

    /// Shared spawn body: pin the headless bin by content hash, fork it
    /// through the engines cap, optionally injecting a boot manifest path.
    fn spawn_headless_inner(&mut self, boot_manifest: Option<String>) -> EngineId {
        let headless = headless_bin_path();
        let hash = match self.upload_binary(&headless.to_string_lossy(), Some("headless")) {
            UploadBinaryResult::Ok { hash, .. } => hash,
            UploadBinaryResult::Err { error } => panic!("spawn_headless upload failed: {error}"),
        };
        // The cold fork+bind+connect rides this call's reply (the hub holds
        // `SpawnEngineResult::Ok` until its proxy connects to the freshly
        // bound substrate), so it waits under the generous cold-start
        // backstop, not the steady-state reply cap (issue 2064).
        let replies = self.call_with_budget(
            None,
            "aether.fleet",
            &SpawnEngine {
                selector: BinarySelector { query: Some(hash), chassis: None, caps: vec![], target: None },
                args: vec![],
                boot_manifest,
            },
            spawn_cap(),
            "cold-start",
        );
        let payload = single_reply(&replies, "SpawnEngine");
        let engine_id = match SpawnEngineResult::decode_from_bytes(&payload) {
            Some(SpawnEngineResult::Ok { engine_id, .. }) => engine_id,
            Some(SpawnEngineResult::Err { error, .. }) => panic!("spawn_headless failed: {error}"),
            None => panic!("undecodable SpawnEngineResult"),
        };
        let engine = EngineId(Uuid::parse_str(&engine_id).expect("engine_id parses as a UUID"));
        self.spawned.push(engine);
        engine
    }

    /// Enumerate the engines the hub currently supervises (the engines
    /// cap's `ListEngines`). Hub-local — addressed at `aether.fleet`
    /// with no engine route.
    pub fn list_engines(&mut self) -> Vec<EngineDescriptor> {
        let replies = self.call(None, "aether.fleet", &ListEngines {});
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
        let replies = self.call(None, "aether.fleet", &ListEngines {});
        let payload = single_reply(&replies, "ListEngines");
        match ListEnginesResult::decode_from_bytes(&payload) {
            Some(result) => result.recently_died,
            None => panic!("undecodable ListEnginesResult"),
        }
    }

    /// Upload a binary into the hub's content-addressed store (ADR-0115,
    /// issue 1953) by absolute host path, optionally naming it. Hub-local —
    /// addressed at `aether.fleet` with no engine route. The hub reads the
    /// path, sha256s it, forks `<path> --describe`, and stores both; this
    /// returns the decoded [`UploadBinaryResult`].
    pub fn upload_binary(&mut self, staged_path: &str, name: Option<&str>) -> UploadBinaryResult {
        let replies = self.call(
            None,
            "aether.fleet",
            &UploadBinary { staged_path: staged_path.to_owned(), name: name.map(str::to_owned) },
        );
        let payload = single_reply(&replies, "UploadBinary");
        UploadBinaryResult::decode_from_bytes(&payload).expect("undecodable UploadBinaryResult")
    }

    /// Enumerate the hub's stored engine binaries (ADR-0115, issue 1953)
    /// under the given filter. Hub-local — addressed at `aether.fleet` with
    /// no engine route.
    pub fn list_engine_binaries(&mut self, filter: &ListEngineBinaries) -> ListEngineBinariesResult {
        let replies = self.call(None, "aether.fleet", filter);
        let payload = single_reply(&replies, "ListEngineBinaries");
        ListEngineBinariesResult::decode_from_bytes(&payload).expect("undecodable ListEngineBinariesResult")
    }

    /// Upload a component wasm into the hub's content-addressed store
    /// (ADR-0116, issue 1956) by absolute host path, optionally naming it.
    /// Hub-local — addressed at `aether.fleet` with no engine route. The
    /// hub reads the path, sha256s it, parses its manifest from the wasm,
    /// and stores both; returns the decoded [`UploadComponentResult`].
    pub fn upload_component(&mut self, staged_path: &str, name: Option<&str>) -> UploadComponentResult {
        let replies = self.call(
            None,
            "aether.fleet",
            &UploadComponent { staged_path: staged_path.to_owned(), name: name.map(str::to_owned) },
        );
        let payload = single_reply(&replies, "UploadComponent");
        UploadComponentResult::decode_from_bytes(&payload).expect("undecodable UploadComponentResult")
    }

    /// Enumerate the hub's stored component binaries (ADR-0116, issue 1956)
    /// under the given filter. Hub-local — addressed at `aether.fleet` with
    /// no engine route.
    pub fn list_component_binaries(&mut self, filter: &ListComponentBinaries) -> ListComponentBinariesResult {
        let replies = self.call(None, "aether.fleet", filter);
        let payload = single_reply(&replies, "ListComponentBinaries");
        ListComponentBinariesResult::decode_from_bytes(&payload).expect("undecodable ListComponentBinariesResult")
    }

    /// Enumerate the components an `engine` has actually loaded and
    /// registered, by ADR-0099 lineage name (issue 2020). Engine-local —
    /// addressed at the engine's own `aether.component` mailbox, a definitive
    /// snapshot of the live trampoline set. Poll this after a boot-manifest
    /// spawn to learn deterministically when a requested component is loaded,
    /// rather than inferring liveness from a log-ring side channel.
    pub fn list_components(&mut self, engine: EngineId) -> Vec<String> {
        let replies = self.call(Some(engine), "aether.component", &ListComponents {});
        let payload = single_reply(&replies, "ListComponents");
        match ListComponentsResult::decode_from_bytes(&payload) {
            Some(result) => result.names,
            None => panic!("undecodable ListComponentsResult"),
        }
    }

    /// Resolve a component selector hub-local against the registry
    /// (ADR-0116, issue 1956) — the `ResolveComponent` hop aether-mcp does
    /// before forwarding a `LoadComponent` to a substrate. Hub-local —
    /// addressed at `aether.fleet` with no engine route. Returns the
    /// decoded [`ResolveComponentResult`].
    pub fn resolve_component(&mut self, selector: ComponentSelector) -> ResolveComponentResult {
        let replies = self.call(None, "aether.fleet", &ResolveComponent { selector });
        let payload = single_reply(&replies, "ResolveComponent");
        ResolveComponentResult::decode_from_bytes(&payload).expect("undecodable ResolveComponentResult")
    }

    /// Load a component into `engine` by registry selector (ADR-0116, issue
    /// 1956), mirroring aether-mcp's resolve-then-forward: resolve the
    /// selector hub-local to the wasm bytes + `@actor` export, then forward
    /// `LoadComponent { wasm, export }` to the engine's `aether.component`
    /// mailbox. Returns the loaded component's [`Loaded`] (mailbox id,
    /// lineage address, capabilities). Panics on a resolve / load error.
    pub fn load_by_selector(&mut self, engine: EngineId, selector: &str) -> Loaded {
        let resolved = self.resolve_component(ComponentSelector {
            query: Some(selector.to_owned()),
            namespace: None,
            handled_kind: None,
        });
        let (wasm, export) = match resolved {
            ResolveComponentResult::Ok { wasm, export, .. } => (wasm, export),
            ResolveComponentResult::Err { error } => {
                panic!("resolve of selector {selector:?} failed: {error}")
            }
        };
        let replies = self.call(
            Some(engine),
            "aether.component",
            &LoadComponent { wasm, name: None, config: Vec::new(), export },
        );
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { mailbox_id, name, capabilities }) => Loaded { mailbox_id, addr: name, capabilities },
            Some(LoadResult::Err { error }) => {
                panic!("load by selector {selector:?} failed: {error}")
            }
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Replace the component bound to `mailbox_id` on `engine` with a build
    /// resolved from a registry selector (ADR-0116, issue 1956) — the
    /// resolve-then-forward twin of [`replace`](Self::replace), which loads
    /// by dist stem. Returns the swapped binary's advertised capabilities.
    pub fn replace_by_selector(
        &mut self,
        engine: EngineId,
        mailbox_id: MailboxId,
        selector: &str,
    ) -> ComponentCapabilities {
        let resolved = self.resolve_component(ComponentSelector {
            query: Some(selector.to_owned()),
            namespace: None,
            handled_kind: None,
        });
        let (wasm, export) = match resolved {
            ResolveComponentResult::Ok { wasm, export, .. } => (wasm, export),
            ResolveComponentResult::Err { error } => {
                panic!("resolve of selector {selector:?} failed: {error}")
            }
        };
        let replies = self.call(
            Some(engine),
            "aether.component",
            &ReplaceComponent {
                mailbox_id,
                wasm,
                drain_timeout_ms: None,
                config: Vec::new(),
                // ADR-0096: thread the selector's `@actor` half, so a
                // `module@actor` replace instantiates the named export
                // instead of the trampoline's current hosted type.
                export,
            },
        );
        let payload = single_reply(&replies, "ReplaceComponent");
        match ReplaceResult::decode_from_bytes(&payload) {
            Some(ReplaceResult::Ok { capabilities }) => capabilities,
            Some(ReplaceResult::Err { error }) => {
                panic!("replace by selector {selector:?} failed: {error}")
            }
            None => panic!("undecodable ReplaceResult"),
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
            &LoadComponent { wasm, name: None, config: Vec::new(), export: None },
        );
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { mailbox_id, name, capabilities }) => Loaded { mailbox_id, addr: name, capabilities },
            Some(LoadResult::Err { error }) => panic!("load of {stem:?} failed: {error}"),
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Like [`load_full`](Self::load_full) but selects a specific actor type
    /// from a multi-actor bundle via `export` (ADR-0096, issue 1994). The
    /// `export` string is the actor's `NAMESPACE` const. Returns all three
    /// `LoadResult::Ok` fields as a [`Loaded`].
    pub fn load_full_export(&mut self, engine: EngineId, stem: &str, export: &str) -> Loaded {
        let wasm = read_component_wasm(stem);
        let replies = self.call(
            Some(engine),
            "aether.component",
            &LoadComponent { wasm, name: None, config: Vec::new(), export: Some(export.to_owned()) },
        );
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { mailbox_id, name, capabilities }) => Loaded { mailbox_id, addr: name, capabilities },
            Some(LoadResult::Err { error }) => {
                panic!("load of {stem:?}@{export:?} failed: {error}")
            }
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Terminate `engine` through the asserting `call`
    /// path — the agent-facing `terminate_substrate`, distinct from the
    /// `Drop`-only best-effort
    /// `terminate_quietly`. The engines cap
    /// removes the fleet entry synchronously before it replies, so a
    /// follow-up [`list_engines`](Self::list_engines) reflects the
    /// eviction with no heartbeat-eviction wait. Drops `engine` from the
    /// teardown set so `Drop` doesn't double-terminate it.
    pub fn terminate(&mut self, engine: EngineId) {
        let replies = self.call(None, "aether.fleet", &TerminateEngine { engine_id: engine.0.to_string() });
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
    pub fn replace(&mut self, engine: EngineId, mailbox_id: MailboxId, stem: &str) -> ComponentCapabilities {
        let wasm = read_component_wasm(stem);
        let replies = self.call(
            Some(engine),
            "aether.component",
            &ReplaceComponent {
                mailbox_id,
                wasm,
                drain_timeout_ms: None,
                config: Vec::new(),
                // Replace-by-stem reuses the trampoline's current hosted
                // type — no export selector.
                export: None,
            },
        );
        let payload = single_reply(&replies, "ReplaceComponent");
        match ReplaceResult::decode_from_bytes(&payload) {
            Some(ReplaceResult::Ok { capabilities }) => capabilities,
            Some(ReplaceResult::Err { error }) => panic!("replace with {stem:?} failed: {error}"),
            None => panic!("undecodable ReplaceResult"),
        }
    }

    /// Replace by stem like [`replace`](Self::replace), but select a
    /// specific exported actor type from a multi-actor replacement
    /// module via `export` (ADR-0096) — the `ReplaceComponent.export`
    /// twin of [`load_full_export`](Self::load_full_export). The
    /// `export` string is the target actor's `NAMESPACE`. Returns the
    /// instantiated export's advertised capabilities.
    pub fn replace_export(
        &mut self,
        engine: EngineId,
        mailbox_id: MailboxId,
        stem: &str,
        export: &str,
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
                export: Some(export.to_owned()),
            },
        );
        let payload = single_reply(&replies, "ReplaceComponent");
        match ReplaceResult::decode_from_bytes(&payload) {
            Some(ReplaceResult::Ok { capabilities }) => capabilities,
            Some(ReplaceResult::Err { error }) => {
                panic!("replace with {stem:?}@{export:?} failed: {error}")
            }
            None => panic!("undecodable ReplaceResult"),
        }
    }

    /// Tail `recipient`'s per-actor `ActorLogRing` (ADR-0081) on
    /// `engine`. `since: None` reads from the oldest retained entry;
    /// `Some(n)` returns only entries with `sequence > n` (the per-actor
    /// cursor). `contains` applies a case-sensitive message substring
    /// filter substrate-side. `max: 0` resolves to the substrate-default cap. The
    /// framework dispatch loop answers `LogTail` for every native actor
    /// and wasm trampoline, so `recipient` is any live mailbox path.
    pub fn log_tail(
        &mut self,
        engine: EngineId,
        recipient: &str,
        since: Option<u64>,
        contains: Option<String>,
    ) -> LogTailResult {
        let replies = self.call(Some(engine), recipient, &LogTail { max: 0, min_level: None, since, contains });
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
    /// Steady-state call: waits on the reply chain's `ReplyEnd` under the
    /// [`reply_cap`] backstop. The reply-wait is settlement-driven — a
    /// slow-but-healthy chain re-arms rather than dying on a wall-clock
    /// gate (issue 2064).
    fn call<K>(&mut self, engine: Option<EngineId>, mailbox: &str, request: &K) -> Vec<MailEnvelope>
    where
        K: Kind + Serialize,
    {
        self.call_with_budget(engine, mailbox, request, reply_cap(), "reply")
    }

    /// Write one `Call` frame and read until its `ReplyEnd`, returning the
    /// `ReplyEvent` envelopes seen in between and recording the call into
    /// [`calls`](Self::calls). Blocks on the reply, re-arming on each
    /// `READ_REARM` socket-timeout, until `budget` is exhausted — a
    /// genuine wedge, panicked with `gate` naming the latency class
    /// (cold-start vs reply), the cid, the mailbox, and the budget (issue
    /// 2064). A non-timeout read error (the connection genuinely failed) or
    /// a `ReplyEnd::Err` panics immediately, distinct from a wedge.
    fn call_with_budget<K>(
        &mut self,
        engine: Option<EngineId>,
        mailbox: &str,
        request: &K,
        budget: Duration,
        gate: &str,
    ) -> Vec<MailEnvelope>
    where
        K: Kind + Serialize,
    {
        let cid = self.next_cid;
        self.next_cid += 1;

        self.write_call(cid, engine, mailbox, K::ID, request.encode_into_bytes())
            .expect("test setup: writing a Call frame to the hub succeeds");

        let mut events: Vec<MailEnvelope> = Vec::new();
        let start = Instant::now();
        loop {
            match read_frame(&mut self.stream) {
                Ok(WireFrame::ReplyEvent { cid: got_cid, envelope }) => {
                    assert_eq!(got_cid, cid, "ReplyEvent cid mismatch");
                    events.push(envelope);
                }
                Ok(WireFrame::ReplyEnd { cid: got_cid, result }) => {
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
                Ok(other) => panic!("unexpected frame for call {cid}: {other:?}"),
                // Socket read-timeout: no frame yet. Re-arm until the
                // cumulative backstop is exhausted; a healthy chain settles
                // first, a genuine wedge trips the assert below.
                Err(e) if is_rearm_timeout(&e) => {
                    let waited = start.elapsed();
                    assert!(
                        waited < budget,
                        "fleetharness {gate} gate wedged: call {cid} to {mailbox:?} did not settle \
                         within the {budget:?} backstop — a healthy chain never reaches this cap, \
                         so this is a genuine deadlock/livelock",
                    );
                    eprintln!("fleetharness: {gate} call {cid} to {mailbox:?} slow: waited {waited:?}, extending");
                }
                // Any other read error is a real failure (connection closed,
                // decode), not a slow chain.
                Err(e) => panic!("test setup: reading a reply frame for call {cid} failed: {e}"),
            }
        }
    }

    fn handshake(&mut self) {
        write_frame(
            &mut self.stream,
            &WireFrame::Hello(Hello {
                wire_version: WIRE_VERSION,
                peer: PeerKind::Client { client_name: "fleetharness".into(), client_version: "0.0.1".into() },
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
                    to: MailboxAddress { engine, mailbox: mailbox_id_from_path(mailbox) },
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
        let payload = TerminateEngine { engine_id: engine.0.to_string() }.encode_into_bytes();
        if self.write_call(cid, None, "aether.fleet", TerminateEngine::ID, payload).is_err() {
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
    /// carrier the guest decodes as its `WasmActor::Config` (ADR-0090).
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
            &LoadComponent { wasm, name: None, config: config.encode_into_bytes(), export: None },
        );
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { name, .. }) => name,
            Some(LoadResult::Err { error }) => panic!("load of {stem:?} failed: {error}"),
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Like [`load_with_config`](Self::load_with_config) but also selects a
    /// specific actor type from a multi-actor bundle via `export` (ADR-0096,
    /// issue 1994). The `export` string is the actor's `NAMESPACE` const —
    /// `LoadComponent.export` routes the host to that type instead of the
    /// module's entry type. Returns the registered ADR-0099 lineage address.
    pub fn load_with_config_export<C>(&mut self, engine: EngineId, stem: &str, config: &C, export: &str) -> String
    where
        C: Kind + Serialize,
    {
        let wasm = read_component_wasm(stem);
        let replies = self.call(
            Some(engine),
            "aether.component",
            &LoadComponent { wasm, name: None, config: config.encode_into_bytes(), export: Some(export.to_owned()) },
        );
        let payload = single_reply(&replies, "LoadComponent");
        match LoadResult::decode_from_bytes(&payload) {
            Some(LoadResult::Ok { name, .. }) => name,
            Some(LoadResult::Err { error }) => {
                panic!("load of {stem:?}@{export:?} failed: {error}")
            }
            None => panic!("undecodable LoadResult"),
        }
    }

    /// Route a one-entry traced batch (`DispatchTraced`) to a forked
    /// engine's `aether.trace` mailbox and return the chassis-root
    /// [`MailId`] every dispatched envelope inherited plus the reply
    /// envelopes collected across the settlement window. Mirrors
    /// `aether-mcp`'s `send_mail_traced`, minus the round-2 trace-tree
    /// stitch: Tier-A asserts settlement (the `call` read
    /// already spans it — the server holds the wire `Call` open until
    /// chain settlement) and the collected replies, not the
    /// reconstructed tree.
    ///
    /// The leading `ReplyEvent` is the trace cap's synchronous
    /// `DispatchTracedAck::Ok` — its ordering is well-defined (the trace
    /// handler replies before the dispatched children run), so it is
    /// split off and decoded for the `root`, and the trailing events are
    /// the dispatched mail's correlated replies. Panics on an
    /// `Err`/undecodable ack, mirroring `single_reply`.
    pub fn send_traced<K>(&mut self, engine: EngineId, recipient: &str, mail: &K) -> (MailId, Vec<MailEnvelope>)
    where
        K: Kind + Serialize,
    {
        let batch = DispatchTraced {
            mails: vec![NamedMail {
                recipient_name: recipient.to_owned(),
                kind_name: K::NAME.to_owned(),
                payload: mail.encode_into_bytes(),
                count: 1,
            }],
        };
        let mut events = self.call(Some(engine), TRACE_MAILBOX_NAME, &batch);
        assert!(!events.is_empty(), "send_traced expected a DispatchTracedAck reply event, got none");
        let ack = events.remove(0);
        let root = match DispatchTracedAck::decode_from_bytes(&ack.payload) {
            Some(DispatchTracedAck::Ok { root }) => root,
            Some(DispatchTracedAck::Err { error }) => panic!("send_traced batch rejected: {error}"),
            None => panic!("undecodable DispatchTracedAck"),
        };
        (root, events)
    }
}

impl Drop for FleetHarness {
    fn drop(&mut self) {
        let engines = mem::take(&mut self.spawned);
        for engine in engines {
            self.terminate_quietly(engine);
        }
        // Best-effort: reap this harness's per-engine scratch dirs.
        let _ = fs::remove_dir_all(&self.store_root);
    }
}

/// Point this harness's forked substrates at a unique per-process scratch
/// root for their per-engine executable materialization (ADR-0115), so a
/// concurrent fork+exec test on the shared default
/// `dirs::data_dir()/aether/engines` root can't collide. Threaded into
/// `boot_hub` as `FleetConfig::fleet_store_root` (ADR-0090) — not an
/// env var — so the cap resolves it at `FleetServer::init`.
fn isolate_store_root() -> PathBuf {
    let temp_dir = env::temp_dir();
    loop {
        let nonce = STORE_ROOT_NONCE.fetch_add(1, Ordering::Relaxed);
        // Each `FleetHarness` in a process gets a fresh nonce-tagged root,
        // and `create_dir` claims it before `boot_hub` opens
        // `root/binaries`, so a concurrent same-process harness can't race on
        // the path.
        //
        // The hub's binary store (ADR-0115) is isolated under
        // `root/binaries` too, but that dir rides
        // `FleetConfig::binary_store_dir` (ADR-0090).
        let root = temp_dir.join(format!("aether-fleetharness-{}-{nonce}", process::id()));
        match fs::create_dir(&root) {
            Ok(()) => return root,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => {
                panic!("test setup: creating store root {} ({e})", root.display())
            }
        }
    }
}

pub fn allocate_store_root_for_test() -> PathBuf {
    isolate_store_root()
}

/// Boot a hub-shaped passive chassis: a forwarding `RpcServerCapability`
/// (engine-addressed Calls route through `aether.fleet`), the engines
/// cap, and `TraceDispatchCapability` so the `RpcServer`'s local Calls
/// settle and close. Returns the chassis and the port the RPC server
/// bound. Mirrors the seed's `boot_hub`. `binary_store_dir` isolates the
/// hub's content-addressed store (ADR-0115) per-harness, and
/// `fleet_store_root` isolates the per-engine spawn-dir parent (issue
/// 1274) per-harness — both via `FleetConfig` (ADR-0090); the heartbeat
/// stays disabled (the `Default`).
fn boot_hub(binary_store_dir: &Path, fleet_store_root: &Path) -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    let authority = boot_authority();
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&authority, d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor_configured::<FleetServer>(
            (),
            FleetConfig {
                binary_store_dir: Some(binary_store_dir.to_string_lossy().into_owned()),
                fleet_store_root: Some(fleet_store_root.to_string_lossy().into_owned()),
                ..FleetConfig::default()
            },
        )
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: "fleetharness-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
                route_target: Some(aether_data::mailbox_id_from_name("aether.fleet")),
            },
            RpcServerConfig { port: Some(0) },
        )
        .build_passive()
        .expect("test setup: hub caps boot");
    let port = chassis.handle::<RpcServerHandle>().expect("test setup: RpcServerHandle published").local_port;
    (chassis, port)
}

/// Exactly one `ReplyEvent` payload, panicking if a call that should
/// yield a single reply yielded zero or many.
fn single_reply(replies: &[MailEnvelope], label: &str) -> Vec<u8> {
    match replies {
        [one] => one.payload.clone(),
        other => panic!("{label} expected exactly one reply event, got {}", other.len()),
    }
}

/// Env override for the dist-tree resolution: an explicit path to the `dist/` tree
/// `cargo xtask dist` packages, for pointing the harness at a dist built
/// somewhere other than the running tree's workspace root.
pub const DIST_DIR_ENV: &str = "AETHER_DIST_DIR";

/// Workspace `dist/` directory, resolved at run time: the [`DIST_DIR_ENV`]
/// override when set, else `dist/` under the nearest ancestor of the current
/// directory that is a checkout root (holds a `Cargo.lock`).
///
/// Run time rather than `env!("CARGO_MANIFEST_DIR")`: the lane hosts share
/// one build directory across many checkouts, so a compiled test binary can
/// outlive the tree it was compiled in — and a compile-time path then names
/// a checkout whose `dist/` no longer exists, failing every scenario in the
/// member at once while a per-test replay of the same scenario passes. The
/// tree the test *runs in* is the one whose dist it must read, and cargo and
/// nextest run every test with the current directory inside its own package,
/// so the walk up from there lands on the right checkout root. The
/// compile-time path remains only as the last resort for a caller whose
/// current directory is outside any checkout.
fn dist_dir() -> PathBuf {
    if let Some(dir) = env::var_os(DIST_DIR_ENV) {
        return PathBuf::from(dir);
    }
    env::current_dir()
        .ok()
        .and_then(|current| runtime_dist_dir(&current))
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist"))
}

/// `dist/` under the nearest ancestor of `current` that is a checkout root
/// (holds a `Cargo.lock`), or `None` when no ancestor is one.
///
/// Nearest root, not nearest `dist/`: a checkout without a dist must resolve
/// to where its dist *would* be, so the missing-manifest panic names the
/// `cargo xtask dist` the caller actually needs to run — never a leftover
/// dist somewhere above the checkout.
fn runtime_dist_dir(current: &Path) -> Option<PathBuf> {
    current.ancestors().find(|dir| dir.join("Cargo.lock").is_file()).map(|root| root.join("dist"))
}

/// The chassis bin `FleetHarness` forks — the `dist/manifest.json` chassis
/// key and the `dist/bin/` filename `cargo xtask dist` packages.
const HEADLESS_BIN: &str = "aether-headless";

/// Env override for [`headless_bin_path`]: an explicit path to a
/// prebuilt headless chassis binary, bypassing the dist manifest — for
/// pointing the harness at a fresh `target/` build without re-running
/// `cargo xtask dist`.
pub const HEADLESS_BIN_ENV: &str = "AETHER_HARNESS_FLEET_HEADLESS_BIN";

/// Classify a raw `dist/manifest.json` for the presence of chassis bin
/// `bin` — the chassis-map twin of [`classify_dist_manifest`], reusing
/// the same classification (a bin name absent from the chassis map — a
/// `--no-bins` dist — classifies as [`DistManifestClassification::MissingStem`]).
pub fn classify_dist_chassis(raw: &str, bin: &str) -> DistManifestClassification {
    let manifest: ManifestView = match serde_json::from_str(raw) {
        Ok(manifest) => manifest,
        Err(_) => return DistManifestClassification::Unparseable,
    };
    manifest.chassis.get(bin).cloned().map_or(DistManifestClassification::MissingStem, |relative_path| {
        DistManifestClassification::Available { relative_path }
    })
}

/// The `aether-headless` binary [`FleetHarness::spawn_headless`]
/// forks: the [`HEADLESS_BIN_ENV`] override when set, else the chassis
/// entry in `dist/manifest.json` resolved under `dist/` — the tree
/// `cargo xtask dist` packages. `CARGO_BIN_EXE_*` is not an option here:
/// it only resolves inside the package that defines the binary, and this
/// harness is a crate any scenario suite can dev-dep.
///
/// # Panics
/// Panics with a `cargo xtask dist` hint when neither resolves — the
/// harness cannot fork a chassis it cannot locate, so the failure is
/// loud and actionable rather than a skip (mirroring the
/// `AETHER_REQUIRE_RUNTIME` strict style of the wasm-side guard).
pub fn headless_bin_path() -> PathBuf {
    if let Ok(path) = env::var(HEADLESS_BIN_ENV) {
        return PathBuf::from(path);
    }
    chassis_bin_path(HEADLESS_BIN)
}

/// Resolve any chassis binary by its `dist/manifest.json` chassis-map
/// name (issue #3812) — the general form [`headless_bin_path`] wraps.
/// A test that needs a chassis binary other than the forked headless
/// engine (e.g. the hub store's distinct-second-upload subject) resolves
/// it here instead of `CARGO_BIN_EXE_*`, which only resolves inside the
/// package that defines the binary.
///
/// # Panics
/// Panics with a `cargo xtask dist` hint when the manifest or the bin
/// entry is missing — the caller cannot proceed without the binary, so
/// the failure is loud and actionable rather than a skip.
pub fn chassis_bin_path(bin: &str) -> PathBuf {
    let dist = dist_dir();
    let manifest_path = dist.join("manifest.json");
    let unavailable = |detail: &str| -> ! {
        panic!(
            "chassis binary {bin:?} unavailable via {}; {detail}; run `cargo xtask dist` to build the chassis bins + \
             manifest",
            manifest_path.display(),
        )
    };
    let raw = match fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => unavailable("manifest is absent"),
        Err(e) => unavailable(&format!("manifest is unreadable ({e})")),
    };
    match classify_dist_chassis(&raw, bin) {
        DistManifestClassification::Available { relative_path } => dist.join(relative_path),
        DistManifestClassification::MissingStem => {
            unavailable("bin is missing from the manifest's chassis map (a `--no-bins` dist?)")
        }
        DistManifestClassification::Unparseable => unavailable("manifest does not parse as dist metadata"),
    }
}

pub fn classify_dist_manifest(raw: &str, stem: &str) -> DistManifestClassification {
    let manifest: ManifestView = match serde_json::from_str(raw) {
        Ok(manifest) => manifest,
        Err(_) => return DistManifestClassification::Unparseable,
    };
    manifest.components.get(stem).cloned().map_or(DistManifestClassification::MissingStem, |relative_path| {
        DistManifestClassification::Available { relative_path }
    })
}

fn dist_component_message(stem: &str, manifest_path: &Path, detail: &str) -> String {
    format!(
        "component stem {stem:?} unavailable via {}; {detail}; run `cargo xtask dist` to build the component wasm + manifest, then remove generated `dist/` once it is no longer needed",
        manifest_path.display(),
    )
}

fn require_runtime_enabled() -> bool {
    env::var("AETHER_REQUIRE_RUNTIME").is_ok()
}

pub fn dist_component_guard_outcome(
    stem: &str,
    manifest_path: &Path,
    classification: &DistManifestClassification,
    require_runtime: bool,
) -> DistComponentGuardOutcome {
    let unavailable = |detail: &str| {
        let message = dist_component_message(stem, manifest_path, detail);
        if require_runtime {
            DistComponentGuardOutcome::RequireRuntime(message)
        } else {
            DistComponentGuardOutcome::Skip(message)
        }
    };
    match classification {
        DistManifestClassification::Available { .. } => DistComponentGuardOutcome::Available,
        DistManifestClassification::MissingStem => unavailable("stem is missing from the manifest"),
        DistManifestClassification::Unparseable => {
            unavailable("manifest is unreadable or does not parse as dist metadata")
        }
    }
}

fn dist_component_requirement(stem: &str) -> DistComponentRequirement {
    let dist = dist_dir();
    let manifest_path = dist.join("manifest.json");
    let raw = match fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return DistComponentRequirement::ManifestAbsent;
        }
        Err(e) => return DistComponentRequirement::ManifestUnreadable(e.to_string()),
    };
    match classify_dist_manifest(&raw, stem) {
        DistManifestClassification::Available { relative_path } => {
            DistComponentRequirement::Available(dist.join(relative_path))
        }
        DistManifestClassification::MissingStem => DistComponentRequirement::StemMissing,
        DistManifestClassification::Unparseable => {
            DistComponentRequirement::ManifestUnreadable("manifest does not parse as dist metadata".to_owned())
        }
    }
}

fn dist_component_guard_for_requirement(
    stem: &str,
    manifest_path: &Path,
    requirement: DistComponentRequirement,
    require_runtime: bool,
) -> DistComponentGuardOutcome {
    let unavailable = |detail: &str| {
        let message = dist_component_message(stem, manifest_path, detail);
        if require_runtime {
            DistComponentGuardOutcome::RequireRuntime(message)
        } else {
            DistComponentGuardOutcome::Skip(message)
        }
    };
    match requirement {
        DistComponentRequirement::Available(_) => DistComponentGuardOutcome::Available,
        DistComponentRequirement::ManifestAbsent => unavailable("manifest is absent"),
        DistComponentRequirement::ManifestUnreadable(detail) => unavailable(&detail),
        DistComponentRequirement::StemMissing => {
            dist_component_guard_outcome(stem, manifest_path, &DistManifestClassification::MissingStem, require_runtime)
        }
    }
}

fn component_wasm_path_result(stem: &str) -> Result<PathBuf, String> {
    let manifest_path = dist_dir().join("manifest.json");
    match dist_component_requirement(stem) {
        DistComponentRequirement::Available(path) => Ok(path),
        DistComponentRequirement::ManifestAbsent => {
            Err(dist_component_message(stem, &manifest_path, "manifest is absent"))
        }
        DistComponentRequirement::ManifestUnreadable(detail) => {
            Err(dist_component_message(stem, &manifest_path, &detail))
        }
        DistComponentRequirement::StemMissing => {
            Err(dist_component_message(stem, &manifest_path, "stem is missing from the manifest"))
        }
    }
}

/// Stem-aware dist precondition guard for `FleetHarness` tests that load
/// component wasm. Returns `true` when the manifest exists, parses, and
/// contains `stem`; returns `false` after printing a `skipping:` line
/// locally when the precondition is not met. Under
/// `AETHER_REQUIRE_RUNTIME`, the same actionable message panics so a
/// missing or stale `cargo xtask dist` pre-build can't pass silently.
#[must_use]
pub fn dist_component_available(stem: &str) -> bool {
    let manifest_path = dist_dir().join("manifest.json");
    match dist_component_guard_for_requirement(
        stem,
        &manifest_path,
        dist_component_requirement(stem),
        require_runtime_enabled(),
    ) {
        DistComponentGuardOutcome::Available => true,
        DistComponentGuardOutcome::Skip(message) => {
            eprintln!("skipping: {message}");
            false
        }
        DistComponentGuardOutcome::RequireRuntime(message) => panic!("{message}"),
    }
}

/// Read a component wasm by stem through `dist/manifest.json` (the
/// #1445 dist tree). Panics with a `cargo xtask dist` hint if the
/// manifest is absent — the harness can't locate component wasm without
/// it. Tests guard their load path with [`dist_component_available`] so a
/// missing manifest or stale stem skips rather than reaching this panic.
pub fn read_component_wasm(stem: &str) -> Vec<u8> {
    let wasm_path = component_wasm_path(stem);
    fs::read(&wasm_path).unwrap_or_else(|e| panic!("reading component wasm {} ({e})", wasm_path.display()))
}

/// The absolute on-disk path of a component wasm by stem, resolved through
/// `dist/manifest.json` (the #1445 dist tree) — the staged path the
/// ADR-0116 `upload_component` reads. Tests guard with
/// [`dist_component_available`] before reaching this. Panics with a
/// `cargo xtask dist` hint if the manifest is absent, unreadable, or the stem is
/// missing.
pub fn component_wasm_path(stem: &str) -> PathBuf {
    component_wasm_path_result(stem).unwrap_or_else(|message| panic!("{message}"))
}

/// Unit tests over the harness-owned pure helpers: the latency-budget
/// decisions the re-arm read loop owns (issue 2064) and the dist
/// manifest classification / skip-or-panic guard. The wire round-trip
/// and the cold-start/reply backstops themselves are exercised by the
/// booted `FleetHarness` scenarios (`fleetharness_spawn`, `fleetharness_mail`,
/// … in the per-cap crates: `aether-fleet`, `aether-rpc`, …).
#[cfg(test)]
mod tests {
    use std::io::{Error as IoError, ErrorKind};
    use std::path::Path;
    use std::time::Duration;
    use std::{env, fs, process};

    use aether_codec::frame::FrameError;

    use crate::{
        DistComponentGuardOutcome, DistManifestClassification, HEADLESS_BIN, cap_from_secs, classify_dist_chassis,
        classify_dist_manifest, dist_component_guard_outcome, is_rearm_timeout, runtime_dist_dir,
    };

    #[test]
    fn cap_from_secs_maps_seconds_and_zero_sentinel() {
        // The only logic here: seconds → `Duration`, with `0` as the
        // "wait forever" sentinel (the gate's `elapsed >= cap` never trips).
        assert_eq!(cap_from_secs(0), Duration::MAX, "0 → wait forever");
        assert_eq!(cap_from_secs(60), Duration::from_mins(1));
        assert_eq!(cap_from_secs(300), Duration::from_mins(5));
    }

    #[test]
    fn rearm_timeout_classification() {
        // A timed-out blocking read re-arms; both platform spellings count.
        assert!(is_rearm_timeout(&FrameError::Io(IoError::from(ErrorKind::WouldBlock))));
        assert!(is_rearm_timeout(&FrameError::Io(IoError::from(ErrorKind::TimedOut))));
        // A genuine failure does not re-arm — it propagates as an error.
        assert!(!is_rearm_timeout(&FrameError::Io(IoError::from(ErrorKind::UnexpectedEof))));
        assert!(!is_rearm_timeout(&FrameError::Io(IoError::from(ErrorKind::ConnectionReset))));
    }

    #[test]
    fn manifest_with_bundle_stem_is_available() {
        let manifest = r#"{
            "components": {
                "aether_test_fixtures_bundle": "components/aether_test_fixtures_bundle.wasm"
            }
        }"#;
        assert!(matches!(
            classify_dist_manifest(manifest, "aether_test_fixtures_bundle"),
            DistManifestClassification::Available { .. }
        ));
    }

    #[test]
    fn manifest_missing_bundle_stem_is_classified() {
        let manifest = r#"{
            "components": {
                "aether_kit_commons": "components/aether_kit_commons.wasm"
            }
        }"#;
        assert_eq!(
            classify_dist_manifest(manifest, "aether_test_fixtures_bundle"),
            DistManifestClassification::MissingStem,
        );
    }

    #[test]
    fn manifest_chassis_map_classifies_headless_bin() {
        // The chassis-map twin of the component classification: the
        // headless entry resolves to its `bin/` relative path, and a
        // `--no-bins` manifest (empty or absent chassis map) classifies
        // as missing rather than panicking at parse.
        let with_bins = r#"{
            "components": {},
            "chassis": {
                "aether-headless": "bin/aether-headless"
            }
        }"#;
        assert_eq!(
            classify_dist_chassis(with_bins, HEADLESS_BIN),
            DistManifestClassification::Available { relative_path: "bin/aether-headless".to_owned() },
        );
        let no_bins = r#"{ "components": {}, "chassis": {} }"#;
        assert_eq!(classify_dist_chassis(no_bins, HEADLESS_BIN), DistManifestClassification::MissingStem);
        let chassis_absent = r#"{ "components": {} }"#;
        assert_eq!(classify_dist_chassis(chassis_absent, HEADLESS_BIN), DistManifestClassification::MissingStem);
    }

    #[test]
    fn missing_stem_skips_locally_and_panics_when_runtime_is_required() {
        let manifest_path = Path::new("/tmp/dist/manifest.json");
        let local = dist_component_guard_outcome(
            "aether_test_fixtures_bundle",
            manifest_path,
            &DistManifestClassification::MissingStem,
            false,
        );
        let required = dist_component_guard_outcome(
            "aether_test_fixtures_bundle",
            manifest_path,
            &DistManifestClassification::MissingStem,
            true,
        );

        let DistComponentGuardOutcome::Skip(skip_message) = local else {
            panic!("missing stem should skip locally");
        };
        let DistComponentGuardOutcome::RequireRuntime(panic_message) = required else {
            panic!("missing stem should require runtime under AETHER_REQUIRE_RUNTIME");
        };
        assert_eq!(skip_message, panic_message);
        assert!(skip_message.contains("aether_test_fixtures_bundle"));
        assert!(skip_message.contains("/tmp/dist/manifest.json"));
        assert!(skip_message.contains("cargo xtask dist"));
        assert!(skip_message.contains("remove generated `dist/`"));
    }

    #[test]
    fn dist_resolves_under_the_checkout_the_caller_runs_in() {
        // The lane hosts share one build directory across many checkouts, so
        // a test binary can run in a tree it was not compiled in — dispatch
        // 3177 failed 84 scenarios at once on a compile-time dist path into a
        // checkout whose dist was gone. Resolution walks up from the caller's
        // directory instead: the nearest checkout root wins, and a directory
        // outside any checkout resolves to nothing rather than to a guess.
        let scratch = env::temp_dir().join(format!("aether-dist-resolve-{}", process::id()));
        let checkout = scratch.join("outer").join("checkout");
        let nested = checkout.join("crates").join("some-crate");
        fs::create_dir_all(&nested).expect("scratch tree");
        fs::write(checkout.join("Cargo.lock"), "").expect("checkout marker");

        assert_eq!(runtime_dist_dir(&nested), Some(checkout.join("dist")));
        assert_eq!(runtime_dist_dir(&scratch), None, "no ancestor checkout, no resolution");

        fs::remove_dir_all(&scratch).expect("scratch removed");
    }
}
