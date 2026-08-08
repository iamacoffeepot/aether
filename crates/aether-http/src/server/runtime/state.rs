// The whole runtime module shares one import surface (ADR-0122); each
// concern submodule re-inherits it from the module root through this glob
// rather than restating a bespoke list per file.
#[allow(clippy::wildcard_imports)]
use super::*;

use crate::server::shard::HttpDispatchShard;
use aether_actor::Single;
use aether_substrate::Subname;
use std::collections::HashSet;
use std::collections::hash_map::Entry;

/// One socket retained by the supervisor while its dispatch shards are
/// `Starting`. It is not charged to `live_connections` until a Live shard
/// accepts it, but it does count toward the configured connection ceiling.
pub struct PendingPeer {
    pub stream: TcpStream,
    pub peer: SocketAddr,
}

/// Move-only context attached to one staged shard birth. The child's identity
/// rides its `SpawnOutcome`; what stays supervisor-owned until authoritative
/// activation is the half of the [`WakeSink`] no spawn result can supply — the
/// shard's index in the round-robin set, its inbound sender, and its wake flag.
pub struct ShardSpawnContext {
    pub index: usize,
    pub subname: String,
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    pub wake_dirty: Arc<AtomicBool>,
}

/// Per-index startup result. Keeping failures distinct from pending attempts
/// makes a stale or duplicate task completion unable to decrement `remaining`
/// twice.
pub enum ShardSlot {
    Pending,
    Ready(WakeSink),
    Failed,
}

/// Lazy dispatch-shard lifecycle. `Starting` retains accepted sockets and
/// indexed child results until every birth settles; `Ready` alone exposes
/// sinks to round-robin assignment.
pub enum ShardStartup {
    Idle,
    Starting {
        remaining: usize,
        next_to_stage: Option<usize>,
        slots_by_index: Vec<ShardSlot>,
        pending_peers: VecDeque<PendingPeer>,
    },
    Ready {
        shards: Vec<WakeSink>,
        next_shard: usize,
    },
    Failed,
}

/// State transition produced by one shard attempt. The final transition owns
/// the retained FIFO so it can be drained exactly once after the startup enum
/// has already become `Ready` or `Failed`.
pub enum ShardSettlement {
    Pending,
    Ready { pending_peers: VecDeque<PendingPeer>, shard_count: usize },
    Failed { pending_peers: VecDeque<PendingPeer> },
    Stale,
}

/// `aether.http.server` supervisor state (ADR-0135). Owns the TCP listener +
/// accept thread, the shared route table, the global live-connection ceiling,
/// and the dispatch-shard sinks. Per-request work never runs here — the
/// supervisor's steady-state job is assigning each accepted connection to a
/// shard and serving the route-registration surface (ADR-0130).
pub struct HttpSupervisorState {
    /// The resolved boot config, kept whole: the tuning fields seed each
    /// shard at spawn, and `max_connections` backs the assignment-time
    /// ceiling check.
    pub config: HttpServerConfig,
    /// Registered routes (ADR-0130), shared with every shard (and, in later
    /// stages, the readers): the supervisor's registration handlers write
    /// under the lock, dispatch-time resolution reads. Unordered —
    /// resolution picks the winner per request by `(prefix length, method
    /// specificity)`, which is deterministic without a sort: two distinct
    /// equal-length prefixes cannot both match one path, and duplicate
    /// `(prefix, method)` keys are rejected at registration. Route counts
    /// are tens per substrate, so the linear scan is dwarfed by the header
    /// parse that precedes it (ADR-0130).
    pub routes: SharedRoutes,
    /// Global live-connection count backing the `max_connections` ceiling
    /// (ADR-0108 §6): incremented here at assignment, decremented by the
    /// owning shard on connection close. An atomic rather than a table —
    /// the connections themselves live sharded.
    pub live_connections: Arc<AtomicUsize>,
    /// Cached `Arc<Mailer>` for registry validation in the registration
    /// handlers and for building each shard's wake sink.
    pub mailer: Arc<Mailer>,
    pub listener_port: u16,
    pub accept_shutdown: Arc<AtomicBool>,
    pub accept_thread: Option<JoinHandle<()>>,
    /// The supervisor's own sidecar channel: the accept thread posts
    /// [`InboundEvent::PeerAccepted`] here; nothing else feeds it.
    pub inbound_rx: mpsc::Receiver<InboundEvent>,
    /// The supervisor drain loop's wake-coalescing flag (ADR-0135 §4),
    /// shared with the accept sink; cleared at the top of
    /// `on_inbound_ready`.
    pub wake_dirty: Arc<AtomicBool>,
    /// Stable target for the private `HttpInboundReady` turns that stage one
    /// shard per transactional owner batch. This is the supervisor's
    /// already-Live mailbox, never a reserved child route.
    pub self_mailbox: MailboxId,
    /// Lazy dispatch-shard lifecycle. Accepted sockets remain here while
    /// child activation is pending and only enter a shard after its
    /// `SpawnOutcome` completion proves the route Live.
    pub shard_startup: ShardStartup,
    /// Cap-global stream-id source, cloned into every shard's seed
    /// (ADR-0135) — see [`HttpShardState::next_stream_id`].
    pub next_stream_id: Arc<AtomicU64>,
    /// One monitor per route-holding mailbox (ADR-0079 §8 amended),
    /// registered on its first route claim and released when its
    /// `MonitorNotice` purges the mailbox's routes. The handle's
    /// `Drop` deregisters, so the map is both the dedup guard and the
    /// RAII anchor.
    pub monitors: HashMap<MailboxId, MonitorHandle>,
    /// Mailboxes whose monitor attempt failed — remembered so the
    /// `route holder is not monitorable` warn fires once per mailbox,
    /// not once per route.
    pub unmonitorable: HashSet<MailboxId>,
}

