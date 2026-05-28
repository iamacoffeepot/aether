//! ADR-0047 §4 DAG executor (iamacoffeepot/aether#976).
//!
//! [`Executor`] is the substrate-side machinery that drives a validated
//! [`DagDescriptor`] to completion. It lives inside the `aether.dag`
//! cap (a single-threaded [`NativeActor`](crate::actor::native::NativeActor)),
//! so every method here runs on that one dispatcher thread — no locks,
//! plain `&mut self` state.
//!
//! ## Dispatch model (ADR-0047 §4)
//!
//! At submit the executor mints one ephemeral [`HandleId`] per node,
//! returns them in the `submit_result` so downstream `Ref::Handle`
//! slots can be stamped, then begins execution:
//!
//! - **Sources** dispatch immediately. Each is sent to its mailbox via
//!   the inherited send path with reply correlation routed back to the
//!   executor's own mailbox (not the submitter). The reply resolves the
//!   source's handle in the [`HandleStore`].
//! - **Observers** dispatch *eagerly* at submit with `Ref::Handle`
//!   slots into their input handle ids. The substrate's parking table
//!   ([`HandleStore::park`] via [`Mailer::push`]) gates them: the mail
//!   parks on the first unresolved handle and re-routes when
//!   [`Mailer::resolve_handle`] flushes it — so a multi-input observer
//!   naturally waits for every slot.
//! - **`Call`s** gate on an explicit `pending_inputs` counter rather
//!   than the parking table: a `Call` must dispatch as *its own causal
//!   root* (`send_envelope_as_root`) so the executor can subscribe to
//!   `Settled { call_root }` and accumulate the cap's correlated replies
//!   into an ordered [`Bundle`]. The bundle closes exactly on
//!   settlement (no quiescence window — ADR-0047 §2 rev 2026-05-20,
//!   the hold contract from iamacoffeepot/aether#1031 guarantees the
//!   counter never transiently zeroes with work still coming). A
//!   per-`Call` timeout bounds a never-settling cap.
//!
//! The cap wraps the executor and forwards: `submit` → [`Executor::submit`],
//! source/`Call` reply landings → [`Executor::on_reply`], `Settled`
//! notifications → [`Executor::on_settled`], `aether.dag.cancel` →
//! [`Executor::cancel`], `aether.dag.status` → [`Executor::status`],
//! and the reaping tick → [`Executor::reap`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use aether_data::canonical::canonical_kind_bytes;
use aether_data::{
    DagId, HandleId, KindId, MailId, MailboxId, Ref, SchemaType, Tag, TransformId,
    content_addressed_handle_id, with_tag,
};
use aether_kinds::{
    Bundle, BundleElement, CancelResult, DagDescriptor, DagTransformDone, Node, NodeId,
    StatusResult, trace::Settled,
};

use crate::actor::native::NativeCtx;
use crate::dag::state::{CallBuffer, DagState, DagStatus};
use crate::dag::transform_pool::{TransformOutcome, TransformPool};
use crate::dag::transform_registry::TransformRegistry;
use crate::dag::validator::validate;
use crate::handle_store::HandleStore;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;

use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::thread;

const TARGET: &str = "aether::dag::executor";

/// Env override for the completed-DAG retention window (ADR-0047 §7).
/// Default [`DEFAULT_RETENTION_COMPLETE_MS`].
pub const ENV_RETENTION_COMPLETE_MS: &str = "AETHER_DAG_RETENTION_COMPLETE_MS";
/// Env override for the failed-DAG retention window (ADR-0047 §7).
/// Default [`DEFAULT_RETENTION_FAILED_MS`].
pub const ENV_RETENTION_FAILED_MS: &str = "AETHER_DAG_RETENTION_FAILED_MS";
/// Env override for the per-`Call` settlement timeout (ADR-0047 §4 —
/// never-settling caps). Default [`DEFAULT_CALL_TIMEOUT_MS`].
pub const ENV_CALL_TIMEOUT_MS: &str = "AETHER_DAG_CALL_TIMEOUT_MS";
/// Env override for the default per-`Transform` wall-clock deadline
/// (ADR-0048 §3/§6). A `Node::Transform.timeout_ms` overrides per node.
/// Default [`DEFAULT_TRANSFORM_TIMEOUT_MS`].
pub const ENV_TRANSFORM_TIMEOUT_MS: &str = "AETHER_TRANSFORM_TIMEOUT_MS";
/// Env override for the transform output-byte cap (ADR-0048 §6).
/// Default [`DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES`].
pub const ENV_TRANSFORM_MAX_OUTPUT_BYTES: &str = "AETHER_TRANSFORM_MAX_OUTPUT_BYTES";
/// Env override for the transform compute-pool thread count (ADR-0048
/// §3). Default: available parallelism (clamped to ≥ 1).
pub const ENV_TRANSFORM_POOL_THREADS: &str = "AETHER_TRANSFORM_POOL_THREADS";

/// Default completed-DAG retention before reaping (ADR-0047 §7).
pub const DEFAULT_RETENTION_COMPLETE_MS: u64 = 60_000;
/// Default failed-DAG retention before reaping (ADR-0047 §7).
pub const DEFAULT_RETENTION_FAILED_MS: u64 = 300_000;
/// Default per-`Call` settlement timeout — bounds a cap that never
/// settles (never replies or streams forever). On expiry the `Call`
/// node fails (ADR-0047 §4).
pub const DEFAULT_CALL_TIMEOUT_MS: u64 = 30_000;
/// Default per-`Transform` wall-clock deadline (ADR-0048 §3/§6). A
/// native thread can't be preempted, so on expiry the executor fails
/// the node + orphans the runaway thread.
pub const DEFAULT_TRANSFORM_TIMEOUT_MS: u64 = 30_000;
/// Default transform output-byte cap (ADR-0048 §6). Encoded output
/// exceeding this hard-fails the node.
pub const DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Resolved executor tuning knobs (ADR-0047 §4/§7, ADR-0048 §3/§6), read
/// once at [`Executor::new`]. Extracted from the formerly-inline env reads
/// (issue #1254) so the resolution lives in one named place. Chassis code
/// builds it via [`ExecutorConfig::from_env`]; tests can build it directly
/// without touching process env (issue 464).
#[derive(Clone, Copy, Debug)]
pub struct ExecutorConfig {
    /// Per-`Call` settlement timeout in ms (ADR-0047 §4).
    pub call_timeout_ms: u64,
    /// Default per-`Transform` wall-clock deadline in ms (ADR-0048 §3/§6).
    pub transform_timeout_ms: u64,
    /// Transform output-byte cap (ADR-0048 §6).
    pub transform_max_output_bytes: u64,
    /// Completed-DAG retention window in ms (ADR-0047 §7).
    pub retention_complete_ms: u64,
    /// Failed/cancelled-DAG retention window in ms (ADR-0047 §7).
    pub retention_failed_ms: u64,
    /// Transform compute-pool thread count (ADR-0048 §3). Resolved at
    /// [`ExecutorConfig::from_env`] from available parallelism when unset.
    pub pool_threads: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            call_timeout_ms: DEFAULT_CALL_TIMEOUT_MS,
            transform_timeout_ms: DEFAULT_TRANSFORM_TIMEOUT_MS,
            transform_max_output_bytes: DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES,
            retention_complete_ms: DEFAULT_RETENTION_COMPLETE_MS,
            retention_failed_ms: DEFAULT_RETENTION_FAILED_MS,
            pool_threads: default_pool_threads(),
        }
    }
}

