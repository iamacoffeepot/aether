//! The relay — the sweep's synthetic forwarding actor — with the bounded CPU
//! spin a heavy relay burns per inbound `Ping`, the reverse-order spawn that
//! hands each relay its downstreams' proofs.

use std::hint::black_box;
use std::sync::Arc;

use aether_actor::{ActorRef, OutboundReply};
use aether_data::{Kind, KindId, ReplyContract};
use aether_kinds::{ComponentCapabilities, HandlerCapability};
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx, SpawnError, Subname};

use super::{CountQuery, CountReport, Ping, Topology};
use crate::SubstrateHarness;

/// Bounded, deterministic CPU spin: an FNV-1a-style integer mix run
/// `iters` times. Real compute that occupies the worker thread for the
/// duration — deliberately **not** `thread::sleep`, which would free the
/// core and turn the measurement into park/wake latency instead of
/// compute contention (iamacoffeepot/aether#1074). `black_box` on both
/// the loop input and the accumulator stops the optimizer eliding the
/// loop or folding it to a constant. `iters == 0` is a true no-op, so
/// the trivial topologies stay byte-for-byte unchanged.
#[inline(never)]
fn busy_spin(iters: u64) {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64-bit offset basis
    for i in 0..iters {
        acc ^= black_box(i);
        acc = acc.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a 64-bit prime
    }
    black_box(acc);
}

/// Spawn config for a [`Relay`]: the proofs of the relays it forwards to
/// (already spawned — see [`spawn_relays`]), and how much CPU
/// work to burn per inbound `Ping` before forwarding. `work_iters == 0`
/// is the trivial relay; a non-zero count makes a leaf contend for a
/// core (the parallel-heavy regime, iamacoffeepot/aether#1074).
pub struct RelayConfig {
    pub downstreams: Arc<[ActorRef<Relay>]>,
    pub work_iters: u64,
}

/// A relay forwards each inbound `Ping` to every configured downstream
/// relay, inheriting the trace lineage so the whole topology is one
/// causal tree. A leaf relay (empty `downstreams`) just receives and
/// returns. Before forwarding it burns `work_iters` of `busy_spin`
/// CPU — zero by default, so trivial topologies are unchanged. Pooled
/// (the `Addressable` default).
pub struct Relay {
    downstreams: Arc<[ActorRef<Self>]>,
    work_iters: u64,
    /// `Ping` mails handled, for the run-end keep-up harvest
    /// (iamacoffeepot/aether#1233). A plain field — the actor is
    /// single-threaded over its own state, so no atomics.
    received: u64,
    /// `Ping` mails forwarded downstream, for the same harvest.
    sent: u64,
}

impl aether_actor::Addressable for Relay {
    const NAMESPACE: &'static str = "mlat.relay";
    type Resolver = aether_actor::Many;
}
impl aether_actor::Root for Relay {}
impl aether_actor::HandlesKind<Ping> for Relay {}
impl aether_actor::Lifecycle<Self> for Relay {
    type Config = RelayConfig;
    type Params = ();
    type InitError = BootError;
    type InitCtx<'a> = NativeInitCtx<'a>;
    type Ctx<'a> = NativeCtx<'a, Self>;
    fn init(config: Self::Config, _params: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { downstreams: config.downstreams, work_iters: config.work_iters, received: 0, sent: 0 })
    }
}
impl NativeActor for Relay {
    type State = Self;
}
impl Dispatch<Self> for Relay {
    /// Declare the kinds this relay handles (iamacoffeepot/aether#4236).
    ///
    /// `Dispatch::capabilities` defaults to an empty surface, and a hand-written
    /// `Dispatch` impl — unlike an `#[actor]` one — gets no generated override.
    /// The spawn path seeds the cost table from exactly this list, so leaving it
    /// empty means the relay owns no cost cells: `fold_handler_cost` finds no
    /// cell to fold into, the producer's `group_mail_cost` lookup misses, and the
    /// #1178 cost-aware recruiter reports `cost_confident = false` on every
    /// flush. The harness would then measure a dispatch path with cost-awareness
    /// permanently disabled, which is not what a production `#[actor]` cap does.
    fn capabilities() -> ComponentCapabilities {
        ComponentCapabilities {
            handlers: vec![
                HandlerCapability {
                    id: Ping::ID,
                    name: <Ping as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::None,
                },
                HandlerCapability {
                    id: CountQuery::ID,
                    name: <CountQuery as Kind>::NAME.to_owned(),
                    doc: None,
                    reply: ReplyContract::One(CountReport::ID),
                },
            ],
            ..ComponentCapabilities::default()
        }
    }

    fn dispatch(
        state: &mut Self,
        ctx: &mut NativeCtx<'_, Self, aether_substrate::Manual>,
        kind: KindId,
        payload: &[u8],
    ) -> Option<()> {
        // Run-end keep-up harvest (iamacoffeepot/aether#1233): answer the
        // out-of-band counter query before the `Ping` fast path.
        if kind.0 == CountQuery::ID.0 {
            ctx.reply(&CountReport { sent: state.sent, received: state.received });
            return Some(());
        }
        if kind.0 != Ping::ID.0 {
            return None;
        }
        state.received += 1;
        // Burn the configured CPU budget on this worker thread before
        // forwarding. With heavy leaves and idle cores this is what makes
        // scattering children across workers pay off — the contention the
        // trivial harness can't exhibit (iamacoffeepot/aether#1074).
        busy_spin(state.work_iters);
        // Forward the bytes verbatim to each downstream. Each push
        // stamps its own `t_sent`, so later children in a fan-out reveal
        // any per-child enqueue skew.
        for down in state.downstreams.iter() {
            let _ = ctx.send_envelope_tracked_to(down.erase(), Ping::ID, payload);
            state.sent += 1;
        }
        Some(())
    }
}