/// Dispatch-shard state (ADR-0135): today's whole per-connection machine —
/// connection table, in-flight correlation table, response/request stream
/// tables, websocket state — over the 1/N slice of connections the
/// supervisor assigned here. The dispatcher holds this as the shard actor's
/// state; the addressing identity is the distinct ZST
/// [`HttpDispatchShard`].
pub struct HttpShardState {
    /// The supervisor's shared route table (ADR-0130/0135); this shard only
    /// reads it, at request-dispatch time.
    pub routes: SharedRoutes,
    /// The global live-connection count (ADR-0135): the supervisor
    /// increments at assignment; this shard decrements when it closes (or
    /// fails to adopt) one of its connections.
    pub live_connections: Arc<AtomicUsize>,
    pub max_request_bytes: usize,
    pub max_header_bytes: usize,
    pub request_timeout: Duration,
    /// Idle timeout between requests on a kept-alive connection (and for a
    /// fresh connection that never sends its first byte). Distinct from
    /// `request_timeout`, which stays the in-flight read + response
    /// deadline.
    pub keep_alive_timeout: Duration,
    pub self_mailbox: MailboxId,
    /// Cached `Arc<Mailer>` so the shard can fire wake mails into itself,
    /// validate a matched route's registrant against the registry at
    /// dispatch time, and subscribe to settlement. The shard is
    /// single-threaded post-ADR-0038 so direct storage is fine.
    pub mailer: Arc<Mailer>,
    pub inbound_rx: mpsc::Receiver<InboundEvent>,
    pub inbound_tx: mpsc::Sender<InboundEvent>,
    /// This shard's wake-coalescing flag (ADR-0135 §4), shared by every
    /// sink targeting this shard (the supervisor's assignment sink and
    /// each reader/writer sidecar); cleared at the top of the shard's
    /// `on_inbound_ready`.
    pub wake_dirty: Arc<AtomicBool>,
    pub connections: HashMap<ConnId, ConnState>,
    pub next_conn_id: ConnId,
    /// Dispatch-correlation → open response socket. Populated on
    /// dispatch; cleared on reply, settlement, timeout, or close.
    pub in_flight: HashMap<u64, PendingRequest>,
    /// Credit-window depth (ADR-0128): the count of in-flight response
    /// chunks a stream may hold; also the bounded hand-off channel's
    /// capacity and the initial credit grant.
    pub response_stream_window: u32,
    /// Active response streams (ADR-0128), keyed by a cap-minted `stream_id`
    /// from [`HttpShardState::next_stream_id`]. Promoted from `in_flight` on
    /// `HttpResponseStreamOpen`; torn down on stream end, flood, timeout, or
    /// connection close. Websocket streams share this map and draw from the
    /// same counter, so one key names exactly one stream of either kind.
    pub streams: HashMap<u64, StreamState>,
    /// Inbound request-stream credit-window depth (ADR-0128): the initial
    /// count of `HttpRequestChunk` mails the cap delivers to a streaming
    /// handler before parking the reader on the handler's `HttpRequestCredit`.
    pub request_stream_window: u32,
    /// Active *inbound* request streams (ADR-0128), keyed by a cap-minted
    /// `stream_id`. Created when a streaming handler accepts a request head;
    /// torn down when the body ends (the final response then rides the
    /// `HttpRequestStreamEnd` dispatch through `in_flight`) or on connection
    /// close.
    pub request_streams: HashMap<u64, RequestStreamState>,
    /// Monotonic source of every cap-minted stream id — inbound request
    /// streams, websocket outbound streams, and response streams alike —
    /// shared across every shard (ADR-0135): a handler identifies a stream by
    /// its `stream_id` alone (ADR-0132), so ids must stay unique across the
    /// whole cap, not per shard. Response streams draw from this counter too
    /// (ADR-0128 §2 as amended 2026-07-20; issue 3730): they previously reused
    /// the request's dispatch correlation id, which is minted per sender and
    /// so identifies a stream only by accident — consecutive requests to one
    /// handler collided on it.
    pub next_stream_id: Arc<AtomicU64>,
    /// Read deadline between frames on an upgraded websocket connection
    /// (ADR-0129), from `websocket_idle_timeout_millis` — longer than
    /// `request_timeout`, since an idle websocket is normal.
    pub ws_idle_timeout: Duration,
}