impl ExecutorConfig {
    /// Resolve every knob from its `AETHER_*` env var. Chassis-internal
    /// edge only.
    ///
    /// Resolution runs through confique (ADR-0090): the private
    /// `ExecutorConfigLayer` declares each knob's env key + default in one
    /// place. Behaviour is byte-identical to the prior inline reads — an
    /// unparseable numeric value still falls back to its default, and the
    /// pool-thread count still defaults to available parallelism (clamped
    /// ≥ 1) when unset. The hard-error stance (ADR-0090 §4) lands with the
    /// chassis-env validation pass.
    ///
    /// # Panics
    ///
    /// Panics only if the layer's literal defaults are themselves
    /// malformed — a programmer error caught by the
    /// `executor_config_layer_defaults_match` test, never a runtime config
    /// fault (the env values flow through total parsers).
    #[must_use]
    pub fn from_env() -> Self {
        use confique::Config as _;

        let layer = ExecutorConfigLayer::builder()
            .env()
            .load()
            .expect("ExecutorConfigLayer defaults are well-formed");
        Self {
            call_timeout_ms: layer.call_timeout_ms,
            transform_timeout_ms: layer.transform_timeout_ms,
            transform_max_output_bytes: layer.transform_max_output_bytes,
            retention_complete_ms: layer.retention_complete_ms,
            retention_failed_ms: layer.retention_failed_ms,
            // Runtime-computed default (not a literal confique can hold):
            // unset *or* unparseable → available parallelism (clamped ≥ 1).
            // A parseable value (incl `0`) is honoured verbatim — the prior
            // `parse_env_usize` disposition. Captured as a raw string so
            // confique's parse path can't hard-error on bad input.
            pool_threads: layer
                .pool_threads
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or_else(default_pool_threads),
        }
    }
}

/// The transform compute-pool default: available parallelism, clamped to
/// ≥ 1. Used when `AETHER_TRANSFORM_POOL_THREADS` is unset.
fn default_pool_threads() -> usize {
    thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Env-shaped confique layer behind [`ExecutorConfig`] (ADR-0090). Each
/// numeric knob carries a literal default; `pool_threads` is `Option`
/// because its default is runtime-computed (available parallelism), not a
/// literal. Kept private — the consumed shape stays `ExecutorConfig`.
#[derive(confique::Config)]
struct ExecutorConfigLayer {
    #[config(
        env = "AETHER_DAG_CALL_TIMEOUT_MS",
        parse_env = parse_call_timeout_ms,
        default = 30_000u64
    )]
    call_timeout_ms: u64,
    #[config(
        env = "AETHER_TRANSFORM_TIMEOUT_MS",
        parse_env = parse_transform_timeout_ms,
        default = 30_000u64
    )]
    transform_timeout_ms: u64,
    #[config(
        env = "AETHER_TRANSFORM_MAX_OUTPUT_BYTES",
        parse_env = parse_transform_max_output_bytes,
        default = 67_108_864u64
    )]
    transform_max_output_bytes: u64,
    #[config(
        env = "AETHER_DAG_RETENTION_COMPLETE_MS",
        parse_env = parse_retention_complete_ms,
        default = 60_000u64
    )]
    retention_complete_ms: u64,
    #[config(
        env = "AETHER_DAG_RETENTION_FAILED_MS",
        parse_env = parse_retention_failed_ms,
        default = 300_000u64
    )]
    retention_failed_ms: u64,
    /// Held as the raw string so `ExecutorConfig::from_env` can apply the
    /// soft `.parse().ok()` then fall back to available parallelism on an
    /// unset *or* unparseable value (a confique numeric field would
    /// hard-error on bad input). A parseable value (incl `0`) is honoured
    /// verbatim, byte-identical to the prior `parse_env_usize` read.
    #[config(env = "AETHER_TRANSFORM_POOL_THREADS")]
    pool_threads: Option<String>,
}

// confique's `parse_env` contract is `fn(&str) -> Result<T, impl Error>`,
// so these total helpers carry a `Result` they never fill with `Err` — an
// unparseable value folds back to the same default as the prior inline
// `parse_env_u64` / `parse_env_usize` reads (the warn-on-malformed log is
// dropped, the disposition is byte-identical). The strict (erroring)
// variants land with the ADR-0090 §4 validation pass; hence the per-fn
// `unnecessary_wraps` allow.

/// Parse the per-`Call` timeout; unparseable → [`DEFAULT_CALL_TIMEOUT_MS`].
#[allow(clippy::unnecessary_wraps)]
fn parse_call_timeout_ms(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_CALL_TIMEOUT_MS))
}

/// Parse the transform deadline; unparseable →
/// [`DEFAULT_TRANSFORM_TIMEOUT_MS`].
#[allow(clippy::unnecessary_wraps)]
fn parse_transform_timeout_ms(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_TRANSFORM_TIMEOUT_MS))
}

/// Parse the transform output cap; unparseable →
/// [`DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES`].
#[allow(clippy::unnecessary_wraps)]
fn parse_transform_max_output_bytes(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES))
}

/// Parse the completed-DAG retention; unparseable →
/// [`DEFAULT_RETENTION_COMPLETE_MS`].
#[allow(clippy::unnecessary_wraps)]
fn parse_retention_complete_ms(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_RETENTION_COMPLETE_MS))
}

/// Parse the failed-DAG retention; unparseable →
/// [`DEFAULT_RETENTION_FAILED_MS`].
#[allow(clippy::unnecessary_wraps)]
fn parse_retention_failed_ms(s: &str) -> Result<u64, Infallible> {
    Ok(s.parse().unwrap_or(DEFAULT_RETENTION_FAILED_MS))
}

/// What a landed reply correlates to (ADR-0047 §4). Sources resolve a
/// stored handle and flush downstream; `Call`s accumulate into a
/// bundle that closes on settlement.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum NodeRole {
    Source,
    Call,
}

/// One outstanding reply correlation the executor is waiting on.
#[derive(Copy, Clone, Debug)]
struct Pending {
    dag_id: DagId,
    node_id: NodeId,
    handle_id: HandleId,
    role: NodeRole,
}

/// One in-flight `Call` awaiting settlement, for the per-`Call` timeout
/// sweep (ADR-0047 §4 never-settling caps).
#[derive(Copy, Clone, Debug)]
struct InFlightCall {
    dag_id: DagId,
    node_id: NodeId,
    deadline: Instant,
}

/// One off-thread `Transform` invocation in flight, for the per-call
/// deadline sweep (ADR-0048 §3/§6). The pool's wake mail resolves the
/// node when the `fn` returns; the reaper fails it if the deadline
/// passes first (the runaway thread is then orphaned).
#[derive(Copy, Clone, Debug)]
struct InFlightTransform {
    dag_id: DagId,
    node_id: NodeId,
    handle_id: HandleId,
    /// Content-addressed id the output is stored under on success.
    content_id: HandleId,
    output_kind_id: KindId,
    deadline: Instant,
    /// The deadline expressed in millis, for the timeout diagnostic.
    timeout_ms: u64,
}