pub(super) const RELAY_NS: &str = "mlat.relay";

/// Spawn every relay in `topo` onto `tb` (subname = relay index) and return
/// every relay's proof in index order — relay 0's is the entry a tick source
/// or an injected root feeds, and each one is where a harvest queries that
/// relay's ring or counters.
///
/// Relays spawn from index `n - 1` down to `0`, so every downstream a relay
/// forwards to is already spawned and its `finish()` proof is in hand when
/// the relay's config is built. That holds because every [`Topology`] edge
/// points to a higher index; a factory that breaks the invariant panics here,
/// naming the relay and the edge. A spawn failure returns the failing relay's
/// index with its error, leaving the relays spawned before it live.
///
/// Callers: the sweep cell (`run_cell`), the mail-latency settlement guards,
/// and the cost-cell tripwire below.
///
/// # Errors
///
/// `(index, error)` for the first relay whose spawn fails.
///
/// # Panics
///
/// If `topo` has no relays, or an edge in `topo.downstreams[i]` names an
/// index that is not greater than `i` (or is out of range).
pub fn spawn_relays(tb: &SubstrateHarness, topo: &Topology) -> Result<Vec<ActorRef<Relay>>, (usize, SpawnError)> {
    let mut spawned: Vec<Option<ActorRef<Relay>>> = vec![None; topo.downstreams.len()];
    for i in (0..topo.downstreams.len()).rev() {
        let downstreams = topo.downstreams[i]
            .iter()
            .map(|&j| {
                spawned.get(j).copied().flatten().unwrap_or_else(|| {
                    panic!(
                        "topology {}: relay {i} forwards to relay {j}, which is not spawned before it — every edge \
                         must point to a higher index",
                        topo.name
                    )
                })
            })
            .collect();
        let config = RelayConfig { downstreams, work_iters: topo.work_iters[i] };
        let relay = tb.spawn_actor::<Relay>(Subname::Named(&i.to_string()), config, ()).finish().map_err(|e| (i, e))?;
        spawned[i] = Some(relay);
    }

    assert!(!spawned.is_empty(), "topology {} has no relays — a topology has at least its entry relay", topo.name);
    Ok(spawned.into_iter().map(|relay| relay.expect("every relay index spawned above")).collect())
}

/// Tripwire for iamacoffeepot/aether#4236: the sweep's relays must own live
/// cost cells after a real run.
///
/// A hand-written `Dispatch` impl inherits an empty `capabilities()`, and the
/// spawn path seeds the cost table from exactly that list — so forgetting to
/// declare a handler leaves the actor with no cost cells at all. Nothing fails
/// when that happens: `fold_handler_cost` finds no cell and silently skips, the
/// producer's cost lookup misses, and the #1178 cost-aware recruiter reports
/// `cost_confident = false` on every flush and falls back to the width gate
/// forever. The sweep still produces numbers — measured against a dispatch path
/// with cost-awareness disabled, which is not the path a production `#[actor]`
/// cap takes.
///
/// The pinned value is read back from a live 200-frame run rather than restated
/// from the declaration, so it moves when the wiring moves.
#[cfg(test)]
mod cost_cell_liveness {
    use aether_kinds::{CostTail, CostTailResult};

    use super::*;
    use crate::perf::harness::{TickSource, fanout};
    use crate::{DEFAULT_TICK_DELTA_MICROS, SubstrateHarness};
    use aether_lifecycle::LifecycleCapability;

    #[test]
    fn sweep_relays_own_live_cost_cells_after_a_run() {
        let topo = fanout(4);

        let Ok(mut tb) = SubstrateHarness::builder().with_workers(Some(2)).size(16, 16).build() else {
            // Driverless box: the sweep itself skips the same way.
            return;
        };

        let relays = spawn_relays(&tb, &topo).expect("relays spawn");
        let lifecycle = tb.actor_ref::<LifecycleCapability>();
        tb.spawn_actor::<TickSource>(Subname::Named("src"), (relays[0], 1, lifecycle), ())
            .finish()
            .expect("source spawns");

        let _ = tb.advance(200, DEFAULT_TICK_DELTA_MICROS);

        for (i, relay) in relays.iter().enumerate() {
            let name = format!("mlat.relay:{i}");
            let request = CostTail { kind: Some(Ping::ID) }.encode_into_bytes();
            let bytes = tb.request_bytes(relay.erase(), CostTail::ID, request).expect("the relay answers cost.tail");
            let Some(CostTailResult::Ok { rows }) = CostTailResult::decode_from_bytes(&bytes) else {
                panic!("{name}: cost.tail did not answer Ok");
            };
            let row = rows.first().unwrap_or_else(|| {
                panic!(
                    "{name} owns no cost cell for Ping — its `Dispatch::capabilities` no longer declares the \
                     handler, so the sweep is measuring a path with cost-aware recruitment disabled"
                )
            });
            assert!(
                row.samples > 0,
                "{name} has a Ping cost cell but folded no samples over 200 frames; the dispatch path stopped \
                 reaching `fold_handler_cost`",
            );
        }
    }
}