/// Write a canned refusal to a just-accepted socket and shut it down —
/// the pre-adoption reject (no `ConnState` exists yet), used by the
/// supervisor's ceiling check and its no-shard fallback.
fn refuse_connection(mut stream: TcpStream, status: u16, message: &str) {
    let bytes = render_status_response(status, message);
    let _ = stream.write_all(&bytes).and_then(|()| stream.flush());
    let _ = stream.shutdown(Shutdown::Both);
}

impl HttpSupervisorState {
    /// Build the disabled supervisor state (ADR-0155 §3). The cap is
    /// composed and claims `aether.http.server`, but binds no socket and
    /// spawns no accept thread, so the listener port is `0`, there is no
    /// accept thread, and the route / shard / monitor tables start empty.
    /// `init` returns this when the resolved config is disabled; the
    /// route-registration handlers then fail fast with an `Err` reply
    /// rather than the mail warn-dropping at an unknown mailbox.
    pub fn disabled(config: HttpServerConfig, mailer: Arc<Mailer>) -> Self {
        let (_inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
        Self {
            config,
            routes: Arc::new(RwLock::new(Vec::new())),
            live_connections: Arc::new(AtomicUsize::new(0)),
            mailer,
            listener_port: 0,
            accept_shutdown: Arc::new(AtomicBool::new(false)),
            accept_thread: None,
            inbound_rx,
            wake_dirty: Arc::new(AtomicBool::new(false)),
            self_mailbox: MailboxId::NONE,
            shard_startup: ShardStartup::Idle,
            next_stream_id: Arc::new(AtomicU64::new(0)),
            monitors: HashMap::new(),
            unmonitorable: HashSet::new(),
        }
    }

    fn configured_shard_count(&self) -> usize {
        if self.config.dispatch_shards == 0 {
            thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).max(1))
        } else {
            self.config.dispatch_shards
        }
    }

    fn pending_peer_count(&self) -> usize {
        match &self.shard_startup {
            ShardStartup::Starting { pending_peers, .. } => pending_peers.len(),
            ShardStartup::Idle | ShardStartup::Ready { .. } | ShardStartup::Failed => 0,
        }
    }

    fn schedule_shard_wake<A>(&self, ctx: &NativeCtx<'_, Single, A>) {
        let _ = ctx.send_envelope_tracked(
            self.self_mailbox,
            KindId(<HttpInboundReady as Kind>::ID.0),
            &HttpInboundReady::default().encode_into_bytes(),
        );
    }

    /// Stage at most one deterministic dispatch-shard child in this handler
    /// turn. Owner batches are transactional, so this turn boundary preserves
    /// the pre-migration partial-start contract: one apply conflict rejects
    /// one index rather than every sibling prepared beside it.
    ///
    /// Returns `true` when this wake was consumed as a startup step. The
    /// caller then ends the handler so its birth flushes as an independent
    /// batch. A follow-up wake is always scheduled: it stages the next index,
    /// or resumes ordinary accepted-peer draining after the last index.
    pub fn stage_next_shard(&mut self, ctx: &mut NativeCtx<'_, Single, HttpServerCapability>) -> bool {
        let index = match &mut self.shard_startup {
            ShardStartup::Starting { next_to_stage, .. } => next_to_stage.take(),
            ShardStartup::Idle | ShardStartup::Ready { .. } | ShardStartup::Failed => None,
        };
        let Some(index) = index else {
            return false;
        };

        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundEvent>();
        let wake_dirty = Arc::new(AtomicBool::new(false));
        let seed = HttpShardSeed {
            inbound_rx: Some(inbound_rx),
            inbound_tx: inbound_tx.clone(),
            wake_dirty: Arc::clone(&wake_dirty),
            routes: Arc::clone(&self.routes),
            live_connections: Arc::clone(&self.live_connections),
            max_request_bytes: self.config.max_request_bytes,
            max_header_bytes: self.config.max_header_bytes,
            request_timeout: Duration::from_millis(self.config.request_timeout_millis),
            keep_alive_timeout: Duration::from_millis(self.config.keep_alive_timeout_millis),
            ws_idle_timeout: Duration::from_millis(self.config.websocket_idle_timeout_millis),
            response_stream_window: self.config.response_stream_window,
            request_stream_window: self.config.request_stream_window,
            next_stream_id: Arc::clone(&self.next_stream_id),
        };
        let subname = format!("shard-{index}");
        let shard = ShardSpawnContext { index, subname: subname.clone(), inbound_tx, wake_dirty };
        if let Err(error) = ctx.spawn_child::<HttpDispatchShard>(Subname::Named(&subname), seed, ()).stage_with(shard) {
            tracing::warn!(
                target: "aether_http::server",
                shard = %subname,
                error = ?error,
                "http dispatch shard preparation failed",
            );
            let settlement = self.finish_shard_spawn(index, None);
            self.apply_shard_settlement(settlement);
        }

        if let ShardStartup::Starting { next_to_stage, slots_by_index, .. } = &mut self.shard_startup
            && index + 1 < slots_by_index.len()
        {
            *next_to_stage = Some(index + 1);
        }
        self.schedule_shard_wake(ctx);
        true
    }

    /// Record one synchronous or authoritative shard result. Completions may
    /// arrive out of index order; the final compaction always walks the slots
    /// in deterministic index order.
    pub fn finish_shard_spawn(&mut self, index: usize, sink: Option<WakeSink>) -> ShardSettlement {
        let finished = match &mut self.shard_startup {
            ShardStartup::Starting { remaining, slots_by_index, .. } => {
                let Some(slot) = slots_by_index.get_mut(index) else {
                    return ShardSettlement::Stale;
                };
                if !matches!(slot, ShardSlot::Pending) {
                    return ShardSettlement::Stale;
                }
                *slot = sink.map_or_else(|| ShardSlot::Failed, ShardSlot::Ready);
                *remaining -= 1;
                *remaining == 0
            }
            ShardStartup::Idle | ShardStartup::Ready { .. } | ShardStartup::Failed => {
                return ShardSettlement::Stale;
            }
        };
        if !finished {
            return ShardSettlement::Pending;
        }

        let ShardStartup::Starting { slots_by_index, pending_peers, .. } =
            mem::replace(&mut self.shard_startup, ShardStartup::Failed)
        else {
            unreachable!("the final shard result can only settle Starting")
        };
        let shards = slots_by_index
            .into_iter()
            .filter_map(|slot| match slot {
                ShardSlot::Ready(sink) => Some(sink),
                ShardSlot::Pending | ShardSlot::Failed => None,
            })
            .collect::<Vec<_>>();
        if shards.is_empty() {
            ShardSettlement::Failed { pending_peers }
        } else {
            let shard_count = shards.len();
            self.shard_startup = ShardStartup::Ready { shards, next_shard: 0 };
            ShardSettlement::Ready { pending_peers, shard_count }
        }
    }

    /// Apply a final startup transition after `finish_shard_spawn` has made
    /// the lifecycle authoritative. Ready drains retained sockets FIFO;
    /// Failed returns one controlled `503` per retained socket. Pending and
    /// stale attempts perform no side effect.
    pub fn apply_shard_settlement(&mut self, settlement: ShardSettlement) {
        match settlement {
            ShardSettlement::Pending => {}
            ShardSettlement::Ready { mut pending_peers, shard_count } => {
                tracing::info!(
                    target: "aether_http::server",
                    port = self.listener_port,
                    shards = shard_count,
                    "http dispatch shards activated",
                );
                while let Some(PendingPeer { stream, peer }) = pending_peers.pop_front() {
                    self.dispatch_ready_peer(stream, peer);
                }
            }
            ShardSettlement::Failed { mut pending_peers } => {
                tracing::warn!(
                    target: "aether_http::server",
                    port = self.listener_port,
                    "http dispatch shard startup failed",
                );
                while let Some(PendingPeer { stream, peer }) = pending_peers.pop_front() {
                    refuse_connection(stream, 503, "no dispatch shards");
                    tracing::warn!(
                        target: "aether_http::server",
                        %peer,
                        "http conn refused: no dispatch shards",
                    );
                }
            }
            ShardSettlement::Stale => {
                tracing::debug!(
                    target: "aether_http::server",
                    "stale http dispatch shard completion ignored",
                );
            }
        }
    }

    fn dispatch_ready_peer(&mut self, stream: TcpStream, peer: SocketAddr) {
        let ShardStartup::Ready { shards, next_shard } = &mut self.shard_startup else {
            refuse_connection(stream, 503, "no dispatch shards");
            return;
        };
        let index = *next_shard % shards.len();
        *next_shard = next_shard.wrapping_add(1);
        self.live_connections.fetch_add(1, Ordering::AcqRel);
        if !shards[index].post(InboundEvent::PeerAccepted { stream, peer }) {
            // The shard's receiver is gone — teardown is in progress; the
            // socket just dropped with the event.
            self.live_connections.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Adopt one accepted connection. Capacity counts both live shard-owned
    /// sockets and supervisor-owned pending sockets. The first peer starts
    /// lazy staging; later peers stay FIFO in `Starting`; only `Ready` may
    /// post into a child sink.
    pub fn assign_peer<A>(&mut self, ctx: &mut NativeCtx<'_, Single, A>, stream: TcpStream, peer: SocketAddr) {
        let live = self.live_connections.load(Ordering::Acquire);
        let pending = self.pending_peer_count();
        if live.saturating_add(pending) >= self.config.max_connections {
            refuse_connection(stream, 503, "server at connection capacity");
            tracing::warn!(
                target: "aether_http::server",
                %peer,
                live,
                pending,
                "http conn refused: at capacity",
            );
            return;
        }

        let mut start_count = None;
        match &mut self.shard_startup {
            ShardStartup::Idle => {
                let count = self.configured_shard_count();
                let mut pending_peers = VecDeque::with_capacity(self.config.max_connections.min(16));
                pending_peers.push_back(PendingPeer { stream, peer });
                self.shard_startup = ShardStartup::Starting {
                    remaining: count,
                    next_to_stage: Some(0),
                    slots_by_index: (0..count).map(|_| ShardSlot::Pending).collect(),
                    pending_peers,
                };
                start_count = Some(count);
            }
            ShardStartup::Starting { pending_peers, .. } => {
                pending_peers.push_back(PendingPeer { stream, peer });
            }
            ShardStartup::Ready { .. } => self.dispatch_ready_peer(stream, peer),
            ShardStartup::Failed => {
                refuse_connection(stream, 503, "no dispatch shards");
                tracing::warn!(
                    target: "aether_http::server",
                    %peer,
                    "http conn refused: no dispatch shards",
                );
            }
        }
        if let Some(count) = start_count {
            debug_assert!(count > 0, "configured shard count is always positive");
            self.schedule_shard_wake(ctx);
        }
    }

    /// Claim `(prefix, method)` for `mailbox`, dispatching as `kind`
    /// (ADR-0130), or join its shared member set (ADR-0136). Exclusive
    /// (`shared: false`): a key held by anyone else is answered `Err`;
    /// the same sole mailbox re-claiming its own key is an idempotent
    /// `Ok` that updates `kind` — so a component re-running `wire`
    /// after `replace_component` re-registers cleanly (its `MailboxId`
    /// is stable). Shared (`shared: true`): joins the key's member set
    /// when the set is shared and the `kind` matches; re-registering an
    /// existing membership is an idempotent `Ok`. Mixing exclusive and
    /// shared on one key, or joining with a different `kind`, is a
    /// conflict `Err` either way.
    ///
    /// # Panics
    /// Panics if the route-table `RwLock` is poisoned — fail-fast per
    /// ADR-0063 (a poisoned table means a supervisor or shard already
    /// panicked mid-read/write).
    pub fn register_route(
        &mut self,
        prefix: &str,
        method: Option<HttpMethod>,
        kind: KindId,
        mailbox: MailboxId,
        shared: bool,
    ) -> RegisterRouteResult {
        register_route(&self.routes, prefix, method, kind, mailbox, shared)
    }

    /// Monitor `mailbox` on its first route claim so the cap purges
    /// the mailbox's routes itself when the occupant departs — vacate
    /// or close, whichever comes first (ADR-0079 §8 amended).
    ///
    /// An `Err` (an actor outside the registry, or a spawner-less test
    /// binding) means "not monitorable", and the claim still stands: the
    /// route lives until substrate teardown. That is harmless for a mailbox
    /// that never goes away, and *not* harmless for a wasm trampoline, which
    /// vacates on `DropComponent` while staying addressable — an unmonitored
    /// route then keeps dispatching at an empty trampoline, which warn-drops
    /// every request, so the cap answers `502` to that prefix for the rest of
    /// the process. Nothing recovers it, because the notice that would have
    /// purged the route is the one that never arrives.
    ///
    /// So the failure is logged rather than discarded (issue 4195): the
    /// symptom it produces is indistinguishable from a lost `MonitorNotice`,
    /// and without this line neither branch leaves any trace to tell them
    /// apart.
    pub fn watch<M: aether_actor::ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, M>, mailbox: MailboxId) {
        if self.monitors.contains_key(&mailbox) || self.unmonitorable.contains(&mailbox) {
            return;
        }
        let Entry::Vacant(slot) = self.monitors.entry(mailbox) else {
            return;
        };
        match ctx.monitor(mailbox) {
            Ok(handle) => {
                slot.insert(handle);
            }
            Err(error) => {
                self.unmonitorable.insert(mailbox);
                tracing::warn!(
                    target: "aether_http::server",
                    %mailbox,
                    ?error,
                    "route holder is not monitorable; its routes cannot be purged when it departs",
                );
            }
        }
    }

    /// Release `mailbox`'s membership in the `(prefix, method)` route
    /// (ADR-0136); the last member's release drops the route.
    /// Idempotent — releasing a route that isn't held (or a set the
    /// mailbox never joined) is still `Ok`, mirroring the input cap's
    /// unsubscribe semantics.
    ///
    /// # Panics
    /// Panics if the route-table `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn unregister_route(
        &mut self,
        prefix: &str,
        method: Option<HttpMethod>,
        mailbox: MailboxId,
    ) -> RegisterRouteResult {
        unregister_route(&self.routes, prefix, method, mailbox)
    }

    /// Release every route membership held by `mailbox` (ADR-0130's
    /// `UnregisterRoutesAll`, ADR-0136 set semantics); sets it empties
    /// drop entirely.
    ///
    /// # Panics
    /// Panics if the route-table `RwLock` is poisoned — fail-fast per
    /// ADR-0063.
    pub fn unregister_routes_all(&mut self, mailbox: MailboxId) {
        unregister_routes_all(&self.routes, mailbox);
    }
}