/// The DAG executor. Holds every live + recently-terminal DAG plus the
/// reply-correlation table the cap routes landings through.
pub struct Executor {
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    /// Monotonic per-substrate counter behind every minted [`DagId`].
    next_dag: u64,
    /// Live + recently-terminal DAGs, keyed by id.
    dags: HashMap<DagId, DagState>,
    /// Reply-correlation table: the correlation minted on a source /
    /// `Call` dispatch maps back to the node it resolves.
    pending: HashMap<u64, Pending>,
    /// In-flight `Call`s with a settlement deadline, swept by [`Self::reap`].
    in_flight_calls: HashMap<u64, InFlightCall>,
    /// In-flight off-thread `Transform`s, keyed by the pool's `job_id`,
    /// swept for deadline by [`Self::reap`] and resolved on the pool's
    /// `DagTransformDone` wake (ADR-0048 §3).
    in_flight_transforms: HashMap<u64, InFlightTransform>,
    /// Cached per-`Call` settlement timeout.
    call_timeout: Duration,
    /// Cached default per-`Transform` deadline (ADR-0048 §3).
    transform_timeout: Duration,
    /// Cached transform output-byte cap (ADR-0048 §6).
    transform_max_output_bytes: usize,
    /// Cached completed-DAG retention window.
    retention_complete: Duration,
    /// Cached failed/cancelled-DAG retention window.
    retention_failed: Duration,
    /// Native-transform registry, shared with the validator's submit
    /// path (ADR-0048 §2).
    transform_registry: Arc<TransformRegistry>,
    /// Dedicated transform compute pool (ADR-0048 §3).
    transform_pool: TransformPool,
}

impl Executor {
    /// Build a fresh executor bound to the cap's `Arc<Mailer>` + own
    /// mailbox id. Takes the resolved [`ExecutorConfig`] (chassis builds it
    /// via [`ExecutorConfig::from_env`]; tests build it directly), builds
    /// the native-transform registry from the link-time inventory
    /// (ADR-0048 §2), and spins up the dedicated transform compute pool
    /// (ADR-0048 §3).
    #[must_use]
    pub fn new(mailer: Arc<Mailer>, self_mailbox: MailboxId, config: ExecutorConfig) -> Self {
        let transform_pool = TransformPool::new(config.pool_threads, &mailer);
        Self {
            mailer,
            self_mailbox,
            next_dag: 1,
            dags: HashMap::new(),
            pending: HashMap::new(),
            in_flight_calls: HashMap::new(),
            in_flight_transforms: HashMap::new(),
            call_timeout: Duration::from_millis(config.call_timeout_ms),
            transform_timeout: Duration::from_millis(config.transform_timeout_ms),
            transform_max_output_bytes: usize::try_from(config.transform_max_output_bytes)
                .unwrap_or(usize::MAX),
            retention_complete: Duration::from_millis(config.retention_complete_ms),
            retention_failed: Duration::from_millis(config.retention_failed_ms),
            transform_registry: Arc::new(TransformRegistry::from_inventory()),
            transform_pool,
        }
    }

    /// Shut the transform compute pool down (join its workers). Called
    /// from the cap's `unwire`.
    pub fn shutdown(&mut self) {
        self.transform_pool.shutdown();
    }

    /// Test-only: total native-transform `invoke` calls the compute pool
    /// has started. Cache hits never reach the pool, so this excludes
    /// them — the `transform_skips_invoke_on_cache_hit` fixture asserts
    /// it stays at 1 across two DAGs with the same content-address
    /// (ADR-0048 §4, iamacoffeepot/aether#982).
    #[must_use]
    pub fn transform_call_count(&self) -> usize {
        self.transform_pool.invoke_count()
    }

    /// Borrow the handle store the executor publishes resolved values
    /// into. Sourced from the cap's `Arc<Mailer>`.
    fn store(&self) -> &Arc<HandleStore> {
        self.mailer.handle_store()
    }

    /// Mint the next [`DagId`] (ADR-0047 §4: monotonic-per-substrate
    /// with the `Tag::Dag` discriminator).
    fn mint_dag_id(&mut self) -> DagId {
        let counter = self.next_dag;
        self.next_dag = self.next_dag.wrapping_add(1);
        if self.next_dag == 0 {
            self.next_dag = 1;
        }
        DagId(with_tag(Tag::Dag, counter))
    }

    /// Submit one DAG. Validates synchronously (ADR-0047 §1/§3); on
    /// success mints the [`DagId`], allocates one handle per node,
    /// kicks off source dispatch + eager observer dispatch + zero-input
    /// `Call` dispatch, and returns the `(dag_id, output_handles)`. On a
    /// validation failure returns the structured
    /// [`DagError`](aether_kinds::DagError) and dispatches nothing.
    ///
    /// The returned [`SubmitOutcome`] is the wire `SubmitResult` shape;
    /// the cap replies it verbatim.
    ///
    /// Takes the descriptor by value — the cap owns it off the decoded
    /// `Submit` mail and the validator clones it into the `ValidatedDag`
    /// it hands back, so owning here keeps the call site clean.
    #[allow(clippy::needless_pass_by_value)]
    pub fn submit(&mut self, ctx: &mut NativeCtx<'_>, descriptor: DagDescriptor) -> SubmitOutcome {
        let validated = {
            let registry = self.mailer.registry();
            let caps = self.mailer.capability_registry();
            match validate(&descriptor, registry, caps, Some(&self.transform_registry)) {
                Ok(v) => v,
                Err(error) => return SubmitOutcome::Err { error },
            }
        };

        let dag_id = self.mint_dag_id();

        // Allocate one ephemeral handle per node, holding a ref on
        // behalf of the DAG (ADR-0047 §1: handle ids available before
        // sources dispatch). Released on reaping / cancellation.
        let mut handles: HashMap<NodeId, HandleId> = HashMap::new();
        for node in &validated.descriptor.nodes {
            let id = self.store().next_ephemeral();
            self.store().inc_ref(id);
            handles.insert(node.id(), id);
        }

        let output_handles = {
            let state = DagState::new(dag_id, validated, handles);
            let outputs = state.output_handles();
            self.dags.insert(dag_id, state);
            outputs
        };

        // Begin execution. Order: sources first (so a zero-input `Call`
        // / `Transform` / observer with already-resolved inputs sees
        // them), then observers (park), then `Call`s + `Transform`s
        // (gate / dispatch if no inputs).
        self.dispatch_sources(ctx, dag_id);
        self.dispatch_observers(ctx, dag_id);
        self.dispatch_ready_calls(ctx, dag_id);
        self.dispatch_ready_transforms(ctx, dag_id);

        SubmitOutcome::Ok {
            dag_id,
            output_handles,
        }
    }

