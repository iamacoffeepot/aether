//! The relay — the sweep's synthetic forwarding actor — with the bounded CPU
//! spin a heavy relay burns per inbound `Ping` and the deterministic id the
//! topology is wired from before any actor spawns.

use std::hint::black_box;
use std::sync::Arc;

use aether_actor::OutboundReply;
use aether_data::{Kind, KindId, MailboxId, ReplyContract, mailbox_id_from_name};
use aether_kinds::{ComponentCapabilities, HandlerCapability};
use aether_substrate::{BootError, Dispatch, NativeActor, NativeCtx, NativeInitCtx};

use super::{CountQuery, CountReport, Ping};

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

/// Spawn config for a [`Relay`]: who to forward to, and how much CPU
/// work to burn per inbound `Ping` before forwarding. `work_iters == 0`
/// is the trivial relay; a non-zero count makes a leaf contend for a
/// core (the parallel-heavy regime, iamacoffeepot/aether#1074).
pub struct RelayConfig {
    pub downstreams: Arc<[MailboxId]>,
    pub work_iters: u64,
}

/// A relay forwards each inbound `Ping` to every configured downstream
/// mailbox, inheriting the trace lineage so the whole topology is one
/// causal tree. A leaf relay (empty `downstreams`) just receives and
/// returns. Before forwarding it burns `work_iters` of `busy_spin`
/// CPU — zero by default, so trivial topologies are unchanged. Pooled
/// (the `Addressable` default).
pub struct Relay {
    downstreams: Arc<[MailboxId]>,
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
    type Ctx<'a> = NativeCtx<'a>;
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
        ctx: &mut NativeCtx<'_, aether_substrate::Manual, Self>,
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
        for &down in state.downstreams.iter() {
            let _ = ctx.send_envelope_tracked(down, Ping::ID, payload);
            state.sent += 1;
        }
        Some(())
    }
}

pub(super) const RELAY_NS: &str = "mlat.relay";

/// Deterministic `MailboxId` for relay instance `i`. Mirrors the
/// substrate's `mailbox_id_from_name("{NAMESPACE}:{subname}")` so the
/// whole topology can be wired from precomputed ids before any actor is
/// spawned (sidesteps spawn-ordering between a relay and its
/// downstreams).
// Harness wires its synthetic relay topology from precomputed name-hashed ids
// before any actor spawns — id derivation, not sibling-cap addressing.
#[must_use]
#[allow(clippy::disallowed_methods)]
pub fn relay_id(i: usize) -> MailboxId {
    MailboxId(mailbox_id_from_name(&format!("{RELAY_NS}:{i}")).0)
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
    use aether_kinds::{CostTail, CostTailResult, LifecycleSubscribe, LifecycleSubscribeResult, Tick};
    use aether_substrate::Subname;

    use super::*;
    use crate::SubstrateHarness;
    use crate::perf::harness::{TickSource, fanout, ticksrc_id};

    #[test]
    fn sweep_relays_own_live_cost_cells_after_a_run() {
        let topo = fanout(4);

        let Ok(mut tb) = SubstrateHarness::builder().with_workers(Some(2)).size(16, 16).build() else {
            // Driverless box: the sweep itself skips the same way.
            return;
        };

        for i in 0..topo.downstreams.len() {
            let downstreams: Arc<[MailboxId]> = topo.downstreams[i].iter().map(|&j| relay_id(j)).collect();
            let config = RelayConfig { downstreams, work_iters: topo.work_iters[i] };
            tb.spawn_actor::<Relay>(Subname::Named(&i.to_string()), config, ()).finish().expect("relay spawns");
        }
        tb.spawn_actor::<TickSource>(Subname::Named("src"), (relay_id(0), 1), ()).finish().expect("source spawns");

        let sub_req = LifecycleSubscribe { stage: Tick::ID.0, mailbox: ticksrc_id().0 }.encode_into_bytes();
        let reply =
            tb.send_bytes_and_await("aether.lifecycle", LifecycleSubscribe::ID, sub_req).expect("subscribe sends");
        assert!(matches!(LifecycleSubscribeResult::decode_from_bytes(&reply), Some(LifecycleSubscribeResult::Ok)));

        let _ = tb.advance(200);

        for i in 0..topo.downstreams.len() {
            let name = format!("mlat.relay:{i}");
            let request = CostTail { kind: Some(Ping::ID) }.encode_into_bytes();
            let bytes = tb.send_bytes_and_await(&name, CostTail::ID, request).expect("the relay answers cost.tail");
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