impl HttpShardState {
    pub fn wake_sink(&self) -> WakeSink {
        WakeSink {
            inbound_tx: self.inbound_tx.clone(),
            mailer: Arc::clone(&self.mailer),
            self_id: self.self_mailbox,
            wake_kind: KindId(<HttpInboundReady as Kind>::ID.0),
            dirty: Arc::clone(&self.wake_dirty),
        }
    }

    pub fn subscribe_settlement(&self, mail_id: MailId) {
        if let Some(registry) = self.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                mail_id,
                self.self_mailbox,
                <Settled as Kind>::ID,
                Arc::clone(&self.mailer),
            );
        }
    }

    /// Release this connection's slot in the global live count (ADR-0135).
    /// Paired with the supervisor's assignment-time increment; called
    /// exactly once per assigned connection — on close, or on an adoption
    /// failure before any `ConnState` exists.
    fn release_connection_slot(&self) {
        self.live_connections.fetch_sub(1, Ordering::AcqRel);
    }

    /// Allocate a fresh `ConnId`, store the connection's write half, and
    /// spin a reader thread for the read half. The global `max_connections`
    /// ceiling was already enforced at assignment by the supervisor
    /// (ADR-0135); an adoption failure here releases the slot it charged.
    pub fn spawn_reader_for_peer(&mut self, stream: TcpStream, peer: SocketAddr) {
        let conn_id = self.next_conn_id;
        self.next_conn_id += 1;

        let read_half = match stream.try_clone() {
            Ok(half) => half,
            Err(e) => {
                tracing::warn!(
                    target: "aether_http::server",
                    %peer,
                    error = %e,
                    "http conn: try_clone failed; dropping",
                );
                self.release_connection_slot();
                return;
            }
        };
        // Slow-loris guard + response deadline (ADR-0108 §6): bound
        // every blocking read on this socket.
        if let Err(e) = read_half.set_read_timeout(Some(self.request_timeout)) {
            tracing::warn!(
                target: "aether_http::server",
                %peer,
                error = %e,
                "http conn: set_read_timeout failed; dropping",
            );
            self.release_connection_slot();
            return;
        }
        let write_half = stream;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);
        // Per-connection reader control channel (ADR-0128 + keep-alive): the
        // dispatcher's half is stored in `ConnState`, the reader's half moves
        // into the thread.
        let (control_tx, control_rx) = mpsc::channel::<ReaderControl>();

        let sink = self.wake_sink();
        let tuning = ReaderTuning {
            request_timeout: self.request_timeout,
            idle_timeout: self.keep_alive_timeout,
            max_header_bytes: self.max_header_bytes,
            max_request_bytes: self.max_request_bytes,
            ws_idle_timeout: self.ws_idle_timeout,
            ws_max_message_bytes: self.max_request_bytes,
        };
        let shared = ReaderShared { routes: Arc::clone(&self.routes), peer: peer.to_string() };

        // Per-connection transport reader below the mail layer — carries
        // inbound mail in; no inbound chain to inherit, no settlement
        // umbrella.
        #[allow(clippy::disallowed_methods)]
        let thread = match thread::Builder::new().name(format!("aether-http-reader-{conn_id}")).spawn(move || {
            run_reader_loop(read_half, conn_id, &shutdown_for_thread, &sink, &control_rx, tuning, &shared);
        }) {
            Ok(thread) => thread,
            Err(e) => {
                tracing::warn!(
                    target: "aether_http::server",
                    %peer,
                    error = %e,
                    "http reader thread spawn failed",
                );
                self.release_connection_slot();
                return;
            }
        };

        self.connections.insert(
            conn_id,
            ConnState {
                peer,
                write_half,
                shutdown,
                control_tx,
                active_stream: None,
                reader_thread: Some(thread),
                ws_pending_key: None,
                websocket: None,
            },
        );
        tracing::debug!(
            target: "aether_http::server",
            conn = conn_id,
            %peer,
            "http conn accepted",
        );
    }

    /// Dispatch a reader-prepared buffered request (ADR-0135 §2): the
    /// reader resolved the handler, validated it live, and encoded the
    /// payload; this side stashes a websocket handshake key when one
    /// rode along (ADR-0129), sends, subscribes settlement, and records
    /// the in-flight entry. A handler that died since the reader's
    /// check is caught by the settlement `502` net — the same net that
    /// covers the dispatch-to-delivery gap.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_prepared(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        payload: &[u8],
        handler: MailboxId,
        kind: KindId,
        method: HttpMethod,
        keep_alive: bool,
        ws_key: Option<String>,
    ) {
        if ws_key.is_some()
            && let Some(conn) = self.connections.get_mut(&conn_id)
        {
            conn.ws_pending_key = ws_key;
        }
        let mail_id = ctx.send_envelope_detached(handler, kind, payload);
        // Safety net (ADR-0108 §5): if the chain settles with no
        // response, `on_settled` answers `502`. Best-effort — a chassis
        // without the settlement registry still serves the reply path.
        self.subscribe_settlement(mail_id);
        self.in_flight.insert(mail_id.correlation_id, PendingRequest { conn_id, method, keep_alive, handler });
    }

    /// Open the inbound request stream for a reader-posted head bound
    /// for a streaming handler (ADR-0128 / ADR-0135 §2). The reader
    /// resolved `handler` and made every reject decision; the shard
    /// seats the session — minting the stream id and seeding credit —
    /// because the stream tables live here. The method re-parses from
    /// the head's raw string; the reader already rejected
    /// non-enumerated verbs, so the defensive arm only fires on a
    /// torn-down race.
    pub fn open_requested_stream(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        conn_id: ConnId,
        head: ParsedHead,
        handler: MailboxId,
    ) {
        let Some(method) = parse_http_method(&head.method) else {
            self.write_status_response(conn_id, 501, "method not implemented");
            self.close_connection(conn_id, "unsupported method");
            return;
        };
        self.start_request_stream(ctx, conn_id, handler, method, head);
    }

    /// Send a control message to a connection's parked reader; a send failure
    /// means the reader already exited, so the connection is closed.
    pub fn signal_reader(&mut self, conn_id: ConnId, control: ReaderControl) {
        let sent = self.connections.get(&conn_id).is_some_and(|conn| conn.control_tx.send(control).is_ok());
        if !sent {
            self.close_connection(conn_id, "reader gone");
        }
    }

    /// Hand a fully rendered response to the connection's parked reader
    /// (ADR-0135 §3): the reader performs the socket write on its own
    /// clone and either loops into the next request (`resume`) or exits
    /// into the normal `ReaderClosed` teardown. The shard's dispatch
    /// never blocks on a peer's receive window — a stalled peer stalls
    /// only its own reader thread. A send failure means the reader
    /// already exited, so the connection closes instead.
    pub fn respond_and_finish(&mut self, conn_id: ConnId, bytes: Vec<u8>, resume: bool) {
        self.signal_reader(conn_id, ReaderControl::Respond { bytes, resume });
    }

    /// Release the reader for the next request on a kept-alive connection by
    /// signalling its resume channel. A send failure means the reader
    /// already exited (its own read error / EOF), so the connection is
    /// closed instead.
    pub fn resume_connection(&mut self, conn_id: ConnId) {
        self.signal_reader(conn_id, ReaderControl::Resume);
    }

    /// Format + write a canned status response (the cap's own
    /// `413` / `431` / `501` / `502` / `503` / `504`).
    pub fn write_status_response(&mut self, conn_id: ConnId, status: u16, message: &str) {
        let bytes = render_status_response(status, message);
        self.write_raw_to(conn_id, &bytes);
    }

    pub fn write_raw_to(&mut self, conn_id: ConnId, bytes: &[u8]) {
        let Some(conn) = self.connections.get_mut(&conn_id) else {
            return;
        };
        if let Err(e) = conn.write_half.write_all(bytes).and_then(|()| conn.write_half.flush()) {
            tracing::debug!(
                target: "aether_http::server",
                conn = conn_id,
                error = %e,
                "http response write failed",
            );
        }
    }

    pub fn close_connection(&mut self, conn_id: ConnId, reason: &str) {
        let Some(mut conn) = self.connections.remove(&conn_id) else {
            return;
        };
        self.release_connection_slot();
        conn.shutdown.store(true, Ordering::Release);
        let _ = conn.write_half.shutdown(Shutdown::Both);
        // Detach the reader without joining inline — the dispatcher must
        // not block on it. The thread sees the shutdown (or its own EOF)
        // and exits; the JoinHandle drop detaches.
        drop(conn.reader_thread.take());
        // Tear down any response stream bound to this connection (ADR-0128).
        // The socket shutdown above unblocks a write-blocked writer; dropping
        // the sender (in `teardown_stream`) unblocks a recv-blocked one.
        let stream_ids: Vec<u64> =
            self.streams.iter().filter(|(_, stream)| stream.conn_id == conn_id).map(|(id, _)| *id).collect();
        for stream_id in stream_ids {
            self.teardown_stream(stream_id);
        }
        // Drop any inbound request stream bound to this connection (ADR-0128);
        // the reader (parked on the control channel or blocked mid-read) is
        // already unblocked by the dropped `ConnState` sender / socket
        // shutdown above.
        self.request_streams.retain(|_, stream| stream.conn_id != conn_id);
        // Drop any in-flight entry pinned to this connection so we don't
        // write to a dead socket.
        self.in_flight.retain(|_, pending| pending.conn_id != conn_id);
        tracing::debug!(
            target: "aether_http::server",
            conn = conn_id,
            peer = %conn.peer,
            reason,
            "http conn closed",
        );
    }
}