    /// Dispatch every `Source` node of `dag_id`: send its opaque payload
    /// to its mailbox with reply correlation routed back here, and stash
    /// the correlation so the reply resolves the source's handle.
    fn dispatch_sources(&mut self, ctx: &mut NativeCtx<'_>, dag_id: DagId) {
        let Some(state) = self.dags.get(&dag_id) else {
            return;
        };
        let sources: Vec<(NodeId, MailboxId, KindId, Vec<u8>, HandleId)> = state
            .descriptor
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Source {
                    id,
                    mailbox,
                    kind_id,
                    payload,
                } => state
                    .handles
                    .get(id)
                    .map(|h| (*id, *mailbox, *kind_id, payload.clone(), *h)),
                _ => None,
            })
            .collect();
        for (node_id, mailbox, kind_id, payload, handle_id) in sources {
            // Dispatch as the source's own causal root — NOT inheriting
            // the submit chain (ADR-0047 §1: sources dispatch async
            // *after* the submit ack, so the submit reply settles
            // independently of the DAG's execution). The reply still
            // routes back to this cap via the ReplyTarget::Component tag
            // the binding stamps; the minted MailId's correlation keys
            // the table.
            let mail_id = ctx.send_envelope_as_root(mailbox, kind_id, &payload);
            self.pending.insert(
                mail_id.correlation_id,
                Pending {
                    dag_id,
                    node_id,
                    handle_id,
                    role: NodeRole::Source,
                },
            );
        }
    }

    /// Dispatch every `Observer` node of `dag_id` eagerly with
    /// `Ref::Handle` slots. The substrate parking table gates them: the
    /// mail parks until every input handle resolves, then re-routes and
    /// dispatches to the observer's recipient with the resolved values
    /// spliced inline.
    fn dispatch_observers(&mut self, ctx: &mut NativeCtx<'_>, dag_id: DagId) {
        let registry = Arc::clone(self.mailer.registry());
        let Some(state) = self.dags.get(&dag_id) else {
            return;
        };
        // Collect `(node_id, recipient, kind_id, payload, has_inputs)`.
        // `has_inputs` distinguishes a gated observer (parks until its
        // sources resolve) from a degenerate zero-input observer that
        // dispatches at once and is marked resolved here.
        let observers: Vec<(NodeId, MailboxId, KindId, Vec<u8>, bool)> = state
            .descriptor
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Observer {
                    id,
                    recipient,
                    kind_id,
                } => {
                    let has_inputs = state.descriptor.edges.iter().any(|e| e.to == *id);
                    assemble_request(state, &registry, *id, *kind_id)
                        .map(|p| (*id, *recipient, *kind_id, p, has_inputs))
                }
                _ => None,
            })
            .collect();
        for (node_id, recipient, kind_id, payload, has_inputs) in observers {
            // Dispatch as the observer's own causal root — NOT inheriting
            // the submit chain. A gated observer parks on its first
            // unresolved input handle; parked mail would otherwise hold
            // the submit chain's `in_flight` non-zero forever and the
            // submit ack would never settle (ADR-0047 §1 async execution).
            let _ = ctx.send_envelope_as_root(recipient, kind_id, &payload);
            if !has_inputs {
                // A zero-input observer dispatches immediately and is
                // terminal-resolved right away — no source ever resolves
                // it via `resolve_node`.
                if let Some(state) = self.dags.get_mut(&dag_id) {
                    state.mark_resolved(node_id);
                }
            }
        }
    }

    /// Dispatch every `Call` node of `dag_id` whose inputs are already
    /// resolved (`pending_inputs == 0`). At submit only zero-input calls
    /// fire here; downstream calls fire from [`Self::on_reply`] as their
    /// inputs land.
    fn dispatch_ready_calls(&mut self, ctx: &mut NativeCtx<'_>, dag_id: DagId) {
        let ready: Vec<NodeId> = {
            let Some(state) = self.dags.get(&dag_id) else {
                return;
            };
            state
                .descriptor
                .nodes
                .iter()
                .filter_map(|n| match n {
                    Node::Call { id, .. } => (state.pending_inputs.get(id).copied().unwrap_or(0)
                        == 0
                        && !state.resolved.contains(id))
                    .then_some(*id),
                    _ => None,
                })
                .collect()
        };
        for node_id in ready {
            self.dispatch_call(ctx, dag_id, node_id);
        }
    }

    /// Dispatch every `Transform` node of `dag_id` whose inputs are
    /// already resolved (`pending_inputs == 0`). At submit only
    /// zero-input transforms fire here; downstream transforms fire from
    /// [`Self::resolve_node`] as their inputs land (ADR-0048 §3).
    fn dispatch_ready_transforms(&mut self, ctx: &mut NativeCtx<'_>, dag_id: DagId) {
        let ready: Vec<NodeId> = {
            let Some(state) = self.dags.get(&dag_id) else {
                return;
            };
            state
                .descriptor
                .nodes
                .iter()
                .filter_map(|n| match n {
                    Node::Transform { id, .. } => {
                        (state.pending_inputs.get(id).copied().unwrap_or(0) == 0
                            && !state.resolved.contains(id))
                        .then_some(*id)
                    }
                    _ => None,
                })
                .collect()
        };
        for node_id in ready {
            self.dispatch_transform(ctx, dag_id, node_id);
        }
    }

    /// Dispatch one `Transform` node (ADR-0048 §3). Resolves the node's
    /// `transform_id` against the registry, computes the
    /// content-addressed handle id from the input handles in slot order,
    /// and:
    ///
    /// - **cache hit** (`HandleStore::contains`): the cached output
    ///   resolves the node directly — the invoke call is skipped
    ///   entirely (auto-dedup, the headline value, ADR-0048 §4);
    /// - **cache miss**: resolves each input handle to its canonical
    ///   bytes, submits the type-erased thunk to the off-thread compute
    ///   pool, and registers the in-flight deadline. The pool's
    ///   `DagTransformDone` wake lands the result via
    ///   [`Self::on_transform_complete`].
    fn dispatch_transform(&mut self, ctx: &mut NativeCtx<'_>, dag_id: DagId, node_id: NodeId) {
        // Resolve the node's transform_id + output kind + per-node
        // timeout + assigned handle.
        let Some((transform_id, output_kind_id, node_timeout)) =
            self.transform_node_meta(dag_id, node_id)
        else {
            return;
        };
        let Some(state) = self.dags.get(&dag_id) else {
            return;
        };
        if state.status.is_terminal() || state.resolved.contains(&node_id) {
            return;
        }
        let Some(handle_id) = state.handles.get(&node_id).copied() else {
            if let Some(state) = self.dags.get_mut(&dag_id) {
                state.mark_failed(
                    node_id,
                    "transform node missing handle assignment".to_owned(),
                );
            }
            return;
        };

        // The input handle ids in slot-index order (ADR-0048 §4). The
        // executor walks `Edge.slot` ascending for the consumer.
        let mut slotted: Vec<(u32, HandleId)> = state
            .descriptor
            .edges
            .iter()
            .filter(|e| e.to == node_id)
            .filter_map(|e| state.handles.get(&e.from).map(|h| (e.slot, *h)))
            .collect();
        slotted.sort_by_key(|(slot, _)| *slot);
        let inputs_in_order: Vec<HandleId> = slotted.iter().map(|(_, h)| *h).collect();

        let Some(entry) = self.transform_registry.lookup(transform_id) else {
            // Validation already rejected unknown ids; a miss here is a
            // substrate invariant violation. Fail the node rather than
            // silently stall.
            if let Some(state) = self.dags.get_mut(&dag_id) {
                state.mark_failed(node_id, format!("transform {transform_id} not registered"));
            }
            return;
        };

        // Content-addressed cache check (ADR-0048 §4, iamacoffeepot/aether#982).
        let content_id = content_addressed_handle_id(transform_id, &inputs_in_order);
        if self.store().contains(content_id) {
            // Cache hit: the cached output resolves the node directly —
            // skip the invoke entirely. Read the bytes + kind back and
            // resolve through the node's handle.
            self.store().inc_ref(content_id);
            if let Some((kind, bytes)) = self.store().get(content_id) {
                self.resolve_node(ctx, dag_id, node_id, handle_id, kind, &bytes);
            } else if let Some(state) = self.dags.get_mut(&dag_id) {
                state.mark_failed(node_id, "transform cache hit but value vanished".to_owned());
            }
            return;
        }

        // Cache miss: resolve each input handle to its canonical bytes
        // (slot order). A missing input handle is a substrate invariant
        // violation — fail the node.
        let mut input_bytes: Vec<Vec<u8>> = Vec::with_capacity(inputs_in_order.len());
        for handle in &inputs_in_order {
            let Some((_, bytes)) = self.store().get(*handle) else {
                if let Some(state) = self.dags.get_mut(&dag_id) {
                    state.mark_failed(
                        node_id,
                        "transform input handle unresolved at dispatch".to_owned(),
                    );
                }
                return;
            };
            input_bytes.push(bytes);
        }

        // Submit to the off-thread compute pool (ADR-0048 §3). The wake
        // mail lands `DagTransformDone { job_id }` at this cap.
        let job_id = self.transform_pool.submit(
            entry.invoke,
            input_bytes,
            self.self_mailbox,
            KindId(<DagTransformDone as aether_data::Kind>::ID.0),
        );
        let timeout = node_timeout.map_or(self.transform_timeout, Duration::from_millis);
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self.in_flight_transforms.insert(
            job_id,
            InFlightTransform {
                dag_id,
                node_id,
                handle_id,
                content_id,
                output_kind_id,
                deadline: Instant::now() + timeout,
                timeout_ms,
            },
        );
    }

    /// Resolve a `Transform` node's `(transform_id, output_kind_id,
    /// timeout_ms)`. `None` if the node isn't a `Transform` (or the DAG
    /// is gone).
    fn transform_node_meta(
        &self,
        dag_id: DagId,
        node_id: NodeId,
    ) -> Option<(TransformId, KindId, Option<u64>)> {
        self.dags
            .get(&dag_id)?
            .descriptor
            .nodes
            .iter()
            .find_map(|n| match n {
                Node::Transform {
                    id,
                    transform_id,
                    output_kind_id,
                    timeout_ms,
                } if *id == node_id => Some((*transform_id, *output_kind_id, *timeout_ms)),
                _ => None,
            })
    }

    /// An off-thread transform invocation finished (ADR-0048 §3). Pull
    /// the stashed outcome from the pool and resolve / fail the node:
    ///
    /// - `Ok(bytes)`: enforce the output-byte cap (ADR-0048 §6), store
    ///   the bytes under the content-addressed id, and resolve the
    ///   node's handle (flushing parked downstream mail). A domain `Err`
    ///   value is inside these bytes — it is a successful, cached output,
    ///   not a DAG failure.
    /// - decode / arity / panic: fail the node with the diagnostic.
    ///
    /// A wake for a node the executor already failed (timeout fired
    /// first, or the DAG was cancelled) is discarded — the stashed
    /// outcome is forgotten so the completion map doesn't leak.
    pub fn on_transform_complete(&mut self, ctx: &mut NativeCtx<'_>, job_id: u64) {
        let Some(in_flight) = self.in_flight_transforms.remove(&job_id) else {
            // Already timed out / cancelled — forget any stashed outcome.
            self.transform_pool.forget(job_id);
            return;
        };
        let Some(outcome) = self.transform_pool.take_outcome(job_id) else {
            // Wake arrived before the outcome was stashed (should not
            // happen — the worker stashes before posting the wake). Put
            // the in-flight entry back so the reaper / a re-wake can
            // still resolve it.
            self.in_flight_transforms.insert(job_id, in_flight);
            return;
        };

        let InFlightTransform {
            dag_id,
            node_id,
            handle_id,
            content_id,
            output_kind_id,
            ..
        } = in_flight;

        // Drop the result for a cancelled / completed DAG.
        if self
            .dags
            .get(&dag_id)
            .is_none_or(|s| s.status.is_terminal())
        {
            return;
        }

        match outcome {
            TransformOutcome::Ok { bytes } => {
                if bytes.len() > self.transform_max_output_bytes {
                    if let Some(state) = self.dags.get_mut(&dag_id) {
                        state.mark_failed(
                            node_id,
                            format!(
                                "transform output exceeded {} bytes",
                                self.transform_max_output_bytes
                            ),
                        );
                    }
                    return;
                }
                // Store the output under the content-addressed id so a
                // future identical compute hits the cache (ADR-0048 §4),
                // hold a ref for the DAG, then resolve the node's handle.
                if let Err(e) = self.store().put(content_id, output_kind_id, bytes.clone()) {
                    tracing::warn!(
                        target: TARGET,
                        error = ?e,
                        ?node_id,
                        "failed to store content-addressed transform output",
                    );
                } else {
                    self.store().inc_ref(content_id);
                }
                self.resolve_node(ctx, dag_id, node_id, handle_id, output_kind_id, &bytes);
            }
            TransformOutcome::Err { error } => {
                if let Some(state) = self.dags.get_mut(&dag_id) {
                    state.mark_failed(node_id, format!("transform failed: {error}"));
                }
            }
            TransformOutcome::Panicked { message } => {
                if let Some(state) = self.dags.get_mut(&dag_id) {
                    state.mark_failed(node_id, format!("transform panicked: {message}"));
                }
            }
        }
    }

    /// Dispatch one `Call` node as its own causal root (ADR-0047 §4 step
    /// 2). Assembles the request from resolved input handles, sends via
    /// `send_envelope_as_root` (mints `call_root`), subscribes
    /// `Settled { call_root }`, and opens an accumulation buffer keyed
    /// on the call's correlation. A no-input call dispatches an empty
    /// request.
    fn dispatch_call(&mut self, ctx: &mut NativeCtx<'_>, dag_id: DagId, node_id: NodeId) {
        let registry = Arc::clone(self.mailer.registry());
        let Some(state) = self.dags.get_mut(&dag_id) else {
            return;
        };
        if state.status.is_terminal() {
            return;
        }
        // Already dispatched? (a re-entrant input landing). The buffer
        // keyed on this node guards against double-dispatch via the
        // resolved set below.
        let Some((recipient, kind_id)) = state.descriptor.nodes.iter().find_map(|n| match n {
            Node::Call {
                id,
                recipient,
                kind_id,
            } if *id == node_id => Some((*recipient, *kind_id)),
            _ => None,
        }) else {
            return;
        };
        let Some(payload) = assemble_request(state, &registry, node_id, kind_id) else {
            // Missing handle assignment is a substrate invariant
            // violation — fail the node rather than dispatch a malformed
            // request.
            state.mark_failed(node_id, "call request assembly failed".to_owned());
            return;
        };

        // Dispatch as a fresh causal root so settlement scopes to this
        // call, not the whole DAG (ADR-0047 §2 per-Call root).
        let call_root: MailId = ctx.send_envelope_as_root(recipient, kind_id, &payload);

        // Open the accumulation buffer + register the in-flight call.
        let dag_id_copy = dag_id;
        self.pending.insert(
            call_root.correlation_id,
            Pending {
                dag_id: dag_id_copy,
                node_id,
                handle_id: *state.handles.get(&node_id).unwrap_or(&HandleId(0)),
                role: NodeRole::Call,
            },
        );
        if let Some(state) = self.dags.get_mut(&dag_id) {
            state.call_buffers.insert(
                call_root.correlation_id,
                CallBuffer {
                    node_id,
                    elements: Vec::new(),
                },
            );
        }
        self.in_flight_calls.insert(
            call_root.correlation_id,
            InFlightCall {
                dag_id,
                node_id,
                deadline: Instant::now() + self.call_timeout,
            },
        );

        // Subscribe to settlement of the call's chain. The chassis
        // settlement registry pushes a `Settled { call_root }` mail back
        // at this cap when the chain quiesces (ADR-0047 §4 step 4). On a
        // chassis with no registry the bundle can't close on settlement;
        // the per-Call timeout still bounds it.
        if let Some(reg) = self.mailer.settlement_registry() {
            reg.subscribe_settlement_mail(
                call_root,
                self.self_mailbox,
                <Settled as aether_data::Kind>::ID,
                Arc::clone(&self.mailer),
            );
        } else {
            tracing::warn!(
                target: TARGET,
                "no settlement registry on this chassis; Call bundles close only on timeout",
            );
        }
    }

    /// A source / `Call` reply landed on the cap's mailbox. `correlation`
    /// is the reply envelope's correlation id; `kind` / `payload` are
    /// the reply's. Returns `true` when the correlation matched a
    /// pending dispatch (so the cap suppresses the fallback warn).
    ///
    /// A source reply resolves the source's handle (publishing the
    /// reply bytes into the store + flushing parked observers) and
    /// decrements downstream `Call` input counters. A `Call` reply
    /// appends to the call's accumulation buffer; the buffer closes
    /// later on `Settled`.
    pub fn on_reply(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        correlation: u64,
        kind: KindId,
        payload: &[u8],
    ) -> bool {
        let Some(pending) = self.pending.get(&correlation).copied() else {
            return false;
        };

        match pending.role {
            NodeRole::Source => {
                // Single-reply node: consume the correlation.
                self.pending.remove(&correlation);
                self.resolve_node(
                    ctx,
                    pending.dag_id,
                    pending.node_id,
                    pending.handle_id,
                    kind,
                    payload,
                );
                true
            }
            NodeRole::Call => {
                // Multi-reply: keep the correlation until settlement.
                if let Some(state) = self.dags.get_mut(&pending.dag_id)
                    && let Some(buf) = state.call_buffers.get_mut(&correlation)
                {
                    buf.elements.push((kind, payload.to_vec()));
                }
                true
            }
        }
    }

    /// Resolve one source node's output handle: publish the reply bytes
    /// under the node's handle (so the parking-table walk splices them
    /// into downstream observer / call requests), flush parked mail,
    /// mark the node resolved, and decrement / dispatch downstream
    /// `Call`s whose last input just landed.
    fn resolve_node(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        dag_id: DagId,
        node_id: NodeId,
        handle_id: HandleId,
        kind: KindId,
        payload: &[u8],
    ) {
        // Drop late replies for a cancelled / completed DAG.
        let Some(state) = self.dags.get(&dag_id) else {
            return;
        };
        if state.status.is_terminal() {
            return;
        }

        // Publish + flush parked observer mail. `resolve_handle` puts the
        // bytes then re-routes everything parked on this handle.
        if let Err(e) = self
            .mailer
            .resolve_handle(handle_id, kind, payload.to_vec())
        {
            tracing::warn!(
                target: TARGET,
                error = ?e,
                ?node_id,
                "failed to resolve source handle; downstream consumers stay parked",
            );
        }

        if let Some(state) = self.dags.get_mut(&dag_id) {
            state.mark_resolved(node_id);
        }

        // Decrement downstream `Call` inputs; mark downstream observers
        // resolved once every input handle they consume has landed (the
        // parked observer mail un-parks and dispatches at that point);
        // collect `Call`s now ready to dispatch.
        let newly_ready: Vec<NodeId> = {
            let Some(state) = self.dags.get_mut(&dag_id) else {
                return;
            };
            let consumers: Vec<NodeId> = state
                .descriptor
                .edges
                .iter()
                .filter(|e| e.from == node_id)
                .map(|e| e.to)
                .collect();
            let mut ready = Vec::new();
            let mut observers_done = Vec::new();
            for consumer in consumers {
                // `Call` consumers gate on the explicit counter.
                if let Some(remaining) = state.pending_inputs.get_mut(&consumer) {
                    *remaining = remaining.saturating_sub(1);
                    if *remaining == 0 && !state.resolved.contains(&consumer) {
                        ready.push(consumer);
                    }
                    continue;
                }
                // Observer consumers gate through the parking table; mark
                // resolved once every source feeding the observer's slots
                // is resolved (so its mail has un-parked).
                let is_observer = state
                    .descriptor
                    .nodes
                    .iter()
                    .any(|n| n.id() == consumer && matches!(n, Node::Observer { .. }));
                if is_observer && !state.resolved.contains(&consumer) {
                    let all_inputs_resolved = state
                        .descriptor
                        .edges
                        .iter()
                        .filter(|e| e.to == consumer)
                        .all(|e| state.resolved.contains(&e.from));
                    if all_inputs_resolved {
                        observers_done.push(consumer);
                    }
                }
            }
            for observer in observers_done {
                state.mark_resolved(observer);
            }
            ready
        };
        // A counter-gated consumer is either a `Call` (dispatch as a
        // causal root awaiting settlement) or a `Transform` (run the
        // pure fn off-thread). Dispatch each by its node type.
        for consumer in newly_ready {
            if self.node_is_transform(dag_id, consumer) {
                self.dispatch_transform(ctx, dag_id, consumer);
            } else {
                self.dispatch_call(ctx, dag_id, consumer);
            }
        }
    }

    /// `true` if `node_id` is a `Transform` node in `dag_id`.
    fn node_is_transform(&self, dag_id: DagId, node_id: NodeId) -> bool {
        self.dags.get(&dag_id).is_some_and(|state| {
            state
                .descriptor
                .nodes
                .iter()
                .any(|n| n.id() == node_id && matches!(n, Node::Transform { .. }))
        })
    }

    /// A `Settled { call_root }` notification landed (ADR-0047 §4 step
    /// 4). Close the call's bundle: drain the accumulation buffer into
    /// the call's `Bundle` handle, mark the call resolved, flush
    /// downstream consumers, and dispatch any downstream `Call`s whose
    /// last input just landed. Returns `true` if the root matched an
    /// in-flight call.
    pub fn on_settled(&mut self, ctx: &mut NativeCtx<'_>, call_root: MailId) -> bool {
        let correlation = call_root.correlation_id;
        let Some(pending) = self.pending.remove(&correlation) else {
            return false;
        };
        self.in_flight_calls.remove(&correlation);

        let (node_id, handle_id, elements) = {
            let Some(state) = self.dags.get_mut(&pending.dag_id) else {
                return true;
            };
            if state.status.is_terminal() {
                state.call_buffers.remove(&correlation);
                return true;
            }
            let Some(buf) = state.call_buffers.remove(&correlation) else {
                return true;
            };
            (pending.node_id, pending.handle_id, buf.elements)
        };
        let _ = node_id;

        // Build the ordered, self-describing Bundle from the accumulated
        // replies and resolve the call's handle to it.
        let bundle = Bundle {
            elements: elements
                .into_iter()
                .map(|(kind_id, payload)| BundleElement { kind_id, payload })
                .collect(),
        };
        let bundle_bytes = <Bundle as aether_data::Kind>::encode_into_bytes(&bundle);
        self.resolve_node(
            ctx,
            pending.dag_id,
            pending.node_id,
            handle_id,
            <Bundle as aether_data::Kind>::ID,
            &bundle_bytes,
        );
        true
    }

    /// Cancel a DAG (ADR-0047 §5). Marks it cancelled, drops every
    /// parked mail on the DAG's handles, releases the executor's refs,
    /// and drops outstanding reply correlations + settlement
    /// subscriptions for the DAG so late replies / `Settled` are no-ops.
    /// Replies `Ok { cancelled: true }` for a live DAG, `Ok { cancelled:
    /// false }` for one that already completed, `Err` for an unknown id.
    pub fn cancel(&mut self, dag_id: DagId) -> CancelResult {
        let Some(state) = self.dags.get_mut(&dag_id) else {
            return CancelResult::Err {
                error: format!("unknown dag {dag_id}"),
            };
        };
        if state.status.is_terminal() {
            return CancelResult::Ok { cancelled: false };
        }
        state.mark_cancelled();

        // Drop parked mail + release refs on every handle the DAG owns.
        let handle_ids: Vec<HandleId> = state.handles.values().copied().collect();
        for id in &handle_ids {
            let _ = self.store().take_parked(*id);
            self.store().dec_ref(*id);
        }
        // Drop the DAG's reply correlations + in-flight call / transform
        // entries so a late source reply / `Settled` / `DagTransformDone`
        // finds no entry and is a no-op. An orphaned transform thread
        // still runs to completion but its result is dropped.
        self.pending.retain(|_, p| p.dag_id != dag_id);
        self.in_flight_calls.retain(|_, c| c.dag_id != dag_id);
        self.in_flight_transforms.retain(|_, t| t.dag_id != dag_id);
        if let Some(state) = self.dags.get_mut(&dag_id) {
            state.call_buffers.clear();
        }

        CancelResult::Ok { cancelled: true }
    }

    /// Query a DAG's status (ADR-0047 §1/§6). Returns the wire
    /// `StatusResult` for a known DAG, or `None` for an unknown id (the
    /// cap maps `None` to its `UnknownDag` reply shape).
    #[must_use]
    pub fn status(&self, dag_id: DagId) -> Option<StatusResult> {
        self.dags.get(&dag_id).map(DagState::status_result)
    }

    /// The reaping tick (ADR-0047 §7, ADR-0048 §6). Sweeps terminal DAGs
    /// whose `completed_at` is past retention (separate windows for
    /// completed vs failed / cancelled), times out in-flight `Call`s
    /// whose settlement deadline has passed (failing the node — a never-
    /// settling cap is a node failure, not a partial bundle), and times
    /// out in-flight `Transform`s past their deadline (failing the node;
    /// the runaway thread orphans, ADR-0048 §6). Returns the number of
    /// DAGs reaped.
    pub fn reap(&mut self) -> usize {
        let now = Instant::now();

        // Time out in-flight calls past their deadline.
        let timed_out: Vec<(u64, DagId, NodeId)> = self
            .in_flight_calls
            .iter()
            .filter(|(_, c)| now >= c.deadline)
            .map(|(corr, c)| (*corr, c.dag_id, c.node_id))
            .collect();
        for (correlation, dag_id, node_id) in timed_out {
            self.in_flight_calls.remove(&correlation);
            self.pending.remove(&correlation);
            if let Some(state) = self.dags.get_mut(&dag_id) {
                state.call_buffers.remove(&correlation);
                state.mark_failed(node_id, "call timed out waiting for settlement".to_owned());
            }
        }

        // Time out in-flight transforms past their deadline (ADR-0048
        // §6). The node fails; the runaway thread is orphaned (a native
        // thread can't be safely preempted) and its eventual outcome is
        // discarded when its wake lands with no in-flight entry.
        let timed_out_transforms: Vec<(u64, DagId, NodeId, u64)> = self
            .in_flight_transforms
            .iter()
            .filter(|(_, t)| now >= t.deadline)
            .map(|(job, t)| (*job, t.dag_id, t.node_id, t.timeout_ms))
            .collect();
        for (job_id, dag_id, node_id, timeout_ms) in timed_out_transforms {
            self.in_flight_transforms.remove(&job_id);
            self.transform_pool.forget(job_id);
            if let Some(state) = self.dags.get_mut(&dag_id) {
                state.mark_failed(node_id, format!("timeout: {timeout_ms}ms"));
            }
        }

        // Sweep terminal DAGs past retention.
        let reapable: Vec<DagId> = self
            .dags
            .iter()
            .filter(|(_, state)| {
                let Some(at) = state.completed_at else {
                    return false;
                };
                let window = match &state.status {
                    DagStatus::Complete => self.retention_complete,
                    DagStatus::Failed { .. } | DagStatus::Cancelled => self.retention_failed,
                    DagStatus::Pending | DagStatus::Running => return false,
                };
                now.duration_since(at) >= window
            })
            .map(|(id, _)| *id)
            .collect();
        let reaped = reapable.len();
        for dag_id in reapable {
            if let Some(state) = self.dags.remove(&dag_id) {
                // Release the executor's refs on the DAG's handles. The
                // entries evict per the global LRU once refcount hits
                // zero (Phase 1 semantics).
                for id in state.handles.values() {
                    self.store().dec_ref(*id);
                }
                self.pending.retain(|_, p| p.dag_id != dag_id);
                self.in_flight_calls.retain(|_, c| c.dag_id != dag_id);
            }
        }
        reaped
    }

    /// Count of live + recently-terminal DAGs. Test introspection.
    #[must_use]
    pub fn dag_count(&self) -> usize {
        self.dags.len()
    }
}

