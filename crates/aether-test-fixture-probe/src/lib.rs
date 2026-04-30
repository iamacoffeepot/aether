//! Test-fixture component for substrate-feature scenarios. Not a
//! demo, not exemplary — its only job is to expose substrate /
//! test-bench primitives (input subscription, drop, replace, capture)
//! to scenario assertions in a way that's easy to observe.
//!
//! Behaviour:
//!
//! - On every tick, broadcasts `aether.test_fixture.tick_observed`
//!   with a monotonic counter. Lets scenarios count tick deliveries
//!   via `TestBench::count_observed` (broadcasts arrive on the
//!   loopback and get recorded by kind name).
//!
//! Future surface (deferred until the relevant scenarios land):
//! state-set + render-on-command for capture_frame round-trip,
//! mail-echo for replace_component, IO-reply forwarding.

use aether_component::{Component, Ctx, InitCtx, Sink, handlers, resolve_sink};
use aether_kinds::Tick;

/// Broadcast payload emitted on each tick. Postcard-shaped — schema
/// rides in the wasm's `aether.kinds` custom section, so the bench's
/// loopback decoder can record the kind name without the test
/// pre-registering anything.
#[derive(
    aether_mail::Kind, aether_mail::Schema, serde::Serialize, serde::Deserialize, Debug, Clone,
)]
#[kind(name = "aether.test_fixture.tick_observed")]
pub struct TickObserved {
    pub count: u64,
}

const BROADCAST: Sink<TickObserved> = resolve_sink::<TickObserved>("hub.claude.broadcast");

pub struct Probe {
    tick_count: u64,
}

#[handlers]
impl Component for Probe {
    fn init(_ctx: &mut InitCtx<'_>) -> Self {
        Probe { tick_count: 0 }
    }

    /// Counts ticks delivered to this mailbox; broadcasts the running
    /// total so scenarios can observe it on the loopback.
    ///
    /// # Agent
    /// Not sent manually; the substrate's tick fanout fires it once
    /// per advance for every input-subscribed mailbox. Watch
    /// `receive_mail` for `aether.test_fixture.tick_observed` to see
    /// the count climbing.
    #[handler]
    fn on_tick(&mut self, _ctx: &mut Ctx<'_>, _: Tick) {
        self.tick_count += 1;
        BROADCAST.send(&TickObserved {
            count: self.tick_count,
        });
    }
}

aether_component::export!(Probe);