/// Wire-shaped submit outcome — the cap replies it as
/// [`aether_kinds::SubmitResult`] verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    Ok {
        dag_id: DagId,
        output_handles: Vec<aether_kinds::NodeHandle>,
    },
    Err {
        error: aether_kinds::DagError,
    },
}

/// Assemble an observer / `Call` request payload for `consumer` from the
/// DAG's handle assignment: every `Ref<K>` slot of the consumer's
/// request schema (in declaration order) is filled with a
/// `Ref::Handle { id, kind_id }` pointing at the upstream node feeding
/// that slot. Returns `None` if the consumer's kind isn't registered or
/// an edge references a node with no handle assignment.
///
/// The slot → upstream-node mapping comes from the descriptor edges: an
/// edge `{ from, to: consumer, slot }` says slot `slot` is fed by
/// `from`'s output handle. The walk-and-resolve path
/// ([`crate::handle_store::walk_and_resolve`]) substitutes each handle's
/// stored bytes inline when the mail dispatches (or parks until they
/// resolve).
///
/// `registry` is the routing registry the consumer's request schema is
/// resolved against (the executor passes `self.mailer.registry()`).
fn assemble_request(
    state: &DagState,
    registry: &Registry,
    consumer: NodeId,
    kind_id: KindId,
) -> Option<Vec<u8>> {
    // Resolve the consumer's request schema: the registered descriptor
    // for `kind_id`, which must be a struct of `Ref<K>` fields.
    let descriptor = registry.kind_descriptor(kind_id)?;
    let SchemaType::Struct { fields, .. } = &descriptor.schema else {
        return None;
    };
    // The declared inner kind id of each `Ref<K>` slot, in declaration
    // order — emitted onto the wire `Ref::Handle.kind_id`.
    let ref_slot_kinds: Vec<KindId> = fields
        .iter()
        .filter_map(|f| match &f.ty {
            SchemaType::Ref(cell) => Some(slot_inner_kind_id(registry, cell)),
            _ => None,
        })
        .collect();

    // Map slot index -> the upstream node feeding it.
    let mut slot_source: HashMap<u32, NodeId> = HashMap::new();
    for edge in &state.descriptor.edges {
        if edge.to == consumer {
            slot_source.insert(edge.slot, edge.from);
        }
    }

    let mut out: Vec<u8> = Vec::new();
    for (slot_index, expected_kind) in ref_slot_kinds.iter().enumerate() {
        let slot = u32::try_from(slot_index).unwrap_or(u32::MAX);
        let from = slot_source.get(&slot)?;
        let handle = *state.handles.get(from)?;
        // The Handle variant carries no `K`, so any marker type works —
        // emit the wire `Ref::Handle { id, kind_id }`. The walk-and-
        // resolve path validates against the *field's* expected type at
        // dispatch, splicing the stored bytes inline (or parking).
        let r: Ref<u8> = Ref::Handle {
            id: handle.0,
            kind_id: expected_kind.0,
        };
        let mut field_bytes = postcard::to_allocvec(&r).ok()?;
        out.append(&mut field_bytes);
    }
    Some(out)
}

/// The declared inner kind id of a `Ref<K>` slot. The schema cell
/// carries `K`'s schema; look up its registered kind id so the emitted
/// `Ref::Handle.kind_id` matches what the consumer declared. Falls back
/// to `KindId(0)` when no registered kind matches — the walk-and-resolve
/// path validates against the *field's* expected type, not this id, so a
/// fallback id still resolves for the common case where the producer's
/// stored kind equals the slot type.
fn slot_inner_kind_id(registry: &Registry, cell: &aether_data::SchemaCell) -> KindId {
    let inner: &SchemaType = cell;
    let target = canonical_kind_bytes("", inner);
    registry
        .list_kind_descriptors()
        .into_iter()
        .find(|d| canonical_kind_bytes("", &d.schema) == target)
        .map_or(KindId(0), |d| {
            registry.kind_id(&d.name).unwrap_or(KindId(0))
        })
}

#[cfg(test)]
mod config_tests {
    use super::{
        DEFAULT_CALL_TIMEOUT_MS, DEFAULT_RETENTION_COMPLETE_MS, DEFAULT_RETENTION_FAILED_MS,
        DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES, DEFAULT_TRANSFORM_TIMEOUT_MS, ExecutorConfig,
        ExecutorConfigLayer, default_pool_threads, parse_call_timeout_ms,
        parse_retention_complete_ms, parse_retention_failed_ms, parse_transform_max_output_bytes,
        parse_transform_timeout_ms,
    };

    // ADR-0090: the confique migration is byte-identical to the prior
    // inline env reads. These exercise resolution without touching process
    // env (issue 464) — the parsers are pure, and the defaults check loads
    // the layer with no `.env()` source.

    #[test]
    fn parse_numbers_soft_fall_back_to_defaults() {
        assert_eq!(parse_call_timeout_ms("100").unwrap(), 100);
        assert_eq!(
            parse_call_timeout_ms("nope").unwrap(),
            DEFAULT_CALL_TIMEOUT_MS
        );
        assert_eq!(parse_transform_timeout_ms("200").unwrap(), 200);
        assert_eq!(
            parse_transform_timeout_ms("nope").unwrap(),
            DEFAULT_TRANSFORM_TIMEOUT_MS
        );
        assert_eq!(parse_transform_max_output_bytes("4096").unwrap(), 4096);
        assert_eq!(
            parse_transform_max_output_bytes("nope").unwrap(),
            DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES
        );
        assert_eq!(parse_retention_complete_ms("300").unwrap(), 300);
        assert_eq!(
            parse_retention_complete_ms("nope").unwrap(),
            DEFAULT_RETENTION_COMPLETE_MS
        );
        assert_eq!(parse_retention_failed_ms("400").unwrap(), 400);
        assert_eq!(
            parse_retention_failed_ms("nope").unwrap(),
            DEFAULT_RETENTION_FAILED_MS
        );
    }

    #[test]
    fn executor_config_layer_defaults_match() {
        use confique::Config as _;
        // No `.env()` source: literal defaults only, env-free. Guards the
        // layer defaults against the named consts + `ExecutorConfig::default`.
        let layer = ExecutorConfigLayer::builder()
            .load()
            .expect("defaults load");
        let default = ExecutorConfig::default();
        assert_eq!(layer.call_timeout_ms, DEFAULT_CALL_TIMEOUT_MS);
        assert_eq!(layer.call_timeout_ms, default.call_timeout_ms);
        assert_eq!(layer.transform_timeout_ms, DEFAULT_TRANSFORM_TIMEOUT_MS);
        assert_eq!(
            layer.transform_max_output_bytes,
            DEFAULT_TRANSFORM_MAX_OUTPUT_BYTES
        );
        assert_eq!(layer.retention_complete_ms, DEFAULT_RETENTION_COMPLETE_MS);
        assert_eq!(layer.retention_failed_ms, DEFAULT_RETENTION_FAILED_MS);
        // Pool threads has no literal default — the layer field is `None`
        // when unset, resolved to available parallelism in `from_env`.
        assert_eq!(layer.pool_threads, None);
        assert!(default.pool_threads >= 1);
        assert_eq!(default.pool_threads, default_pool_threads());
    }
}
